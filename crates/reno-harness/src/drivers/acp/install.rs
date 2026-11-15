//! Antigravity's server: the pinned release archive, trusted installs, the registry's newer
//! releases and pruning of superseded ones.
//!
//! Nothing here runs on the run path except locating an installed server
//! ([`newest_antigravity_install`]); installing is an explicit action
//! ([`install_harness`], [`install_antigravity_release`]) the caller takes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::HarnessError;
use crate::process::{Command, Stdio};
use crate::upstream_proto::HarnessId;

pub(super) fn antigravity_archive() -> Option<crate::archive_install::ArchivePin> {
    let (url, entry, sha512) = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        (
            "https://dl.google.com/agy-extensions/releases/macos/agy-acp-server-1.2.1-darwin-arm64.zip",
            "agy_acp_server.par",
            "c0049b1f423ffdf0e6a79a6caa104a27b6368522b1eb2c67d9eeed1424af2a07898a66b475b0824a734a0bdd01264473a7d67d1550357a8607fb136507b0afae",
        )
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        (
            "https://dl.google.com/agy-extensions/releases/macos/agy-acp-server-1.2.1-darwin-x86_64.zip",
            "agy_acp_server.par",
            "790541c7e7bbbac1cb15bb4a77b02c53f30c997090487fbf42df16846e2e5d8d4f1cec92038d88956fbec33331ebacefe8faf974aad0690aadc2332215712ff2",
        )
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        (
            "https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-1.2.1-linux-x86_64.zip",
            "agy_acp_server.par",
            "dd9b778479dbfc753661d63ff66a3235e7249e8451af12adc11d26509172de1ff50452c3af57684009bb944314bd587b2be200bab17df743a2706ce4067d6731",
        )
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        (
            "https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-1.2.1-linux-arm64.zip",
            "agy_acp_server.par",
            "d907f928609ff2232bfeeec9177fe925d3e02e1773295cfb30097e5c4e134758021bff916ba5ae2e2e2ff81d374564349f268f92a4577bc88370537011473e29",
        )
    } else {
        return None;
    };
    Some(crate::archive_install::ArchivePin {
        name: "antigravity-acp",
        version: "1.2.1",
        url,
        entry,
        sha512,
    })
}

fn antigravity_legacy_archive() -> Option<crate::archive_install::ArchivePin> {
    let (url, entry, sha512) = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        (
            "https://dl.google.com/agy-extensions/releases/macos/agy-acp-server-agy_acp_server_1.1.1-darwin-arm64.zip",
            "agy_acp_server.par",
            "82576ba00164331daeba798db43f9e7c9097b76f0cd536cc07956f976e0132e3500f51b28288f07bdb5a3f90f9702dabc20ca0627edb25aa7e8e50f9fac29b8a",
        )
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        (
            "https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-agy_acp_server_1.1.1-linux-x86_64.zip",
            "agy_acp_server.par",
            "bccda188b2903d3ff7a56691064fc8698f76d7bc2bb0f24e99bb5f8cda02c77f77629edc5dba640d6e713edf8e6576dfa38e88fb532752a53e26710934b5e4f0",
        )
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        (
            "https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-agy_acp_server_1.1.1-linux-arm64.zip",
            "agy_acp_server.par",
            "f895b4ade624e25f9765c90df66f3a864b19b2d1f2bde2fc485de6b6782ce30be458abed2da587d7cdd2272ea0812a8452639682c6318127e4fe0b4b8fc62dcf",
        )
    } else {
        return None;
    };
    Some(crate::archive_install::ArchivePin {
        name: "antigravity-acp",
        version: "1.1.1",
        url,
        entry,
        sha512,
    })
}

const ANTIGRAVITY_ARCHIVE_NAME: &str = "antigravity-acp";

/// the acp registry's manifest for google's server, the source of releases
/// newer than the pin compiled into this build.
pub const ANTIGRAVITY_REGISTRY_URL: &str = "https://raw.githubusercontent.com/agentclientprotocol/registry/main/antigravity-acp/agent.json";

/// install marker for a release proven by google's code signature, which has
/// no digest in reno's source to record instead.
const ANTIGRAVITY_SIGNED_MARKER: &str = "google-code-signature";

