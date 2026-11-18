//! Managed installs for npm-distributed ACP adapters.
//!
//! Running `npx -y <pkg>` at chat time would put every user's npm state in the
//! hot path: a cold cache means a multi-minute download while the chat shows
//! "Working", and a broken one means npm dying before the adapter ever runs,
//! silently, with an errno-encoded exit code (254 = ENOENT) that surfaces as
//! an opaque "harness protocol error". Instead, pinned adapter packages are
//! installed ONCE into a Reno-owned prefix (`adapters/<pkg>/<version>` under
//! `~/Library/Application Support/Reno` on macOS, the XDG data directory on
//! Linux), with its own npm cache beside it, so a root-owned or read-only user
//! cache cannot break us. Every subsequent launch spawns `node <entry>`
//! directly, with no npm anywhere near a chat turn.
//!
//! Install is atomic: npm runs in a `.tmp-*` sibling which is renamed into
//! place only after the bin entry resolves and a marker file is written, so a
//! killed install can never masquerade as a working adapter, and concurrent
//! installers (two daemons, prewarm racing a run) converge on whichever
//! rename won.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::HarnessError;
use crate::process::Stdio;

/// A pinned npm package: `"@scope/name@1.2.3"` → name + version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NpmPin {
    pub name: &'static str,
    pub version: &'static str,
}

impl NpmPin {
    /// Split a `name@version` pin at the LAST `@` (scoped names carry a
    /// leading one).
    pub(crate) fn parse(pin: &'static str) -> Self {
        match pin.rfind('@') {
            Some(at) if at > 0 => Self {
                name: &pin[..at],
                version: &pin[at + 1..],
            },
            _ => Self {
                name: pin,
                version: "latest",
            },
        }
    }