pub(super) fn antigravity_entry() -> &'static str {
    "agy_acp_server.par"
}

fn antigravity_registry_platform() -> Option<&'static str> {
    Some(if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-aarch64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "darwin-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-aarch64"
    } else {
        return None;
    })
}

/// completed installs whose marker proves their origin: a digest pinned by
/// this or an earlier build, or google's verified code signature.
fn trusted_antigravity_installs() -> Vec<(semver::Version, PathBuf)> {
    let pins: Vec<_> = [antigravity_archive(), antigravity_legacy_archive()]
        .into_iter()
        .flatten()
        .collect();
    crate::archive_install::installed_versions(ANTIGRAVITY_ARCHIVE_NAME)
        .into_iter()
        .filter_map(|(version, marker)| {
            let parsed = semver::Version::parse(&version).ok()?;
            let trusted = pins
                .iter()
                .any(|pin| pin.version == version && pin.sha512 == marker)
                || (crate::code_signature::SUPPORTED && marker == ANTIGRAVITY_SIGNED_MARKER);
            if !trusted {
                return None;
            }
            crate::archive_install::installed_entry_with_marker(
                ANTIGRAVITY_ARCHIVE_NAME,
                &version,
                antigravity_entry(),
                &marker,
            )
            .map(|entry| (parsed, entry))
        })
        .collect()
}

pub(super) fn newest_antigravity_install() -> Option<(semver::Version, PathBuf)> {
    trusted_antigravity_installs()
        .into_iter()
        .max_by(|(left, _), (right, _)| left.cmp(right))
}

/// whether `executable` is the server reno installed and may replace.
pub fn is_managed_antigravity_server(executable: &Path) -> bool {
    newest_antigravity_install().is_some_and(|(_, entry)| entry == executable)
}

/// the registry's current release for this platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntigravityRelease {
    pub version: String,
    /// `None` when the manifest has no usable google-hosted archive here.
    archive_url: Option<String>,
}

impl AntigravityRelease {
    /// whether reno can prove this release's origin before installing it.
    pub fn installable(&self) -> bool {
        antigravity_archive().is_some_and(|pin| pin.version == self.version)
            || (crate::code_signature::SUPPORTED && self.archive_url.is_some())
    }
}

pub fn antigravity_release(manifest: &Value) -> Option<AntigravityRelease> {
    if manifest.get("id")?.as_str()? != ANTIGRAVITY_ARCHIVE_NAME {
        return None;
    }
    let version = manifest.get("version")?.as_str()?;
    semver::Version::parse(version).ok()?;
    let archive_url = antigravity_registry_platform()
        .and_then(|platform| manifest.get("distribution")?.get("binary")?.get(platform))
        .filter(|binary| {
            binary
                .get("cmd")
                .and_then(Value::as_str)
                .is_some_and(|cmd| cmd.strip_prefix("./").unwrap_or(cmd) == antigravity_entry())
        })
        .and_then(|binary| binary.get("archive")?.as_str())
        .filter(|url| is_google_release_url(url))
        .map(str::to_owned);
    Some(AntigravityRelease {
        version: version.to_owned(),
        archive_url,
    })
}

fn is_google_release_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("dl.google.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path().starts_with("/agy-extensions/releases/")
    })
}

/// install `release` beside earlier ones; launches switch to it once complete.
pub async fn install_antigravity_release(
    release: &AntigravityRelease,
) -> Result<PathBuf, HarnessError> {
    if let Some(pin) = antigravity_archive().filter(|pin| pin.version == release.version) {
        return crate::archive_install::ensure_installed(pin, "Antigravity").await;
    }
    if !crate::code_signature::SUPPORTED {
        return Err(HarnessError::Install(format!(
            "Antigravity {} is not pinned by this build of Reno; update Reno to install it",
            release.version
        )));
    }
    let url = release.archive_url.as_deref().ok_or_else(|| {
        HarnessError::Install(format!(
            "the ACP registry lists no Google-hosted Antigravity {} archive for this platform",
            release.version
        ))
    })?;
    let verified = crate::archive_install::VerifiedRelease {
        name: ANTIGRAVITY_ARCHIVE_NAME,
        version: &release.version,
        url,
        entry: antigravity_entry(),
        marker: ANTIGRAVITY_SIGNED_MARKER,
    };
    let expected = release.version.clone();
    crate::archive_install::install_verified(&verified, "Antigravity", |dir| async move {
        crate::code_signature::verify_google_signed(&dir).await?;
        // a signature proves google built the server, not that it is the
        // release the registry named; the install directory, and so launch
        // order, is keyed by that name.
        confirm_reported_version(&dir.join(antigravity_entry()), &expected).await
    })
    .await
}

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(60);