    fn spec(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

    /// Filesystem-safe directory name (`@scope/name` → `scope__name`).
    fn dir_name(&self) -> String {
        self.name.trim_start_matches('@').replace('/', "__")
    }
}

pub(crate) const OK_MARKER: &str = ".reno-install-ok";
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

/// Managed adapter storage. `$RENO_ADAPTERS_DIR` wins, followed by
/// `$RENO_DATA_DIR/adapters`. Otherwise `~/Library/Application Support/Reno/adapters`
/// on macOS and `$XDG_DATA_HOME/reno/adapters` (default `~/.local/share/reno/adapters`)
/// elsewhere.
pub(crate) fn adapters_root() -> Option<PathBuf> {
    adapters_root_with(&|key| std::env::var_os(key), cfg!(target_os = "macos"))
}

fn adapters_root_with(env: &impl Fn(&str) -> Option<OsString>, macos: bool) -> Option<PathBuf> {
    let value = |key| {
        env(key)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    if let Some(dir) = value("RENO_ADAPTERS_DIR") {
        return Some(dir);
    }
    if let Some(dir) = value("RENO_DATA_DIR") {
        return Some(dir.join("adapters"));
    }
    if macos {
        value("HOME").map(|home| {
            home.join("Library")
                .join("Application Support")
                .join("Reno")
                .join("adapters")
        })
    } else {
        value("XDG_DATA_HOME")
            .filter(|dir| dir.is_absolute())
            .or_else(|| value("HOME").map(|home| home.join(".local").join("share")))
            .map(|data| data.join("reno").join("adapters"))
    }
}

fn install_dir_in(root: &Path, pin: &NpmPin) -> PathBuf {
    root.join(pin.dir_name()).join(pin.version)
}

/// The package's bin entry inside an install dir, from its own package.json
/// (`bin` as a string, or a map preferring `bin_name`).
pub(crate) fn bin_entry(dir: &Path, pin: &NpmPin, bin_name: &str) -> Option<PathBuf> {
    let pkg_dir = dir.join("node_modules").join(pin.name);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(pkg_dir.join("package.json")).ok()?).ok()?;
    let rel = match manifest.get("bin")? {
        serde_json::Value::String(entry) => entry.clone(),
        serde_json::Value::Object(map) => map
            .get(bin_name)
            .or_else(|| map.values().next())
            .and_then(|v| v.as_str())
            .map(str::to_owned)?,
        _ => return None,
    };
    let entry = pkg_dir.join(rel);
    entry.is_file().then_some(entry)
}

/// The bin entry of a COMPLETED managed install, `None` when absent.
pub(crate) fn installed_entry(pin: &NpmPin, bin_name: &str) -> Option<PathBuf> {
    let root = adapters_root()?;
    installed_entry_in(&root, pin, bin_name)
}

fn installed_entry_in(root: &Path, pin: &NpmPin, bin_name: &str) -> Option<PathBuf> {
    let dir = install_dir_in(root, pin);
    if !dir.join(OK_MARKER).is_file() {
        return None;
    }
    bin_entry(&dir, pin, bin_name)
}

pub(crate) fn find_npm() -> Option<PathBuf> {
    find_npm_with(&|key| std::env::var_os(key), crate::shell_env::user_path())
}

fn find_npm_with(
    env: &impl Fn(&str) -> Option<OsString>,
    login_shell_path: Option<OsString>,
) -> Option<PathBuf> {
    crate::executable::find_on_paths_with("npm", Vec::new(), env, login_shell_path)
}

fn node_sibling_for_npm(npm: &Path) -> Option<PathBuf> {
    npm.parent()
        .map(|dir| dir.join("node"))
        .filter(|node| node.is_file())
}

fn is_native_executable(entry: &Path) -> Result<bool, HarnessError> {
    let mut file = std::fs::File::open(entry)?;
    let mut head = [0_u8; 4];
    let count = file.read(&mut head)?;
    let head = &head[..count];
    Ok(head.starts_with(b"\x7fELF")
        || [
            [0xfe, 0xed, 0xfa, 0xce],
            [0xce, 0xfa, 0xed, 0xfe],
            [0xfe, 0xed, 0xfa, 0xcf],
            [0xcf, 0xfa, 0xed, 0xfe],
            [0xca, 0xfe, 0xba, 0xbe],
            [0xbe, 0xba, 0xfe, 0xca],
        ]
        .iter()
        .any(|magic| head.starts_with(magic)))
}

/// How to spawn an installed entry: JavaScript runs via `node`; a native ELF
/// or Mach-O bin entry runs directly.
pub(crate) fn launch_for_entry(entry: &Path) -> Result<(PathBuf, Vec<String>), HarnessError> {
    launch_for_entry_with_node(entry, None)
}

fn launch_for_entry_with_node(
    entry: &Path,
    supplied_node: Option<PathBuf>,
) -> Result<(PathBuf, Vec<String>), HarnessError> {
    if is_native_executable(entry)? {
        return Ok((entry.to_path_buf(), Vec::new()));
    }

    let sibling = find_npm().and_then(|npm| node_sibling_for_npm(&npm));
    let node = supplied_node
        .or(sibling)
        .or_else(|| crate::executable::find_on_paths("node", Vec::new()))
        .ok_or_else(|| {
            HarnessError::NotInstalled(
                "node (required to run the agent's npm-distributed ACP adapter; \
                 searched PATH, the login shell's PATH, and fnm/nvm/volta/pnpm/bun \
                 install dirs)"
                    .into(),
            )
        })?;
    Ok((node, vec![entry.display().to_string()]))
}

/// npm encodes fatal fs errors as `256 - errno`, often with no stderr at all;
/// name the ones users actually hit.
fn describe_npm_exit(status: Option<std::process::ExitStatus>) -> String {
    let base = crate::describe_exit(status);
    let hint = match status.and_then(|s| s.code()) {
        Some(254) => Some("ENOENT — a file or directory npm needed is missing"),
        Some(243) => Some("EACCES — permission denied, often a root-owned or unwritable npm dir"),
        Some(226) => Some("EROFS — read-only filesystem"),
        _ => None,
    };
    match hint {
        Some(hint) => format!("{base}, {hint}"),
        None => base,
    }
}

/// One installer at a time per process; installs are rare and npm handles
/// its own intra-install parallelism.
pub(crate) fn install_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Ensure the pinned package is installed; returns its bin entry. Failures
/// carry npm's own output — the whole point is that a dying npm stops being
/// an undiagnosable one-liner.
pub(crate) async fn ensure_installed(
    pin: NpmPin,
    bin_name: &str,
    display_name: &str,
) -> Result<PathBuf, HarnessError> {
    if let Some(entry) = installed_entry(&pin, bin_name) {
        return Ok(entry);
    }
    let _guard = install_lock().lock().await;
    if let Some(entry) = installed_entry(&pin, bin_name) {
        return Ok(entry);
    }

    let Some(npm) = find_npm() else {
        return Err(HarnessError::NotInstalled(format!(
            "npm (required to install the {display_name} ACP adapter {}; searched \
             PATH, the login shell's PATH, and fnm/nvm/volta/pnpm/bun install dirs)",
            pin.spec()
        )));
    };
    let root = adapters_root().ok_or_else(|| {
        HarnessError::Install("cannot locate a platform data directory for managed adapters".into())
    })?;
    let final_dir = install_dir_in(&root, &pin);
    let tmp_dir = root.join(format!(
        ".tmp-{}-{}-{}",
        pin.dir_name(),
        pin.version,
        std::process::id()
    ));
    let cache_dir = root.join(".npm-cache");
    let install = install_into(&npm, &pin, &tmp_dir, &cache_dir, display_name, false).await;
    if let Err(e) = install {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(e);
    }
    if bin_entry(&tmp_dir, &pin, bin_name).is_none() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(HarnessError::Install(format!(
            "npm install {} completed but the package has no runnable bin entry",
            pin.spec()
        )));
    }
    std::fs::write(tmp_dir.join(OK_MARKER), pin.version)?;
    if let Some(parent) = final_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(&tmp_dir, &final_dir).is_err() {
        // Lost a cross-process race (or a stale dir): keep whatever is in
        // place if it's complete, else replace it.
        if installed_entry(&pin, bin_name).is_none() {
            let _ = std::fs::remove_dir_all(&final_dir);
            std::fs::rename(&tmp_dir, &final_dir)?;
        } else {
            let _ = std::fs::remove_dir_all(&tmp_dir);
        }
    }
    installed_entry(&pin, bin_name).ok_or_else(|| {
        HarnessError::Install(format!(
            "install of {} finished but its bin entry did not resolve",
            pin.spec()
        ))
    })
}

// ---------------------------------------------------------------------------------------------
// A package consumed as a LIBRARY by a Reno-owned shim (the Cursor SDK has no bin entry)
// ---------------------------------------------------------------------------------------------

/// Whether the managed install of `pin` under `root` is complete: its marker is written last.
pub(crate) fn is_installed_in(root: &Path, pin: &NpmPin) -> bool {
    install_dir_in(root, pin).join(OK_MARKER).is_file()
}

/// A Reno-owned shim script materialized INSIDE a managed install dir, for SDK packages with no
/// bin entry (`@cursor/sdk`): the shim resolves the SDK from the sibling `node_modules`. Returns
/// the shim path when the install is complete, and `None` when it is not (nothing is installed
/// here). Each build's shim source has its own immutable filename.
pub(crate) fn installed_shim(pin: &NpmPin, shim_name: &str, contents: &str) -> Option<PathBuf> {
    installed_shim_in(&adapters_root()?, pin, shim_name, contents)
}

/// [`installed_shim`] under an explicit managed root.
pub(crate) fn installed_shim_in(
    root: &Path,
    pin: &NpmPin,
    shim_name: &str,
    contents: &str,
) -> Option<PathBuf> {
    if !is_installed_in(root, pin) {
        return None;
    }
    materialize_shim(&install_dir_in(root, pin), shim_name, contents).ok()
}

/// Different running Reno builds must never replace each other's shim: publish complete,
/// content-addressed files, so a reader never sees a partial write.
fn materialize_shim(dir: &Path, name: &str, contents: &str) -> std::io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let digest = format!("{:x}", Sha256::digest(contents.as_bytes()));
    let name = Path::new(name);
    let stem = name.file_stem().unwrap_or_default().to_string_lossy();
    let extension = name.extension().unwrap_or_default().to_string_lossy();
    let shim = dir.join(format!("{stem}-{digest}.{extension}"));
    if std::fs::read(&shim).ok().as_deref() == Some(contents.as_bytes()) {
        return Ok(shim);
    }
    let temporary = dir.join(format!(".shim-{}", uuid::Uuid::new_v4()));
    std::fs::write(&temporary, contents)?;
    if let Err(error) = std::fs::rename(&temporary, &shim) {
        let _ = std::fs::remove_file(&temporary);
        // Another writer publishing these same immutable bytes is a successful race.
        if std::fs::read(&shim).ok().as_deref() != Some(contents.as_bytes()) {
            return Err(error);
        }
    }
    Ok(shim)
}