async fn confirm_reported_version(server: &Path, expected: &str) -> Result<(), HarnessError> {
    let mut command = Command::new(server);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(VERSION_PROBE_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            HarnessError::Install(format!(
                "the downloaded Antigravity server did not report its version within {}s",
                VERSION_PROBE_TIMEOUT.as_secs()
            ))
        })??;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    match antigravity_build_version(&text) {
        Some(reported) if reported == expected => Ok(()),
        Some(reported) => Err(HarnessError::Install(format!(
            "the ACP registry lists Antigravity {expected}, but its archive contains {reported}"
        ))),
        None => Err(HarnessError::Install(format!(
            "the downloaded Antigravity {expected} server reported no build label"
        ))),
    }
}

/// the release in `--version` output's `Build label:` line. google's build
/// metadata also names branch revisions, which are not releases.
pub fn antigravity_build_version(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Build label: "))
        // 1.1.1 prefixed its label with the target name; 1.2.1 dropped it.
        .map(|label| label.trim_start_matches("agy_acp_server_"))
        .filter(|version| semver::Version::parse(version).is_ok())
        .map(str::to_owned)
}

/// remove trusted installs older than the one launches resolve to. a version
/// still running (in any process, including another reno) is kept, and on
/// failure to inspect processes nothing is removed.
pub fn prune_superseded_antigravity_installs() {
    let Some((newest, _)) = newest_antigravity_install() else {
        return;
    };
    let Some(adapters) = crate::adapter_install::adapters_root() else {
        return;
    };
    let Some(running) = running_command_lines() else {
        return;
    };
    for (version, entry) in trusted_antigravity_installs() {
        let Some(dir) = entry.parent().filter(|_| version < newest) else {
            continue;
        };
        let prefixes = install_path_prefixes(dir);
        if running
            .iter()
            .any(|line| prefixes.iter().any(|prefix| line.contains(prefix)))
        {
            continue;
        }
        let trash = adapters.join(format!(
            ".trash-{ANTIGRAVITY_ARCHIVE_NAME}-{version}-{}",
            std::process::id()
        ));
        if std::fs::rename(dir, &trash).is_ok()
            && let Err(error) = std::fs::remove_dir_all(&trash)
        {
            tracing::warn!(%error, %version, "could not remove a superseded Antigravity server");
        }
    }
}

#[cfg(unix)]
pub(crate) fn running_command_lines() -> Option<Vec<String>> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-axww", "-o", "command="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect(),
    )
}

/// how a process may name files under `dir`: as Reno spawned it, and in its
/// canonical form (symlinks resolved).
fn install_path_prefixes(dir: &Path) -> Vec<String> {
    let with_separator = |path: &Path| format!("{}{}", path.display(), std::path::MAIN_SEPARATOR);
    let mut prefixes = vec![with_separator(dir)];
    if let Ok(canonical) = std::fs::canonicalize(dir) {
        prefixes.push(with_separator(&canonical));
    }
    prefixes.dedup();
    prefixes
}

/// Whether the listing device has a pinned, explicitly installable archive.
pub fn can_install(harness: HarnessId) -> bool {
    harness == HarnessId::Antigravity && antigravity_archive().is_some()
}