/// Like [`ensure_installed`], for a package consumed as a LIBRARY by a Reno-owned shim rather
/// than through a bin entry. Installs the pin once (the explicit install step: a run never
/// calls this), writes `contents` to a content-addressed sibling of `shim_name`, and returns the
/// shim path (spawn it via [`launch_for_entry`]). The package's install scripts never run.
pub(crate) async fn ensure_installed_shim(
    pin: NpmPin,
    display_name: &str,
    shim_name: &str,
    contents: &str,
) -> Result<PathBuf, HarnessError> {
    let root = adapters_root().ok_or_else(|| {
        HarnessError::Install("cannot locate a platform data directory for managed adapters".into())
    })?;
    ensure_installed_shim_in(&root, find_npm(), pin, display_name, shim_name, contents).await
}

/// [`ensure_installed_shim`] under an explicit managed root and npm.
pub(crate) async fn ensure_installed_shim_in(
    root: &Path,
    npm: Option<PathBuf>,
    pin: NpmPin,
    display_name: &str,
    shim_name: &str,
    contents: &str,
) -> Result<PathBuf, HarnessError> {
    if let Some(shim) = installed_shim_in(root, &pin, shim_name, contents) {
        return Ok(shim);
    }
    let _guard = install_lock().lock().await;
    if let Some(shim) = installed_shim_in(root, &pin, shim_name, contents) {
        return Ok(shim);
    }
    let Some(npm) = npm else {
        return Err(HarnessError::NotInstalled(format!(
            "npm (required to install the {display_name} SDK {}; searched PATH, the login \
             shell's PATH, and fnm/nvm/volta/pnpm/bun install dirs)",
            pin.spec()
        )));
    };
    let final_dir = install_dir_in(root, &pin);
    let tmp_dir = root.join(format!(
        ".tmp-{}-{}-{}",
        pin.dir_name(),
        pin.version,
        std::process::id()
    ));
    let cache_dir = root.join(".npm-cache");
    // The SDK's scripts are its build's (`build`, `prepare:rg`, ...); none runs at install, and
    // none may.
    let install = install_into(&npm, &pin, &tmp_dir, &cache_dir, display_name, true).await;
    if let Err(e) = install {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(e);
    }
    let published = materialize_shim(&tmp_dir, shim_name, contents)
        .and_then(|_| std::fs::write(tmp_dir.join(OK_MARKER), pin.version));
    if let Err(e) = published {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(e.into());
    }
    if let Some(parent) = final_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(&tmp_dir, &final_dir).is_err() {
        // Lost a cross-process race (or a stale dir): keep whatever is in place if it is
        // complete, else replace it.
        if !is_installed_in(root, &pin) {
            let _ = std::fs::remove_dir_all(&final_dir);
            std::fs::rename(&tmp_dir, &final_dir)?;
        } else {
            let _ = std::fs::remove_dir_all(&tmp_dir);
        }
    }
    installed_shim_in(root, &pin, shim_name, contents).ok_or_else(|| {
        HarnessError::Install(format!(
            "install of {} finished but its shim did not resolve",
            pin.spec()
        ))
    })
}

fn npm_install_plan(
    npm: &Path,
    pin: &NpmPin,
    cache_dir: &Path,
    ignore_scripts: bool,
) -> Result<(PathBuf, Vec<OsString>), HarnessError> {
    let (program, mut args) = (npm.to_path_buf(), Vec::<OsString>::new());
    args.extend(
        [
            "install",
            "--no-audit",
            "--no-fund",
            "--no-progress",
            "--loglevel=error",
            // Defeat a user-level `omit=optional`: packages can ship their
            // platform binary as an optional dependency.
            "--include=optional",
            "--cache",
        ]
        .into_iter()
        .map(OsString::from),
    );
    args.push(cache_dir.as_os_str().to_owned());
    if ignore_scripts {
        // A registry agent's package: its install scripts never run (the
        // ACP registry quarantines packages that need them).
        args.push(OsString::from("--ignore-scripts"));
    }
    args.push(OsString::from(pin.spec()));
    Ok((program, args))
}