/// Only an explicit Settings action may call this installer.
pub async fn install_harness(harness: HarnessId) -> Result<(), HarnessError> {
    let pin = (harness == HarnessId::Antigravity)
        .then(antigravity_archive)
        .flatten()
        .ok_or_else(|| {
            HarnessError::NotInstalled(
                "Set ANTIGRAVITY_ACP_EXECUTABLE to an installed ACP server".into(),
            )
        })?;
    crate::archive_install::ensure_installed(pin, "Antigravity").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::AcpExecutor;

    #[cfg(unix)]
    #[tokio::test]
    async fn signed_release_must_report_the_registry_version() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("agy_acp_server.par");
        std::fs::write(
            &server,
            "#!/bin/sh\necho 'Built from changelist 1 based on //depot/branches/agy_acp_server_release_branch/973763860.1'\necho 'Build label: 1.2.0'\n",
        )
        .unwrap();
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
        confirm_reported_version(&server, "1.2.0").await.unwrap();
        let error = confirm_reported_version(&server, "9.9.9")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("contains 1.2.0"), "{error}");

        std::fs::write(&server, "#!/bin/sh\necho 'agy 9.9.9'\n").unwrap();
        let error = confirm_reported_version(&server, "9.9.9")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no build label"), "{error}");
    }

    #[test]
    fn antigravity_build_version_reads_the_label_not_the_branch() {
        let legacy = "Built from changelist 975248206 based on //depot/branches/agy_acp_server_release_branch/973763860.1/google3\nBuild label: agy_acp_server_1.1.1";
        assert_eq!(antigravity_build_version(legacy).as_deref(), Some("1.1.1"));
        assert_eq!(
            antigravity_build_version("Build label: 1.2.1\nBuild platform: darwin_arm64")
                .as_deref(),
            Some("1.2.1")
        );
        assert_eq!(antigravity_build_version("Build label: nightly"), None);
        assert_eq!(antigravity_build_version("Built on Sep 2"), None);
    }

    /// The server is found among installs this build trusts, newest first, and superseded ones
    /// are pruned unless a process still runs them. Runs in a child process with its own adapters
    /// directory and a PATH that holds nothing.
    #[cfg(unix)]
    #[test]
    fn antigravity_launches_the_newest_trusted_install_and_prunes_the_rest() {
        let Ok(adapters) = std::env::var("RENO_TEST_AGY_ADAPTERS") else {
            let root = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "drivers::acp::install::tests::antigravity_launches_the_newest_trusted_install_and_prunes_the_rest",
                    "--nocapture",
                ])
                .env("RENO_ADAPTERS_DIR", root.path())
                .env("RENO_TEST_AGY_ADAPTERS", root.path())
                .env("PATH", root.path().join("bin"))
                .env("RENO_NO_LOGIN_SHELL", "1")
                .env_remove("ANTIGRAVITY_ACP_EXECUTABLE")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        };
        let (Some(current), Some(legacy)) = (antigravity_archive(), antigravity_legacy_archive())
        else {
            return;
        };
        let root = PathBuf::from(adapters).join(ANTIGRAVITY_ARCHIVE_NAME);
        let install = |version: &str, marker: &str| {
            let dir = root.join(version);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(crate::adapter_install::OK_MARKER), marker).unwrap();
            let server = dir.join(antigravity_entry());
            std::fs::write(&server, "").unwrap();
            server
        };
        let executor = AcpExecutor::antigravity();
        assert!(!executor.installed());

        let legacy_server = install(legacy.version, legacy.sha512);
        assert!(executor.installed());
        assert_eq!(executor.launch_program().ok(), Some(legacy_server.clone()));
        assert!(is_managed_antigravity_server(&legacy_server));

        let current_server = install(current.version, current.sha512);
        install("9.0.0", "tampered");
        install("not-a-version", ANTIGRAVITY_SIGNED_MARKER);
        assert_eq!(executor.launch_program().ok(), Some(current_server.clone()));
        assert!(!is_managed_antigravity_server(&legacy_server));

        let signed_server = install("99.0.0", ANTIGRAVITY_SIGNED_MARKER);
        let newest = if crate::code_signature::SUPPORTED {
            signed_server
        } else {
            current_server.clone()
        };
        assert_eq!(executor.launch_program().ok(), Some(newest.clone()));
        assert!(is_managed_antigravity_server(&newest));

        let mut running = {
            let mut command = std::process::Command::new("/bin/sh");
            command
                .args(["-c", "while :; do /bin/sleep 0.1; done"])
                .arg(&legacy_server);
            command
        };
        let mut running = running
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        prune_superseded_antigravity_installs();
        assert!(legacy_server.is_file(), "a running server is kept");
        assert_eq!(current_server.is_file(), newest == current_server);
        assert!(newest.is_file());
        assert!(
            root.join("9.0.0").is_dir(),
            "untrusted installs are left alone"
        );
        assert!(root.join("not-a-version").is_dir());

        running.kill().unwrap();
        running.wait().unwrap();
        // The exited process may take a moment to release its image.
        for _ in 0..50 {
            prune_superseded_antigravity_installs();
            if !legacy_server.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!legacy_server.exists());
        assert_eq!(executor.launch_program().ok(), Some(newest));
        let leftovers: Vec<_> = std::fs::read_dir(root.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".trash-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// Downloads Google's real archive (~110 MB). Run with
    /// `RENO_TEST_AGY_LIVE_SIGNED=1 RENO_ADAPTERS_DIR=<empty dir>`.
    #[tokio::test]
    async fn antigravity_live_release_is_refused_when_it_misreports_its_version() {
        if std::env::var_os("RENO_TEST_AGY_LIVE_SIGNED").is_none() {
            return;
        }
        let pin = antigravity_archive().unwrap();
        // Google's signed archive, listed under a version it does not contain.
        let release = AntigravityRelease {
            version: "9.9.9".into(),
            archive_url: Some(pin.url.to_owned()),
        };
        assert!(release.installable());
        let error = install_antigravity_release(&release).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("contains {}", pin.version)),
            "{error}"
        );
        assert!(trusted_antigravity_installs().is_empty());
        let adapters = crate::adapter_install::adapters_root().unwrap();
        assert!(
            !adapters
                .join(ANTIGRAVITY_ARCHIVE_NAME)
                .join("9.9.9")
                .exists()
        );
    }

    #[test]
    fn antigravity_registry_release_accepts_only_google_hosted_archives() {
        let Some(platform) = antigravity_registry_platform() else {
            return;
        };
        let manifest = |version: &str, archive: &str, cmd: &str| {
            json!({
                "id": "antigravity-acp",
                "version": version,
                "distribution": {"binary": {platform: {"archive": archive, "cmd": cmd}}}
            })
        };
        let url = "https://dl.google.com/agy-extensions/releases/x/agy-acp-server-1.3.0.zip";
        let cmd = format!("./{}", antigravity_entry());
        let release = antigravity_release(&manifest("1.3.0", url, &cmd)).unwrap();
        assert_eq!(release.version, "1.3.0");
        assert_eq!(release.archive_url.as_deref(), Some(url));
        assert_eq!(release.installable(), crate::code_signature::SUPPORTED);

        for rejected in [
            "http://dl.google.com/agy-extensions/releases/x.zip",
            "https://dl.google.com.evil.test/agy-extensions/releases/x.zip",
            "https://dl.google.com:8443/agy-extensions/releases/x.zip",
            "https://user@dl.google.com/agy-extensions/releases/x.zip",
            "https://dl.google.com/other/x.zip",
            "https://example.com/agy-extensions/releases/x.zip",
        ] {
            let release = antigravity_release(&manifest("1.3.0", rejected, &cmd)).unwrap();
            assert_eq!(release.archive_url, None, "{rejected}");
            assert!(!release.installable(), "{rejected}");
        }
        let renamed = antigravity_release(&manifest("1.3.0", url, "./other_server")).unwrap();
        assert_eq!(renamed.archive_url, None);

        assert!(antigravity_release(&manifest("latest", url, &cmd)).is_none());
        assert!(antigravity_release(&manifest("../1.3.0", url, &cmd)).is_none());
        assert!(antigravity_release(&json!({"id": "other", "version": "1.3.0"})).is_none());

        if let Some(pin) = antigravity_archive() {
            let pinned =
                antigravity_release(&json!({"id": "antigravity-acp", "version": pin.version}))
                    .unwrap();
            assert!(
                pinned.installable(),
                "the pinned digest needs no registry archive"
            );
        }
    }
}