pub(crate) async fn install_into(
    npm: &Path,
    pin: &NpmPin,
    tmp_dir: &Path,
    cache_dir: &Path,
    display_name: &str,
    ignore_scripts: bool,
) -> Result<(), HarnessError> {
    let _ = std::fs::remove_dir_all(tmp_dir);
    std::fs::create_dir_all(tmp_dir)?;
    std::fs::create_dir_all(cache_dir)?;
    // A bare manifest keeps npm from walking up into a user project.
    std::fs::write(tmp_dir.join("package.json"), "{\"private\":true}\n")?;
    tracing::info!(
        target: "reno_harness::adapter_install",
        package = %pin.spec(),
        dir = %tmp_dir.display(),
        "installing ACP adapter"
    );
    let (program, args) = npm_install_plan(npm, pin, cache_dir, ignore_scripts)?;
    let mut cmd = crate::process::Command::new(&program);
    cmd.args(args)
        .current_dir(tmp_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    crate::compose_child_path(&mut cmd, &program);
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let drain = async {
        use tokio::io::AsyncReadExt;
        let mut out = String::new();
        let mut err = String::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_string(&mut out).await;
        }
        if let Some(mut s) = stderr {
            let _ = s.read_to_string(&mut err).await;
        }
        (out, err)
    };
    let ((out, err), status) =
        match tokio::time::timeout(INSTALL_TIMEOUT, async { tokio::join!(drain, child.wait()) })
            .await
        {
            Ok((streams, status)) => (streams, status),
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(HarnessError::Install(format!(
                    "npm install of the {display_name} adapter ({}) timed out after {} minutes — \
                 check your network and npm registry configuration",
                    pin.spec(),
                    INSTALL_TIMEOUT.as_secs() / 60
                )));
            }
        };
    let status = status?;
    if status.success() {
        return Ok(());
    }
    let mut output = err.trim().to_owned();
    if output.is_empty() {
        output = out.trim().to_owned();
    }
    let tail: String = if output.len() > 1200 {
        // npm front-loads "npm ERR!" lines; keep the tail where the cause lands.
        format!("…{}", &output[output.len() - 1200..])
    } else {
        output
    };
    let exit = describe_npm_exit(Some(status));
    Err(HarnessError::Install(if tail.is_empty() {
        format!(
            "npm install of the {display_name} adapter ({}) failed silently ({exit}); \
             npm's own cache or config is likely broken — try `npm cache verify` \
             or reinstalling node/npm",
            pin.spec()
        )
    } else {
        format!(
            "npm install of the {display_name} adapter ({}) failed ({exit}): {tail}",
            pin.spec()
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    fn env(values: &[(&str, OsString)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let values: HashMap<String, OsString> = values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect();
        move |key| values.get(key).cloned()
    }

    #[test]
    fn pin_parses_scoped_and_bare_names() {
        let pin = NpmPin::parse("@agentclientprotocol/codex-acp@1.1.14");
        assert_eq!(pin.name, "@agentclientprotocol/codex-acp");
        assert_eq!(pin.version, "1.1.14");
        assert_eq!(pin.dir_name(), "agentclientprotocol__codex-acp");

        let pin = NpmPin::parse("example-adapter@0.0.33");
        assert_eq!(pin.name, "example-adapter");
        assert_eq!(pin.version, "0.0.33");
        assert_eq!(pin.dir_name(), "example-adapter");
    }

    #[test]
    fn npm_errno_exits_are_decoded() {
        let status = |code: i32| {
            use std::os::unix::process::ExitStatusExt;
            Some(std::process::ExitStatus::from_raw(code << 8))
        };
        assert!(describe_npm_exit(status(254)).contains("ENOENT"));
        assert!(describe_npm_exit(status(243)).contains("EACCES"));
        assert!(describe_npm_exit(status(226)).contains("EROFS"));
        assert_eq!(describe_npm_exit(status(1)), "exit code 1");
    }

    #[test]
    fn bin_entry_reads_string_and_map_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let pin = NpmPin::parse("@scope/tool@1.0.0");
        let pkg = dir.path().join("node_modules").join("@scope").join("tool");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("dist.js"), "x").unwrap();

        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@scope/tool","bin":"dist.js"}"#,
        )
        .unwrap();
        assert_eq!(
            bin_entry(dir.path(), &pin, "tool"),
            Some(pkg.join("dist.js"))
        );

        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@scope/tool","bin":{"tool":"dist.js","other":"missing.js"}}"#,
        )
        .unwrap();
        assert_eq!(
            bin_entry(dir.path(), &pin, "tool"),
            Some(pkg.join("dist.js"))
        );

        // Marker gating uses an isolated managed root, never the process HOME.
        let root = dir.path().join("isolated-adapters");
        let installed = install_dir_in(&root, &pin);
        let installed_pkg = installed.join("node_modules").join("@scope").join("tool");
        std::fs::create_dir_all(&installed_pkg).unwrap();
        std::fs::write(installed_pkg.join("dist.js"), "x").unwrap();
        std::fs::write(
            installed_pkg.join("package.json"),
            r#"{"name":"@scope/tool","bin":"dist.js"}"#,
        )
        .unwrap();
        assert_eq!(installed_entry_in(&root, &pin, "tool"), None);
        std::fs::write(installed.join(OK_MARKER), pin.version).unwrap();
        assert_eq!(
            installed_entry_in(&root, &pin, "tool"),
            Some(installed_pkg.join("dist.js"))
        );
    }

    #[test]
    fn adapters_root_prefers_explicit_dirs_then_the_platform_data_dir() {
        let explicit = PathBuf::from("/srv/reno-adapters");
        let data = PathBuf::from("/srv/reno-data");
        let home = PathBuf::from("/home/ada");
        let xdg = PathBuf::from("/xdg/data");
        assert_eq!(
            adapters_root_with(
                &env(&[
                    ("RENO_ADAPTERS_DIR", explicit.clone().into_os_string()),
                    ("RENO_DATA_DIR", data.clone().into_os_string()),
                    ("HOME", home.clone().into_os_string()),
                ]),
                true,
            ),
            Some(explicit)
        );
        assert_eq!(
            adapters_root_with(
                &env(&[("RENO_DATA_DIR", data.clone().into_os_string())]),
                false,
            ),
            Some(data.join("adapters"))
        );
        assert_eq!(
            adapters_root_with(&env(&[("HOME", home.clone().into_os_string())]), true),
            Some(home.join("Library/Application Support/Reno/adapters"))
        );
        assert_eq!(
            adapters_root_with(&env(&[("HOME", home.clone().into_os_string())]), false),
            Some(home.join(".local/share/reno/adapters"))
        );
        assert_eq!(
            adapters_root_with(
                &env(&[
                    ("HOME", home.into_os_string()),
                    ("XDG_DATA_HOME", xdg.clone().into_os_string()),
                ]),
                false,
            ),
            Some(xdg.join("reno/adapters"))
        );
        assert_eq!(adapters_root_with(&env(&[]), false), None);
    }

    #[test]
    fn npm_discovery_uses_the_path_and_the_install_plan_keeps_argument_boundaries() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let node_root = temp.path().join("Node's Ω & ^ % !");
        let npm = node_root.join("npm");
        std::fs::create_dir_all(&node_root).unwrap();
        std::fs::write(&npm, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([&node_root]).unwrap();
        assert_eq!(
            find_npm_with(&env(&[("PATH", path)]), None),
            Some(npm.clone())
        );

        let cache = temp.path().join("cache's Ω & $(echo nope); ^ % !");
        let pin = NpmPin::parse("@scope/tool@1.2.3");
        let (program, args) = npm_install_plan(&npm, &pin, &cache, false).unwrap();
        assert_eq!(program, npm);
        assert_eq!(args[0], OsString::from("install"));
        assert_eq!(args[7], cache.into_os_string());
        assert_eq!(args[8], OsString::from("@scope/tool@1.2.3"));
        assert_eq!(args.len(), 9);
    }

    #[test]
    fn managed_entry_launch_runs_native_images_directly_and_preserves_javascript_argument_boundaries()
     {
        let temp = tempfile::tempdir().unwrap();
        let elf = temp.path().join("adapter");
        std::fs::write(&elf, b"\x7fELF\x02\x01\x01\x00").unwrap();
        assert_eq!(
            launch_for_entry_with_node(&elf, None).unwrap(),
            (elf, Vec::new())
        );

        let js = temp.path().join("adapter's Ω & $(nope); ^ % !.js");
        let node = temp.path().join("node");
        std::fs::write(&js, b"#!/usr/bin/env node\n").unwrap();
        let launch = launch_for_entry_with_node(&js, Some(node.clone())).unwrap();
        assert_eq!(launch.0, node);
        assert_eq!(launch.1, vec![js.display().to_string()]);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod shim_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const PIN: &str = "@scope/sdk@1.2.3";

    /// Make `path` executable and wait out ETXTBSY: a script written while other tests fork can
    /// be "text file busy" for a moment, because a forked child holds the write descriptor
    /// until it execs.
    fn executable(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        for _ in 0..200 {
            let ran = std::process::Command::new(path)
                .arg("--probe")
                .current_dir(path.parent().unwrap())
                .output();
            match ran {
                Err(error) if error.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                _ => break,
            }
        }
    }

    /// An `npm` that lays a package out the way a real install does, and records its arguments.
    fn fake_npm(dir: &Path) -> (PathBuf, PathBuf) {
        let log = dir.join("npm.log");
        let npm = dir.join("npm");
        executable(
            &npm,
            &format!(
                "#!/bin/sh\necho \"$@\" >> '{}'\nmkdir -p node_modules/@scope/sdk\n\
                 echo '{{\"name\":\"@scope/sdk\",\"version\":\"1.2.3\"}}' > \
                 node_modules/@scope/sdk/package.json\n",
                log.display()
            ),
        );
        // The wait above ran it once with `--probe`.
        std::fs::remove_file(&log).ok();
        (npm, log)
    }

    #[tokio::test]
    async fn the_explicit_install_runs_npm_without_scripts_and_publishes_the_shim() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("adapters");
        let (npm, log) = fake_npm(dir.path());
        let pin = NpmPin::parse(PIN);
        assert!(!is_installed_in(&root, &pin));
        assert_eq!(installed_shim_in(&root, &pin, "shim.mjs", "x"), None);

        let shim = ensure_installed_shim_in(&root, Some(npm.clone()), pin, "Sdk", "shim.mjs", "x")
            .await
            .unwrap();
        assert!(is_installed_in(&root, &pin));
        assert_eq!(std::fs::read_to_string(&shim).unwrap(), "x");
        assert!(shim.starts_with(root.join("scope__sdk").join("1.2.3")));
        assert!(
            root.join("scope__sdk/1.2.3/node_modules/@scope/sdk/package.json")
                .is_file()
        );
        let args = std::fs::read_to_string(&log).unwrap();
        assert!(args.contains("--ignore-scripts"), "{args}");
        assert!(args.contains("@scope/sdk@1.2.3"), "{args}");
        assert_eq!(args.lines().count(), 1);

        // Done once: a second call, and one from a build with another shim source, run no npm.
        let again = ensure_installed_shim_in(&root, Some(npm.clone()), pin, "Sdk", "shim.mjs", "x")
            .await
            .unwrap();
        assert_eq!(again, shim);
        let newer = ensure_installed_shim_in(&root, Some(npm), pin, "Sdk", "shim.mjs", "y")
            .await
            .unwrap();
        assert_ne!(newer, shim);
        assert_eq!(std::fs::read_to_string(&shim).unwrap(), "x");
        assert_eq!(std::fs::read_to_string(&newer).unwrap(), "y");
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
    }

    #[tokio::test]
    async fn no_npm_is_not_installed_and_a_failed_install_leaves_nothing_that_looks_installed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("adapters");
        let pin = NpmPin::parse(PIN);
        let error = ensure_installed_shim_in(&root, None, pin, "Sdk", "shim.mjs", "x")
            .await
            .unwrap_err();
        assert!(matches!(error, HarnessError::NotInstalled(_)), "{error}");

        let failing = dir.path().join("failing-npm");
        executable(
            &failing,
            "#!/bin/sh\necho 'registry down' >&2\nexit 1\n",
        );
        let error = ensure_installed_shim_in(&root, Some(failing), pin, "Sdk", "shim.mjs", "x")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("registry down"), "{error}");
        assert!(!is_installed_in(&root, &pin));
        assert_eq!(installed_shim_in(&root, &pin, "shim.mjs", "x"), None);
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
                    .collect()
            })
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn concurrent_builds_publish_immutable_complete_shims() {
        let dir = tempfile::tempdir().unwrap();
        let sources: Vec<String> = (0..8)
            .map(|build| format!("// build {build}\n{}", "x".repeat(64 * 1024)))
            .collect();
        std::thread::scope(|scope| {
            for source in &sources {
                let dir = dir.path();
                scope.spawn(move || {
                    for _ in 0..40 {
                        let shim = materialize_shim(dir, "shim.mjs", source).unwrap();
                        assert_eq!(std::fs::read_to_string(shim).unwrap(), *source);
                        // An older Reno can still rewrite a legacy filename.
                        std::fs::write(dir.join("shim.mjs"), "old build").unwrap();
                    }
                });
            }
        });
    }
}
