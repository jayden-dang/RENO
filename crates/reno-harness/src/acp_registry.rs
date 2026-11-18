//! Agents from the ACP registry (agentclientprotocol.com's public index of
//! every agent that speaks the Agent Client Protocol): reading its index,
//! choosing the build for this device, installing it, and how an added
//! agent launches. Every registry agent goes through this one path and the
//! generic ACP harness ([`crate::AcpExecutor::registry`]); nothing here is
//! about any one agent.
//!
//! The index (`registry.json`) lists each agent with its distributions:
//! a prebuilt archive per platform (`binary`, with an optional sha256), an
//! npm package (`npx`) or a Python one (`uvx`), each pinned to an exact
//! version. An added agent lives in `<agents root>/<registry id>/`:
//!
//!   agent.json            what it is and how it launches ([`Record`])
//!   <version>/            the install (staged in `.tmp-*`, renamed into
//!                         place once its entry resolves and the marker is
//!                         written: a killed install never passes for one)
//!
//! Installs never run a package's own scripts (npm `--ignore-scripts`; the
//! registry quarantines packages that need them), never run npx/uvx at chat
//! time, and a binary's archive is checked against the registry's sha256
//! whenever it publishes one. Extraction refuses absolute paths, `..`,
//! links and special files, and the agent's `cmd` must resolve inside its
//! install.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent_id::RegistryAgent;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::HarnessError;

/// The registry's published index.
pub const INDEX_URL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";
/// Where its icons are served from (and the only place they're taken from).
pub const ICON_ORIGIN: &str = "https://cdn.agentclientprotocol.com/";

const MAX_INDEX_BYTES: usize = 1024 * 1024;
const MAX_AGENTS: usize = 512;
pub const MAX_ICON_BYTES: usize = 16 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 3 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 20_000;
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
const UV_TIMEOUT: Duration = Duration::from_secs(600);
const MARKER: &str = ".reno-install-ok";
/// What Reno calls itself to a download host: some turn a client with no name away.
const USER_AGENT: &str = concat!("reno/", env!("CARGO_PKG_VERSION"));

/// One agent of the index, as published.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub distribution: Distribution,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Distribution {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub binary: BTreeMap<String, BinaryTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npx: Option<Package>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uvx: Option<Package>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BinaryTarget {
    pub archive: String,
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Package {
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// How an agent installs on a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A prebuilt archive (or raw executable) for the device's platform.
    Binary,
    /// An npm package, run with Node.js.
    Npx,
    /// A Python package, installed with uv.
    Uvx,
    /// One of your own: a program on the device and its arguments, as you
    /// gave them (`own.<slug>`), never installed or updated by Reno.
    Own,
}

/// The registry's ids for the agents Reno drives itself, each with its own
/// harness: shown as built in, never added again from the registry.
pub const BUILT_IN: &[(&str, &str)] = &[
    ("claude-acp", "claude-code"),
    ("codex-acp", "codex"),
    ("devin", "devin"),
    ("grok-build", "grok"),
    ("antigravity-acp", "antigravity"),
    ("pi-acp", "pi"),
    ("muse-code", "muse"),
    ("cursor", "cursor"),
    // opencode has a native driver; the registry lists it twice (the 1.x and the 2.x archives).
    ("opencode", "opencode"),
    ("opencode-v2", "opencode"),
];

/// The built-in harness slug for a registry id Reno drives itself.
pub fn built_in(registry_id: &str) -> Option<&'static str> {
    BUILT_IN
        .iter()
        .find(|(id, _)| *id == registry_id)
        .map(|(_, harness)| *harness)
}

/// The index's agents: each decoded on its own and dropped if it doesn't
/// hold up (an id that isn't one, a URL that isn't https, a package not
/// pinned to an exact version, a `cmd` that leaves its archive), so one bad
/// entry never hides the rest. Errs only when the index itself is unusable.
pub fn parse_index(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(format!(
            "the registry index is over {} KiB",
            MAX_INDEX_BYTES / 1024
        ));
    }
    let index: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("the registry index isn't JSON: {e}"))?;
    let agents = index
        .get("agents")
        .and_then(serde_json::Value::as_array)
        .ok_or("the registry index has no agents")?;
    let mut out: Vec<Entry> = Vec::new();
    for raw in agents.iter().take(MAX_AGENTS) {
        let Ok(entry) = serde_json::from_value::<Entry>(raw.clone()) else {
            continue;
        };
        if valid(&entry) && !out.iter().any(|e| e.id == entry.id) {
            out.push(entry);
        }
    }
    Ok(out)
}

fn short(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.len() <= max && !text.chars().any(char::is_control)
}

/// An https URL with a host and no credentials. (A test registry served on
/// this machine's loopback passes too, with `RENO_ACP_REGISTRY_LOOPBACK=1`.)
pub fn safe_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| {
        let loopback = u.scheme() == "http"
            && matches!(u.host_str(), Some("127.0.0.1" | "localhost"))
            && std::env::var("RENO_ACP_REGISTRY_LOOPBACK").is_ok_and(|v| v == "1");
        (u.scheme() == "https" || loopback)
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
    })
}

fn env_ok(env: &BTreeMap<String, String>) -> bool {
    env.len() <= 32
        && env.iter().all(|(k, v)| {
            !k.is_empty()
                && k.len() <= 128
                && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && v.len() <= 4096
                && !v.contains('\0')
        })
}

fn args_ok(args: &[String]) -> bool {
    args.len() <= 32 && args.iter().all(|a| a.len() <= 512 && !a.contains('\0'))
}

/// `name@1.2.3` / `@scope/name@1.2.3`: a name and an exact version.
pub fn npm_pin(spec: &str) -> Option<(&str, &str)> {
    let at = spec.rfind('@').filter(|&at| at > 0)?;
    let (name, version) = (&spec[..at], &spec[at + 1..]);
    let exact = semver::Version::parse(version).is_ok();
    let name_ok = !name.is_empty()
        && name.len() <= 214
        && name.trim_start_matches('@').bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.' | b'/')
        })
        && name.matches('/').count() == usize::from(name.starts_with('@'));
    (exact && name_ok).then_some((name, version))
}

/// `name==1.2.3` (or uv's `name@1.2.3`): a name and an exact version.
pub fn uv_pin(spec: &str) -> Option<(&str, &str)> {
    let (name, version) = spec.split_once("==").or_else(|| spec.split_once('@'))?;
    let name_ok = !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    let version_ok = !version.is_empty()
        && version.len() <= 64
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'));
    (name_ok && version_ok).then_some((name, version))
}

/// A `cmd` inside its archive: relative, no `..`, no root or drive.
fn relative_cmd(cmd: &str) -> Option<PathBuf> {
    let cmd = cmd.replace('\\', "/");
    let mut out = PathBuf::new();
    for part in cmd.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            p if p.contains(':') => return None,
            p => out.push(p),
        }
    }
    (out.components().count() > 0 && !cmd.starts_with('/')).then_some(out)
}

/// A platform key as the registry writes them (`darwin-aarch64`, `linux-x86_64-musl`): a name,
/// not a path. Platforms this device doesn't run are valid data, only never chosen.
fn platform_name(key: &str) -> bool {
    key.len() <= 40
        && key.contains('-')
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
}

fn valid(e: &Entry) -> bool {
    let d = &e.distribution;
    RegistryAgent::new(&e.id).is_some()
        // (The registry's ids have no dots: `own.<slug>` is yours alone.)
        && !e.id.contains('.')
        && short(&e.name, 80)
        && short(&e.version, 64)
        && e.description.len() <= 600
        && e.authors.len() <= 16
        && e.authors.iter().all(|a| a.len() <= 120)
        && e.license.as_deref().is_none_or(|l| l.len() <= 120)
        && [&e.repository, &e.website, &e.icon]
            .iter()
            .all(|u| u.as_deref().is_none_or(safe_url))
        && (!d.binary.is_empty() || d.npx.is_some() || d.uvx.is_some())
        && d.binary.iter().all(|(platform, t)| {
            platform_name(platform)
                && safe_url(&t.archive)
                && relative_cmd(&t.cmd).is_some()
                && args_ok(&t.args)
                && env_ok(&t.env)
                && t.sha256.as_deref().is_none_or(|h| {
                    h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())
                })
        })
        && d.npx.as_ref().is_none_or(|p| npm_pin(&p.package).is_some() && args_ok(&p.args) && env_ok(&p.env))
        && d.uvx.as_ref().is_none_or(|p| uv_pin(&p.package).is_some() && args_ok(&p.args) && env_ok(&p.env))
}

/// This device's platform key, as the registry names it.
pub fn platform() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("linux", "x86_64") => "linux-x86_64",
        _ => return None,
    })
}

/// What adding an entry would install here: this platform's build first
/// then the npm package, then the Python
/// one.
#[derive(Debug, Clone, PartialEq)]
pub enum Choice<'a> {
    Binary(&'a BinaryTarget),
    Npx(&'a Package),
    Uvx(&'a Package),
}

impl Choice<'_> {
    pub fn kind(&self) -> Kind {
        match self {
            Choice::Binary(_) => Kind::Binary,
            Choice::Npx(_) => Kind::Npx,
            Choice::Uvx(_) => Kind::Uvx,
        }
    }
}

pub fn choose(entry: &Entry) -> Option<Choice<'_>> {
    choose_for(entry, platform())
}

fn choose_for<'a>(entry: &'a Entry, platform: Option<&str>) -> Option<Choice<'a>> {
    let d = &entry.distribution;
    if let Some(platform) = platform
        && let Some(t) = d.binary.get(platform)
    {
        return Some(Choice::Binary(t));
    }
    d.npx
        .as_ref()
        .map(Choice::Npx)
        .or_else(|| d.uvx.as_ref().map(Choice::Uvx))
}

/// What a device lacks to run an entry: nothing, Node.js (npm), uv, or a
/// build for its platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Readiness {
    Ready,
    NeedsNode,
    NeedsUv,
    NoBuild,
}

pub fn readiness(entry: &Entry) -> Readiness {
    readiness_for(
        entry,
        platform(),
        crate::adapter_install::find_npm().is_some(),
        find_uv().is_some(),
    )
}

fn find_uv() -> Option<PathBuf> {
    crate::executable::find_on_paths("uv", Vec::new())
}

/// Whether the download is checked against a digest the registry publishes
/// (a binary with its sha256); npm and PyPI packages are pinned by version
/// and checked by their own registries.
pub fn checksum(entry: &Entry) -> Option<bool> {
    match choose(entry)? {
        Choice::Binary(t) => Some(t.sha256.is_some()),
        _ => None,
    }
}

/// An added agent: what it is, and how it launches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: Kind,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub authors: Vec<String>,
    /// The installed entry: the binary, the npm bin (run with Node.js), or
    /// the uv tool's executable.
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The registry's icon (a one-ink SVG), kept with the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_svg: Option<String>,
    /// The archive was checked against the registry's sha256.
    #[serde(default)]
    pub verified: bool,
}

static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The engine's own place for added agents (`<data dir>/agents`): each
/// engine keeps its own, so a staging or test engine never adds to the
/// production one's. Set once, at startup.
pub fn set_agents_root(dir: PathBuf) {
    let _ = ROOT.set(dir);
}

/// Where added agents live: `$RENO_AGENTS_DIR`, else the engine's
/// ([`set_agents_root`]), else `$RENO_DATA_DIR/agents`, else beside the
/// managed adapters (the `agents` sibling of the adapters directory).
pub fn agents_root() -> Option<PathBuf> {
    let value = |key: &str| {
        std::env::var_os(key)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if let Some(dir) = value("RENO_AGENTS_DIR") {
        return Some(dir);
    }
    if let Some(dir) = ROOT.get() {
        return Some(dir.clone());
    }
    if let Some(dir) = value("RENO_DATA_DIR") {
        return Some(dir.join("agents"));
    }
    crate::adapter_install::adapters_root().and_then(|root| root.parent().map(|p| p.join("agents")))
}

/// An empty folder of Reno's own, where a sign-in or status client starts an agent and has it open
/// its session: not the home directory, which some agents refuse as a workspace (Autohand
/// answers "Unsafe workspace"). Falls back to the system's temporary folder.
pub(crate) fn probe_dir() -> PathBuf {
    let dir = agents_root()
        .map(|root| root.join(".probe"))
        .unwrap_or_else(|| std::env::temp_dir().join("reno-acp-probe"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn agent_dir(agent: RegistryAgent) -> Option<PathBuf> {
    agents_root().map(|root| root.join(agent.registry_id()))
}

/// The added agent's record, if this device has it.
pub fn record(agent: RegistryAgent) -> Option<Record> {
    record_in(&agent_dir(agent)?).filter(|r| r.id == agent.registry_id())
}

fn record_in(dir: &Path) -> Option<Record> {
    serde_json::from_slice(&std::fs::read(dir.join("agent.json")).ok()?).ok()
}

/// Every agent this device has added, by id.
pub fn records() -> Vec<Record> {
    let Some(root) = agents_root() else {
        return Vec::new();
    };
    let Ok(dirs) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<Record> = dirs
        .filter_map(Result::ok)
        .filter_map(|d| RegistryAgent::new(&d.file_name().to_string_lossy()))
        .filter_map(record)
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Whether the agent is added here and its entry is in place.
pub fn installed(agent: RegistryAgent) -> bool {
    record(agent).is_some_and(|r| r.program.is_file())
}

/// What [`launch`] returns: the program, its arguments and its environment.
pub type Launch = (PathBuf, Vec<String>, Vec<(String, String)>);

/// How the agent launches: the program, its arguments (an npm bin runs
/// under Node.js) and the environment the registry gives it.
pub fn launch(agent: RegistryAgent) -> Result<Launch, HarnessError> {
    let record = record(agent).ok_or_else(|| {
        HarnessError::NotInstalled(format!(
            "{} isn't added on this device",
            agent.registry_id()
        ))
    })?;
    if !record.program.is_file() {
        return Err(HarnessError::NotInstalled(format!(
            "{}'s install is missing ({}); add it again",
            record.name,
            record.program.display()
        )));
    }
    let (program, mut args) = match record.kind {
        Kind::Npx => crate::adapter_install::launch_for_entry(&record.program)?,
        Kind::Binary | Kind::Uvx | Kind::Own => (record.program.clone(), Vec::new()),
    };
    args.extend(record.args.iter().cloned());
    // The registry's environment, then yours (yours wins).
    let mut env = record.env;
    env.extend(user_env(agent));
    Ok((program, args, env.into_iter().collect()))
}

/// The id prefix of an agent of your own (`own.<slug>`).
pub const OWN_PREFIX: &str = "own.";

/// Where an agent's own environment is kept on this device (`KEY=VALUE` a
/// line, owner-only): never synced, never in a record or a log.
fn env_file(agent: RegistryAgent) -> Option<PathBuf> {
    agent_dir(agent).map(|d| d.join("env"))
}

/// `KEY=VALUE` a line (blank lines and `#` comments skipped, an `export `
/// before a key allowed), or why a line isn't one.
pub fn parse_env(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {} isn't KEY=VALUE", n + 1));
        };
        let key = key.trim();
        let valid = !key.is_empty()
            && key.len() <= 128
            && !key.as_bytes()[0].is_ascii_digit()
            && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !valid {
            return Err(format!("line {}: {key:?} isn't a variable name", n + 1));
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        if value.contains('\0') || value.len() > 8192 {
            return Err(format!("line {}: the value can't be used", n + 1));
        }
        out.insert(key.to_owned(), value.to_owned());
    }
    if out.len() > 64 {
        return Err("more variables than an agent takes".into());
    }
    Ok(out)
}

/// The environment you gave the agent on this device.
pub fn user_env(agent: RegistryAgent) -> BTreeMap<String, String> {
    env_file(agent)
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|text| parse_env(&text).ok())
        .unwrap_or_default()
}

/// The environment as text, a `KEY=VALUE` line each.
pub fn env_text(env: &BTreeMap<String, String>) -> String {
    env.iter().map(|(k, v)| format!("{k}={v}\n")).collect()
}

/// Keeps `text` as the agent's environment on this device (owner-only,
/// written whole or not at all); empty forgets it.
pub fn set_user_env(
    agent: RegistryAgent,
    text: &str,
) -> Result<BTreeMap<String, String>, HarnessError> {
    let env = parse_env(text).map_err(HarnessError::Install)?;
    record(agent).ok_or_else(|| {
        HarnessError::NotInstalled(format!(
            "{} isn't added on this device",
            agent.registry_id()
        ))
    })?;
    let file = env_file(agent)
        .ok_or_else(|| HarnessError::Install("no data directory for agents".into()))?;
    if env.is_empty() {
        let _ = std::fs::remove_file(&file);
        return Ok(env);
    }
    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut out = options.open(&tmp)?;
        out.write_all(env_text(&env).as_bytes())?;
        out.sync_all()?;
    }
    std::fs::rename(&tmp, &file)?;
    Ok(env)
}

/// An agent of your own's id from its name: `own.` and the name's letters
/// and digits, lowercase, dashes between words.
pub fn own_id(name: &str) -> Option<String> {
    let mut slug = String::new();
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    let slug: String = slug.chars().take(48).collect();
    let id = format!("{OWN_PREFIX}{}", slug.trim_end_matches('-'));
    (!slug.is_empty() && RegistryAgent::new(&id).is_some()).then_some(id)
}

/// A command as a shell splits it: words, `'…'` and `"…"` kept whole, `\`
/// escaping the next character. (No variables, globs or pipes: a command,
/// not a script.)
pub fn split_command(command: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = command.chars();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => word.push(chars.next().ok_or("the command ends in \\")?),
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, '\\') => {
                word.push(chars.next().ok_or("the command ends in \\")?);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, '|' | '&' | ';' | '<' | '>' | '`' | '$') => {
                return Err("a command, not a script: no pipes, redirects or variables".into());
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("a quote isn't closed".into());
    }
    if started {
        words.push(word);
    }
    if words.is_empty() {
        return Err("no command".into());
    }
    Ok(words)
}

/// Removes the agent from this device: its record and every install.
pub fn remove(agent: RegistryAgent) -> Result<(), HarnessError> {
    let Some(dir) = agent_dir(agent) else {
        return Ok(());
    };
    // The record first: the agent is gone even if a running build keeps
    // its files busy until it exits.
    let _ = std::fs::remove_file(dir.join("agent.json"));
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            tracing::warn!(target: "reno_harness::acp_registry", "{} left files behind: {e}", dir.display());
            Ok(())
        }
    }
}

/// Installs `entry`'s build for this device and records it as added (or
/// updated: the new version in its own folder, the record moved to it, the
/// old ones removed). `icon_svg` is kept with the record.
pub async fn install(entry: &Entry, icon_svg: Option<String>) -> Result<Record, HarnessError> {
    let agent = RegistryAgent::new(&entry.id)
        .ok_or_else(|| HarnessError::Install(format!("{:?} isn't a registry id", entry.id)))?;
    if built_in(&entry.id).is_some() {
        return Err(HarnessError::Install(format!("{} is built in", entry.name)));
    }
    if !valid(entry) {
        return Err(HarnessError::Install(format!(
            "{}'s registry entry isn't usable",
            entry.name
        )));
    }
    let choice = choose(entry).ok_or_else(|| {
        HarnessError::NotInstalled(format!("{} has no build for this device", entry.name))
    })?;
    let dir = agent_dir(agent).ok_or_else(|| {
        HarnessError::Install("no data directory for agents on this device".into())
    })?;
    std::fs::create_dir_all(&dir)?;
    let _guard = crate::adapter_install::install_lock().lock().await;
    let version_dir = dir.join(safe_component(&entry.version));
    // A uv tool's launchers name their own folder (absolute links and
    // shebangs), so it installs in place; the rest stage and move in.
    let in_place = choice.kind() == Kind::Uvx;
    let tmp = if in_place {
        version_dir.clone()
    } else {
        dir.join(format!(
            ".tmp-{}-{}",
            safe_component(&entry.version),
            std::process::id()
        ))
    };
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let staged = stage(entry, &choice, &tmp).await;
    let (relative, verified) = match staged {
        Ok(found) => found,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };
    std::fs::write(tmp.join(MARKER), &entry.version)?;
    if !in_place {
        if version_dir.exists() {
            let _ = std::fs::remove_dir_all(&version_dir);
        }
        std::fs::rename(&tmp, &version_dir)?;
    }
    let (args, env) = match &choice {
        Choice::Binary(t) => (t.args.clone(), t.env.clone()),
        Choice::Npx(p) | Choice::Uvx(p) => (p.args.clone(), p.env.clone()),
    };
    let record = Record {
        id: entry.id.clone(),
        name: entry.name.clone(),
        version: entry.version.clone(),
        kind: choice.kind(),
        description: entry.description.clone(),
        authors: entry.authors.clone(),
        program: version_dir.join(relative),
        args,
        env,
        icon_svg,
        verified,
    };
    let previous = record_in(&dir).map(|r| r.program);
    write_record(&dir, &record)?;
    // Older versions go, but the one just replaced stays until the next
    // update, and so does any a chat is still running on (updates don't wait
    // for chats): it may load more of its files.
    if let Ok(children) = std::fs::read_dir(&dir) {
        let running = crate::acp::running_command_lines();
        for child in children.filter_map(Result::ok) {
            let path = child.path();
            let kept = path == version_dir
                || version_in_use(&path, running.as_deref())
                || previous.as_ref().is_some_and(|p| p.starts_with(&path));
            if path.is_dir() && !kept && !child.file_name().to_string_lossy().starts_with('.') {
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
    Ok(record)
}

/// Whether a process runs from this version's folder. If the processes can't be
/// listed it counts as in use: keeping a folder costs less than pulling it out
/// from under a chat.
fn version_in_use(version_dir: &Path, running: Option<&[String]>) -> bool {
    running.is_none_or(|lines| {
        let dir = version_dir.to_string_lossy();
        lines.iter().any(|line| line.contains(&*dir))
    })
}

fn safe_component(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_start_matches('.')
        .to_owned()
}

fn write_record(dir: &Path, record: &Record) -> Result<(), HarnessError> {
    let tmp = dir.join(format!(".agent.json.{}", std::process::id()));
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(
        &serde_json::to_vec_pretty(record).map_err(|e| HarnessError::Install(e.to_string()))?,
    )?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, dir.join("agent.json"))?;
    Ok(())
}

/// Installs the choice into `tmp`: the entry relative to it, and whether a
/// published checksum was matched.
async fn stage(
    entry: &Entry,
    choice: &Choice<'_>,
    tmp: &Path,
) -> Result<(PathBuf, bool), HarnessError> {
    match choice {
        Choice::Binary(t) => {
            let download = tmp.join(".download");
            download_checked(&t.archive, t.sha256.as_deref(), &download, &entry.name).await?;
            let cmd = relative_cmd(&t.cmd).ok_or_else(|| {
                HarnessError::Install(format!("{}'s command leaves its archive", entry.name))
            })?;
            let url = t
                .archive
                .split(['?', '#'])
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let (from, to) = (download.clone(), tmp.to_owned());
            let cmd_name = cmd.clone();
            tokio::task::spawn_blocking(move || extract(&url, &from, &to, &cmd_name))
                .await
                .map_err(|e| HarnessError::Install(format!("extraction task failed: {e}")))??;
            let _ = std::fs::remove_file(&download);
            let program = tmp.join(&cmd);
            let inside = program
                .canonicalize()
                .ok()
                .zip(tmp.canonicalize().ok())
                .is_some_and(|(p, root)| p.starts_with(root));
            if !program.is_file() || !inside {
                return Err(HarnessError::Install(format!(
                    "{}'s download has no {} in it",
                    entry.name, t.cmd
                )));
            }
            make_executable(&program)?;
            Ok((cmd, t.sha256.is_some()))
        }
        Choice::Npx(p) => {
            let (name, version) = npm_pin(&p.package)
                .ok_or_else(|| HarnessError::Install(format!("{} isn't pinned", p.package)))?;
            let npm = crate::adapter_install::find_npm().ok_or_else(|| {
                HarnessError::NotInstalled(format!("Node.js (npm), to install {}", entry.name))
            })?;
            // (Bounded: one per version a device ever installs.)
            let pin = crate::adapter_install::NpmPin {
                name: Box::leak(name.to_owned().into_boxed_str()),
                version: Box::leak(version.to_owned().into_boxed_str()),
            };
            let cache = agents_root()
                .map(|r| r.join(".npm-cache"))
                .unwrap_or_else(|| tmp.join(".npm-cache"));
            crate::adapter_install::install_into(&npm, &pin, tmp, &cache, &entry.name, true)
                .await?;
            let bin = name.rsplit('/').next().unwrap_or(name);
            let entry_path =
                crate::adapter_install::bin_entry(tmp, &pin, bin).ok_or_else(|| {
                    HarnessError::Install(format!("{} has nothing to run", p.package))
                })?;
            let relative = entry_path
                .strip_prefix(tmp)
                .map(Path::to_path_buf)
                .map_err(|_| {
                    HarnessError::Install(format!("{}'s entry leaves its install", p.package))
                })?;
            Ok((relative, false))
        }
        Choice::Uvx(p) => {
            let (name, _) = uv_pin(&p.package)
                .ok_or_else(|| HarnessError::Install(format!("{} isn't pinned", p.package)))?;
            let uv = find_uv().ok_or_else(|| {
                HarnessError::NotInstalled(format!("uv, to install {}", entry.name))
            })?;
            let root = agents_root().unwrap_or_else(|| tmp.join(".uv"));
            uv_tool_install(&uv, &p.package, tmp, &root, &entry.name).await?;
            let bin = tmp.join("bin");
            let named = bin.join(name);
            let program = if named.is_file() {
                named
            } else {
                let mut found: Vec<PathBuf> = std::fs::read_dir(&bin)
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.is_file() || p.is_symlink())
                    .collect();
                found.sort();
                if found.len() != 1 {
                    return Err(HarnessError::Install(format!(
                        "{} installed no single command to run",
                        p.package
                    )));
                }
                found.remove(0)
            };
            Ok((
                program
                    .strip_prefix(tmp)
                    .map(Path::to_path_buf)
                    .unwrap_or(program),
                false,
            ))
        }
    }
}

/// The Python a uv install falls back to when the machine's default cannot build the package's
/// dependencies from wheels (`UV_NO_BUILD`): 3.12, which the audited packages (fast-agent) had
/// wheels for.
const UV_FALLBACK_PYTHON: &str = "3.12";

/// `uv tool install`s `package` into `tmp` (its `tools` and `bin`). Reno's uv installs depend on
/// nothing of the machine's but uv itself:
///
/// 1. uv's own pick first (the machine's default Python, or whatever uv manages), and
/// 2. when that fails (a dependency with no wheel for that Python, which `UV_NO_BUILD` refuses to
///    build: building would run the package's build code), again with a pinned
///    [`UV_FALLBACK_PYTHON`] that uv downloads and manages.
///
/// uv's cache and its managed Pythons live under the agents `root` (`.uv-cache`, `.uv-python`,
/// shared by every uv agent and kept when one is removed), never in `~/.cache/uv` or
/// `~/.local/share/uv`.
async fn uv_tool_install(
    uv: &Path,
    package: &str,
    tmp: &Path,
    root: &Path,
    name: &str,
) -> Result<(), HarnessError> {
    let run = |python: Option<&'static str>| {
        let mut cmd = crate::process::Command::new(uv);
        cmd.args(["tool", "install", "--force", "--quiet"]);
        if let Some(python) = python {
            cmd.args(["--python", python]);
        }
        cmd.arg(package)
            .env("UV_TOOL_DIR", tmp.join("tools"))
            .env("UV_TOOL_BIN_DIR", tmp.join("bin"))
            .env("UV_CACHE_DIR", root.join(".uv-cache"))
            .env("UV_PYTHON_INSTALL_DIR", root.join(".uv-python"))
            // Wheels only: a package's build step is code Reno never runs (as npm's
            // `--ignore-scripts`).
            .env("UV_NO_BUILD", "1")
            .stdin(crate::process::Stdio::null())
            .stdout(crate::process::Stdio::piped())
            .stderr(crate::process::Stdio::piped())
            .kill_on_drop(true);
        crate::compose_child_path(&mut cmd, uv);
        async move {
            let output = tokio::time::timeout(UV_TIMEOUT, cmd.output())
                .await
                .map_err(|_| {
                    HarnessError::Install(format!("uv took too long to install {name}"))
                })??;
            if output.status.success() {
                return Ok(());
            }
            let err = String::from_utf8_lossy(&output.stderr);
            Err(HarnessError::Install(format!(
                "uv couldn't install {package}: {}",
                err.trim().lines().last().unwrap_or("no reason given")
            )))
        }
    };
    match run(None).await {
        Ok(()) => Ok(()),
        Err(first @ HarnessError::Install(_)) if !first.to_string().contains("took too long") => {
            tracing::info!(
                target: "reno_harness::acp_registry",
                "{package} did not install with uv's default Python ({first}); trying Python {UV_FALLBACK_PYTHON}"
            );
            let _ = std::fs::remove_dir_all(tmp.join("tools"));
            let _ = std::fs::remove_dir_all(tmp.join("bin"));
            run(Some(UV_FALLBACK_PYTHON)).await.map_err(|second| {
                HarnessError::Install(format!("{second} (with uv's default Python: {first})"))
            })
        }
        Err(other) => Err(other),
    }
}

/// Downloads `url` to `dest`, checking it against `sha256` when given.
async fn download_checked(
    url: &str,
    sha256: Option<&str>,
    dest: &Path,
    name: &str,
) -> Result<(), HarnessError> {
    let failed = |detail: String| {
        HarnessError::Install(format!("{name}'s download didn't finish: {detail}"))
    };
    if !safe_url(url) {
        return Err(failed("not an https address".into()));
    }
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| failed(e.to_string()))?;
    let response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| failed(e.to_string()))?;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_ARCHIVE_BYTES)
    {
        return Err(failed("it's larger than Reno takes".into()));
    }
    let mut body = response.bytes_stream();
    let mut file = tokio::fs::File::create(dest).await?;
    let mut received = 0_u64;
    let mut digest = Sha256::new();
    loop {
        let chunk = tokio::time::timeout(STALL_TIMEOUT, body.next())
            .await
            .map_err(|_| failed(format!("no data for {}s", STALL_TIMEOUT.as_secs())))?;
        match chunk {
            Some(Ok(bytes)) => {
                received = received.saturating_add(bytes.len() as u64);
                if received > MAX_ARCHIVE_BYTES {
                    return Err(failed("it's larger than Reno takes".into()));
                }
                digest.update(&bytes);
                file.write_all(&bytes).await?;
            }
            Some(Err(e)) => return Err(failed(e.to_string())),
            None => break,
        }
    }
    file.flush().await?;
    let actual = format!("{:x}", digest.finalize());
    if let Some(expected) = sha256
        && !actual.eq_ignore_ascii_case(expected)
    {
        return Err(HarnessError::Install(format!(
            "{name}'s download doesn't match the registry's checksum (expected {expected}, got {actual})"
        )));
    }
    Ok(())
}

/// Unpacks `archive` (by its URL's suffix) into `dest`; a URL with no
/// archive suffix is the executable itself.
fn extract(url: &str, archive: &Path, dest: &Path, cmd: &Path) -> Result<(), HarnessError> {
    let file = || std::fs::File::open(archive);
    if url.ends_with(".zip") {
        return extract_zip(archive, dest);
    }
    if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
        return extract_tar(flate2::read::GzDecoder::new(file()?), dest);
    }
    if url.ends_with(".tar.bz2") || url.ends_with(".tbz2") || url.ends_with(".tbz") {
        return extract_tar(bzip2::read::BzDecoder::new(file()?), dest);
    }
    if url.ends_with(".tar") {
        return extract_tar(file()?, dest);
    }
    if url.ends_with(".tar.xz")
        || url.ends_with(".txz")
        || url.ends_with(".dmg")
        || url.ends_with(".pkg")
        || url.ends_with(".msi")
    {
        return Err(HarnessError::Install(
            "the build comes in a format Reno can't unpack".into(),
        ));
    }
    // A raw executable, named as the registry's `cmd`.
    let target = dest.join(cmd);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(archive, &target).or_else(|_| std::fs::copy(archive, &target).map(|_| ()))?;
    Ok(())
}

fn enclosed(dest: &Path, name: &Path) -> Result<PathBuf, HarnessError> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in name.components() {
        match part {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => {
                return Err(HarnessError::Install(format!(
                    "the archive has an unsafe path ({})",
                    name.display()
                )));
            }
        }
    }
    Ok(dest.join(out))
}

/// Whether `path`'s folder really is under `dest`, however links on the way resolve: a file or
/// folder never lands outside it through a link an archive made earlier.
fn inside(dest: &Path, path: &Path) -> Result<(), HarnessError> {
    folder_inside(dest, path.parent().unwrap_or(path))
}

/// [`inside`] for a folder itself.
fn folder_inside(dest: &Path, folder: &Path) -> Result<(), HarnessError> {
    if folder.canonicalize()?.starts_with(dest.canonicalize()?) {
        Ok(())
    } else {
        Err(HarnessError::Install(format!(
            "the archive writes outside its folder ({})",
            folder.display()
        )))
    }
}

/// A link at `output` to `target`, a relative path that goes up (`..`) only before it goes
/// down, and never above `dest`, counted from where the link really sits.
#[cfg(unix)]
fn symlink_inside(dest: &Path, output: &Path, target: &Path) -> Result<(), HarnessError> {
    use std::path::Component;
    let refused = || {
        HarnessError::Install(format!(
            "the archive has a link that points out ({} -> {})",
            output.display(),
            target.display()
        ))
    };
    inside(dest, output)?;
    let root = dest.canonicalize()?;
    let folder = output.parent().unwrap_or(output).canonicalize()?;
    let depth = folder
        .strip_prefix(&root)
        .map_err(|_| refused())?
        .components()
        .count();
    let (mut up, mut seen_name) = (0_usize, false);
    for part in target.components() {
        match part {
            Component::ParentDir if !seen_name => up += 1,
            Component::Normal(_) => seen_name = true,
            Component::CurDir => {}
            _ => return Err(refused()),
        }
    }
    if up > depth {
        return Err(refused());
    }
    std::os::unix::fs::symlink(target, output)?;
    Ok(())
}

#[cfg(not(unix))]
fn symlink_inside(_: &Path, output: &Path, _: &Path) -> Result<(), HarnessError> {
    Err(HarnessError::Install(format!(
        "the archive has a link or special file ({})",
        output.display()
    )))
}

fn extract_tar(reader: impl std::io::Read, dest: &Path) -> Result<(), HarnessError> {
    let bad = |e: std::io::Error| HarnessError::Install(format!("the archive is damaged: {e}"));
    let mut archive = tar::Archive::new(reader);
    let mut total = 0_u64;
    for (count, entry) in archive.entries().map_err(bad)?.enumerate() {
        if count >= MAX_ARCHIVE_ENTRIES {
            return Err(HarnessError::Install(
                "the archive has too many files".into(),
            ));
        }
        let mut entry = entry.map_err(bad)?;
        let path = entry.path().map_err(bad)?.into_owned();
        let output = enclosed(dest, &path)?;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(&output)?;
            folder_inside(dest, &output)?;
            continue;
        }
        if kind.is_pax_global_extensions()
            || kind.is_pax_local_extensions()
            || kind.is_gnu_longname()
        {
            continue;
        }
        if kind.is_symlink() {
            // Builds link among their own files; a link that points out never lands.
            let target = entry
                .link_name()
                .map_err(bad)?
                .map(std::borrow::Cow::into_owned);
            if let Some(target) = target
                && let Some(parent) = output.parent()
            {
                std::fs::create_dir_all(parent)?;
                symlink_inside(dest, &output, &target)?;
                continue;
            }
        }
        if !kind.is_file() {
            // Hard links and special files never land.
            return Err(HarnessError::Install(format!(
                "the archive has a link or special file ({})",
                path.display()
            )));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_EXTRACTED_BYTES {
            return Err(HarnessError::Install(
                "the archive unpacks larger than Reno takes".into(),
            ));
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        inside(dest, &output)?;
        let mut out = std::fs::File::create(&output)?;
        std::io::copy(&mut entry, &mut out)?;
        #[cfg(unix)]
        if let Ok(mode) = entry.header().mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &output,
                std::fs::Permissions::from_mode(mode & 0o755 | 0o600),
            );
        }
    }
    Ok(())
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<(), HarnessError> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?)
        .map_err(|e| HarnessError::Install(format!("the archive is damaged: {e}")))?;
    if zip.len() > MAX_ARCHIVE_ENTRIES {
        return Err(HarnessError::Install(
            "the archive has too many files".into(),
        ));
    }
    let mut total = 0_u64;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| HarnessError::Install(format!("the archive is damaged: {e}")))?;
        let name = entry
            .enclosed_name()
            .ok_or_else(|| HarnessError::Install("the archive has an unsafe path".into()))?;
        let output = enclosed(dest, &name)?;
        if let Some(mode) = entry.unix_mode()
            && !matches!(mode & 0o170000, 0 | 0o040000 | 0o100000)
        {
            return Err(HarnessError::Install(format!(
                "the archive has a link or special file ({})",
                name.display()
            )));
        }
        if entry.is_dir() {
            std::fs::create_dir_all(&output)?;
            continue;
        }
        total = total.saturating_add(entry.size());
        if total > MAX_EXTRACTED_BYTES {
            return Err(HarnessError::Install(
                "the archive unpacks larger than Reno takes".into(),
            ));
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&output)?;
        std::io::copy(&mut entry, &mut out)?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &output,
                std::fs::Permissions::from_mode(mode & 0o755 | 0o600),
            );
        }
    }
    Ok(())
}

fn make_executable(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(permissions.mode() | 0o755);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

/// Fetches the index (`url`, normally [`INDEX_URL`]): its bytes, and the
/// ETag to ask with next time. `None` bytes: unchanged since `etag`.
pub async fn fetch_index(
    url: &str,
    etag: Option<&str>,
) -> Result<(Option<Vec<u8>>, Option<String>), String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let mut request = client.get(url);
    if let Some(etag) = etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok((None, etag));
    }
    let response = response.error_for_status().map_err(|e| e.to_string())?;
    let bytes = read_capped(response, MAX_INDEX_BYTES).await?;
    Ok((Some(bytes), etag))
}

/// Fetches an agent's icon: only from the registry's own CDN, only a small
/// SVG.
pub async fn fetch_icon(url: &str) -> Option<String> {
    let test_registry = std::env::var("RENO_ACP_REGISTRY_LOOPBACK").is_ok_and(|v| v == "1");
    if !(url.starts_with(ICON_ORIGIN) || test_registry) || !safe_url(url) {
        return None;
    }
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let response = client.get(url).send().await.ok()?.error_for_status().ok()?;
    let bytes = read_capped(response, MAX_ICON_BYTES).await.ok()?;
    let text = String::from_utf8(bytes).ok()?;
    clean_svg(&text)
}

/// An SVG that draws and nothing more: no scripts, handlers, foreign
/// content or outside references.
pub fn clean_svg(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let svg = lower.contains("<svg")
        && ![
            "<script",
            "javascript:",
            "<foreignobject",
            " on",
            "href=\"http",
            "href='http",
            "<!entity",
            "<image",
        ]
        .iter()
        .any(|bad| {
            if *bad == " on" {
                // Event handlers: ` onload=`, ` onclick=`, …
                lower.match_indices(" on").any(|(i, _)| {
                    lower[i + 3..]
                        .bytes()
                        .take_while(u8::is_ascii_alphabetic)
                        .count()
                        > 0
                        && lower[i + 3..]
                            .trim_start_matches(|c: char| c.is_ascii_alphabetic())
                            .starts_with('=')
                })
            } else {
                lower.contains(bad)
            }
        });
    (svg && text.len() <= MAX_ICON_BYTES).then(|| text.to_owned())
}

async fn read_capped(response: reqwest::Response, max: usize) -> Result<Vec<u8>, String> {
    if response.content_length().is_some_and(|n| n as usize > max) {
        return Err("too large".into());
    }
    let mut body = response.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        if out.len() + chunk.len() > max {
            return Err("too large".into());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

// ---- the catalog: the index with a cache, and what a client is shown of it

/// How long a fetched index is used as it is (the CDN's own max-age).
pub const INDEX_TTL: Duration = Duration::from_secs(5 * 60);
/// How long an icon that could not be fetched is not asked for again.
const ICON_RETRY: Duration = Duration::from_secs(60 * 60);

/// The index as loaded: its agents and where they came from.
#[derive(Debug, Clone)]
pub struct LoadedIndex {
    pub entries: Vec<Entry>,
    /// When it was fetched (ms since the Unix epoch).
    pub fetched_at_ms: u64,
    /// This is the cached copy because the fetch failed; `error` says why.
    pub offline: bool,
    pub error: Option<String>,
}

fn index_dir() -> Option<PathBuf> {
    agents_root().map(|root| root.join(".index"))
}

fn ms_of(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// The index from `url` (normally [`INDEX_URL`]), through a cache in `<agents root>/.index`:
/// a copy younger than `ttl` is used as it is (unless `refresh`); an older one is revalidated
/// with its ETag; when the fetch fails the cached copy is served and marked `offline`; with no
/// cached copy a failed fetch is an error. Nothing here polls: it runs when a client asks.
pub async fn load_index(url: &str, refresh: bool, ttl: Duration) -> Result<LoadedIndex, String> {
    let dir = index_dir().ok_or("no data directory for the registry cache")?;
    let (file, etag_file) = (dir.join("index.json"), dir.join("etag"));
    let cached = std::fs::read(&file).ok().and_then(|bytes| {
        let at = std::fs::metadata(&file).and_then(|m| m.modified()).ok()?;
        parse_index(&bytes).ok().map(|entries| (entries, bytes, at))
    });
    if let Some((entries, _, at)) = &cached
        && !refresh
        && std::time::SystemTime::now()
            .duration_since(*at)
            .is_ok_and(|age| age < ttl)
    {
        return Ok(LoadedIndex {
            entries: entries.clone(),
            fetched_at_ms: ms_of(*at),
            offline: false,
            error: None,
        });
    }
    let etag = cached
        .as_ref()
        .and_then(|_| std::fs::read_to_string(&etag_file).ok());
    let fetched = fetch_index(url, etag.as_deref().map(str::trim)).await;
    let now = std::time::SystemTime::now();
    let _ = std::fs::create_dir_all(&dir);
    let failure = match fetched {
        Ok((Some(bytes), new_etag)) => match parse_index(&bytes) {
            Ok(entries) => {
                let _ = write_private(&file, &bytes);
                match new_etag {
                    Some(etag) => {
                        let _ = write_private(&etag_file, etag.as_bytes());
                    }
                    None => {
                        let _ = std::fs::remove_file(&etag_file);
                    }
                }
                return Ok(LoadedIndex {
                    entries,
                    fetched_at_ms: ms_of(now),
                    offline: false,
                    error: None,
                });
            }
            Err(e) => e,
        },
        // Unchanged: the cached copy is current as of now.
        Ok((None, _)) => match &cached {
            Some((entries, bytes, _)) => {
                let _ = write_private(&file, bytes);
                return Ok(LoadedIndex {
                    entries: entries.clone(),
                    fetched_at_ms: ms_of(now),
                    offline: false,
                    error: None,
                });
            }
            None => "the registry said nothing changed, with nothing cached".to_owned(),
        },
        Err(e) => e,
    };
    match cached {
        Some((entries, _, at)) => Ok(LoadedIndex {
            entries,
            fetched_at_ms: ms_of(at),
            offline: true,
            error: Some(failure),
        }),
        None => Err(format!("the registry index could not be fetched: {failure}")),
    }
}

/// The cleaned SVG icon of `entry`, from the cache beside the index or fetched once (only from
/// the registry's own CDN). `None` when it has none or it could not be fetched; a failure is not
/// retried for an hour.
pub async fn icon_of(entry: &Entry) -> Option<String> {
    let url = entry.icon.as_deref()?;
    let dir = index_dir()?.join("icons");
    let key = format!("{:x}", Sha256::digest(url.as_bytes()));
    let name = format!("{}-{}", safe_component(&entry.id), &key[..12]);
    let (hit, miss) = (dir.join(format!("{name}.svg")), dir.join(format!("{name}.none")));
    if let Ok(svg) = std::fs::read_to_string(&hit) {
        return clean_svg(&svg);
    }
    if std::fs::metadata(&miss)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|at| std::time::SystemTime::now().duration_since(at).ok())
        .is_some_and(|age| age < ICON_RETRY)
    {
        return None;
    }
    let _ = std::fs::create_dir_all(&dir);
    match fetch_icon(url).await {
        Some(svg) => {
            let _ = write_private(&hit, svg.as_bytes());
            Some(svg)
        }
        None => {
            let _ = write_private(&miss, b"");
            None
        }
    }
}

/// The device's answer to "what would adding this need": the readiness for a platform, with Node.js
/// and uv present or not (a pure function: [`readiness`] asks the machine).
pub fn readiness_for(entry: &Entry, platform: Option<&str>, node: bool, uv: bool) -> Readiness {
    match choose_for(entry, platform) {
        None => Readiness::NoBuild,
        Some(Choice::Binary(_)) => Readiness::Ready,
        Some(Choice::Npx(_)) if node => Readiness::Ready,
        Some(Choice::Npx(_)) => Readiness::NeedsNode,
        Some(Choice::Uvx(_)) if uv => Readiness::Ready,
        Some(Choice::Uvx(_)) => Readiness::NeedsUv,
    }
}

/// What the person does about a readiness other than ready.
pub fn fix_for(readiness: Readiness) -> Option<&'static str> {
    match readiness {
        Readiness::Ready => None,
        Readiness::NeedsNode => Some("brew install node"),
        Readiness::NeedsUv => Some("brew install uv"),
        Readiness::NoBuild => None,
    }
}

/// The built-in executor id of a harness slug `BUILT_IN` names: the three that Reno runs through
/// its ACP driver carry the `acp.` prefix.
pub fn built_in_executor(harness: &str) -> String {
    match harness {
        "devin" | "grok" | "antigravity" => format!("acp.{harness}"),
        other => other.to_owned(),
    }
}

/// An entry as clients are shown it, for a platform and what the device has. `record` is what is
/// added of it here, `icon` its cleaned SVG.
pub fn entry_view(
    entry: &Entry,
    record: Option<&Record>,
    icon: Option<String>,
    platform: Option<&str>,
    node: bool,
    uv: bool,
) -> Option<reno_proto::RegistryEntry> {
    use reno_proto::{RegistryAdded, RegistryDistribution, RegistryReadiness};
    let choice = choose_for(entry, platform);
    let readiness = readiness_for(entry, platform, node, uv);
    Some(reno_proto::RegistryEntry {
        id: entry.id.clone(),
        executor: format!("acp.{}", entry.id).parse().ok()?,
        name: entry.name.clone(),
        version: entry.version.clone(),
        description: entry.description.clone(),
        authors: entry.authors.clone(),
        license: entry.license.clone(),
        website: entry.website.clone(),
        repository: entry.repository.clone(),
        icon,
        kind: choice.as_ref().map(|c| match c {
            Choice::Binary(_) => RegistryDistribution::Binary,
            Choice::Npx(_) => RegistryDistribution::Npx,
            Choice::Uvx(_) => RegistryDistribution::Uvx,
        }),
        readiness: match readiness {
            Readiness::Ready => RegistryReadiness::Ready,
            Readiness::NeedsNode => RegistryReadiness::NeedsNode,
            Readiness::NeedsUv => RegistryReadiness::NeedsUv,
            Readiness::NoBuild => RegistryReadiness::NoBuild,
        },
        fix: fix_for(readiness).map(str::to_owned),
        host: match &choice {
            Some(Choice::Binary(t)) => reqwest::Url::parse(&t.archive)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned)),
            _ => None,
        },
        checksum: match &choice {
            Some(Choice::Binary(t)) => Some(t.sha256.is_some()),
            _ => None,
        },
        built_in: built_in(&entry.id)
            .and_then(|slug| built_in_executor(slug).parse().ok()),
        added: record.map(|r| RegistryAdded {
            version: r.version.clone(),
            update_available: r.version != entry.version,
        }),
        adding: false,
        delisted: false,
        own: false,
    })
}

/// An agent added here that the index does not list (delisted), or one of your own.
pub fn record_view(record: &Record) -> Option<reno_proto::RegistryEntry> {
    use reno_proto::{RegistryAdded, RegistryDistribution, RegistryReadiness};
    let own = record.kind == Kind::Own;
    Some(reno_proto::RegistryEntry {
        id: record.id.clone(),
        executor: format!("acp.{}", record.id).parse().ok()?,
        name: record.name.clone(),
        version: record.version.clone(),
        description: record.description.clone(),
        authors: record.authors.clone(),
        license: None,
        website: None,
        repository: None,
        icon: record.icon_svg.clone(),
        kind: match record.kind {
            Kind::Binary => Some(RegistryDistribution::Binary),
            Kind::Npx => Some(RegistryDistribution::Npx),
            Kind::Uvx => Some(RegistryDistribution::Uvx),
            Kind::Own => None,
        },
        readiness: RegistryReadiness::Ready,
        fix: None,
        host: None,
        checksum: None,
        built_in: None,
        added: Some(RegistryAdded {
            version: record.version.clone(),
            update_available: false,
        }),
        adding: false,
        delisted: !own,
        own,
    })
}

/// Adds an agent of your own: its `command` (a program and its arguments) as `own.<slug of name>`,
/// with `env` (`KEY=VALUE` lines). The program must exist (on PATH or by its path); Reno never
/// installs or updates it.
pub fn add_own(name: &str, command: &str, env: &str) -> Result<Record, HarnessError> {
    let id = own_id(name)
        .ok_or_else(|| HarnessError::Install("name the agent with letters or digits".into()))?;
    let words = split_command(command).map_err(HarnessError::Install)?;
    let program = &words[0];
    let path = PathBuf::from(program);
    let resolved = if path.is_absolute() || program.contains('/') {
        path.is_file().then_some(path)
    } else {
        crate::executable::find_on_paths(program, Vec::new())
    }
    .ok_or_else(|| HarnessError::NotInstalled(format!("{program} isn't a program on this device")))?;
    let agent = RegistryAgent::new(&id)
        .ok_or_else(|| HarnessError::Install(format!("{id:?} isn't usable as an id")))?;
    parse_env(env).map_err(HarnessError::Install)?;
    let dir = agent_dir(agent)
        .ok_or_else(|| HarnessError::Install("no data directory for agents".into()))?;
    if record_in(&dir).is_some() {
        return Err(HarnessError::Install(format!(
            "an agent named {name} is already added"
        )));
    }
    std::fs::create_dir_all(&dir)?;
    let record = Record {
        id,
        name: name.trim().to_owned(),
        version: "0.0.0".into(),
        kind: Kind::Own,
        description: String::new(),
        authors: Vec::new(),
        program: resolved,
        args: words[1..].to_vec(),
        env: BTreeMap::new(),
        icon_svg: None,
        verified: false,
    };
    write_record(&dir, &record)?;
    set_user_env(agent, env)?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> serde_json::Value {
        serde_json::json!({
            "version": "1.0.0",
            "agents": [
                {
                    "id": "gemini", "name": "Gemini CLI", "version": "0.66.0",
                    "description": "Google's official CLI for Gemini", "authors": ["Google"],
                    "license": "Apache-2.0", "license_url": "https://example.com/l",
                    "distribution": { "npx": { "package": "@google/gemini-cli@0.66.0", "args": ["--acp"] } },
                    "icon": "https://cdn.agentclientprotocol.com/registry/v1/latest/gemini.svg"
                },
                {
                    "id": "goose", "name": "goose", "version": "1.57.0", "description": "x",
                    "distribution": { "binary": {
                        "darwin-aarch64": { "archive": "https://github.com/b/g.tar.bz2", "cmd": "./goose", "args": ["acp"], "sha256": "2fbafa227ab28654c47065f1c9ebcd12f527f017e30c08e618bc6fcf363d7935" },
                        "linux-x86_64": { "archive": "https://github.com/b/g.tar.gz", "cmd": "./goose", "args": ["acp"] },
                        "windows-x86_64": { "archive": "https://github.com/b/g.zip", "cmd": "./goose-package\\goose.exe" }
                    } }
                },
                { "id": "fast-agent", "name": "fast-agent", "version": "0.10.1", "description": "y",
                  "distribution": { "uvx": { "package": "fast-agent-acp==0.10.1", "args": ["-x"], "env": { "FAST_AGENT_MODEL": "codexplan" } } } },
                // Dropped, each for its own reason; the rest still list.
                { "id": "Bad", "name": "x", "version": "1", "distribution": { "npx": { "package": "a@1.0.0" } } },
                { "id": "plain-http", "name": "x", "version": "1", "distribution": { "binary": { "linux-x86_64": { "archive": "http://x.dev/a.zip", "cmd": "a" } } } },
                { "id": "unpinned", "name": "x", "version": "1", "distribution": { "npx": { "package": "left-pad@latest" } } },
                { "id": "escapes", "name": "x", "version": "1", "distribution": { "binary": { "linux-x86_64": { "archive": "https://x.dev/a.zip", "cmd": "../../bin/sh" } } } },
                { "id": "no-dist", "name": "x", "version": "1", "distribution": {} },
                { "id": "gemini", "name": "Twice", "version": "1", "distribution": { "npx": { "package": "a@1.0.0" } } },
                "not an object"
            ],
            "extensions": []
        })
    }

    /// Updates don't wait for chats, so a chat can be several versions behind.
    #[test]
    fn a_version_a_chat_runs_from_is_kept() {
        let dir = std::path::Path::new("/data/acp-registry/agent/1.0.0");
        let running = |line: &str| vec![line.to_owned()];
        assert!(version_in_use(
            dir,
            Some(&running("/data/acp-registry/agent/1.0.0/bin/agent --acp"))
        ));
        assert!(!version_in_use(
            dir,
            Some(&running("/data/acp-registry/agent/2.0.0/bin/agent --acp"))
        ));
        assert!(!version_in_use(dir, Some(&[])));
        // Can't tell: keep it.
        assert!(version_in_use(dir, None));
    }

    /// The registry publishes musl builds (`linux-x86_64-musl`) beside the glibc ones, and may
    /// name platforms Reno doesn't run: an entry keeps listing for the platforms it can use.
    #[test]
    fn an_entry_with_platforms_this_device_doesnt_know_still_lists() {
        let entry = |platform: &str| {
            serde_json::json!({ "agents": [{
                "id": "multi", "name": "Multi", "version": "1.0.0", "description": "x",
                "distribution": { "binary": {
                    "darwin-aarch64": { "archive": "https://x.dev/a.tar.gz", "cmd": "./a" },
                    platform: { "archive": "https://x.dev/b.tar.gz", "cmd": "./b" },
                } }
            }] })
        };
        for platform in ["linux-x86_64-musl", "linux-aarch64-musl", "freebsd-x86_64"] {
            let entries = parse_index(&serde_json::to_vec(&entry(platform)).unwrap()).unwrap();
            assert_eq!(entries.len(), 1, "{platform} dropped the entry");
        }
        // A key that isn't a platform name at all is still bad data.
        for platform in ["../etc", "Linux X86", ""] {
            let entries = parse_index(&serde_json::to_vec(&entry(platform)).unwrap()).unwrap();
            assert!(entries.is_empty(), "{platform:?} was accepted");
        }
    }

    #[test]
    fn the_index_keeps_every_entry_that_holds_up() {
        let entries = parse_index(&serde_json::to_vec(&index()).unwrap()).unwrap();
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["gemini", "goose", "fast-agent"]);
        assert_eq!(entries[0].name, "Gemini CLI");
        assert!(parse_index(b"{\"agents\": 3}").is_err());
        assert!(parse_index(&vec![b' '; MAX_INDEX_BYTES + 1]).is_err());
    }

    #[test]
    fn a_device_takes_its_own_build_first_then_npm_then_uv() {
        let entries = parse_index(&serde_json::to_vec(&index()).unwrap()).unwrap();
        let goose = &entries[1];
        assert!(
            matches!(choose_for(goose, Some("darwin-aarch64")), Some(Choice::Binary(t)) if t.sha256.is_some())
        );
        assert_eq!(choose_for(goose, Some("linux-aarch64")), None);
        assert!(matches!(
            choose_for(&entries[0], Some("linux-aarch64")),
            Some(Choice::Npx(_))
        ));
        assert!(
            matches!(choose_for(&entries[2], None), Some(Choice::Uvx(p)) if p.env["FAST_AGENT_MODEL"] == "codexplan")
        );
    }

    #[test]
    fn pins_are_exact_versions() {
        assert_eq!(
            npm_pin("@google/gemini-cli@0.66.0"),
            Some(("@google/gemini-cli", "0.66.0"))
        );
        assert_eq!(npm_pin("cline@3.0.74"), Some(("cline", "3.0.74")));
        assert_eq!(npm_pin("cline@^3"), None);
        assert_eq!(npm_pin("cline"), None);
        assert_eq!(npm_pin("../x@1.0.0"), None);
        assert_eq!(
            uv_pin("fast-agent-acp==0.10.1"),
            Some(("fast-agent-acp", "0.10.1"))
        );
        assert_eq!(uv_pin("fast-agent-acp>=0.10"), None);
        assert_eq!(
            uv_pin("minion-code@0.1.44"),
            Some(("minion-code", "0.1.44"))
        );
    }

    #[test]
    fn environments_and_commands_read_as_a_shell_would() {
        let env = parse_env("# keys\nOPENAI_API_KEY=sk-1\nexport MODEL=\"gpt 5\"\n\n").unwrap();
        assert_eq!(env["OPENAI_API_KEY"], "sk-1");
        assert_eq!(env["MODEL"], "gpt 5");
        assert!(parse_env("not a pair").is_err());
        assert!(parse_env("1X=y").is_err());
        assert_eq!(env_text(&env), "MODEL=gpt 5\nOPENAI_API_KEY=sk-1\n");
        assert_eq!(
            split_command("~/bin/agent --acp 'a b' c\\ d").unwrap(),
            ["~/bin/agent", "--acp", "a b", "c d"]
        );
        assert!(split_command("agent | tee").is_err());
        assert!(split_command("agent 'open").is_err());
        assert_eq!(
            own_id("Halcyon agent").as_deref(),
            Some("own.halcyon-agent")
        );
        assert_eq!(own_id("  ").as_deref(), None);
    }

    #[test]
    fn built_in_agents_are_known_by_their_registry_ids() {
        assert_eq!(built_in("claude-acp"), Some("claude-code"));
        assert_eq!(built_in("grok-build"), Some("grok"));
        // Pi has a native driver: the registry's adapter copy is not offered beside it.
        assert_eq!(built_in("pi-acp"), Some("pi"));
        // So has Muse Code: the registry's `muse-code` copy is not offered beside it.
        assert_eq!(built_in("muse-code"), Some("muse"));
        // Likewise Cursor: its ACP surface is lossy, and Reno drives its SDK.
        assert_eq!(built_in("cursor"), Some("cursor"));
        // Neither registry copy of opencode is offered beside the native driver.
        assert_eq!(built_in("opencode"), Some("opencode"));
        assert_eq!(built_in("opencode-v2"), Some("opencode"));
        assert_eq!(built_in("gemini"), None);
    }

    #[test]
    fn icons_draw_and_do_nothing_else() {
        assert!(
            clean_svg(
                "<svg viewBox=\"0 0 16 16\"><path fill=\"currentColor\" d=\"M0 0h16v16z\"/></svg>"
            )
            .is_some()
        );
        assert!(clean_svg("<svg><script>alert(1)</script></svg>").is_none());
        assert!(clean_svg("<svg onload=\"x()\"></svg>").is_none());
        assert!(clean_svg("<svg><image href=\"http://x\"/></svg>").is_none());
        assert!(clean_svg("<svg> one two </svg>").is_some());
        assert!(clean_svg("not svg").is_none());
    }

    #[tokio::test]
    async fn icons_come_only_from_the_registrys_cdn_and_small() {
        // (Refused before any request: another host, plain http.)
        if std::env::var("RENO_ACP_REGISTRY_LOOPBACK").is_err() {
            assert!(fetch_icon("https://example.com/gemini.svg").await.is_none());
            assert!(
                fetch_icon("http://cdn.agentclientprotocol.com/registry/v1/latest/gemini.svg")
                    .await
                    .is_none()
            );
            assert!(
                fetch_icon("https://cdn.agentclientprotocol.com.example.com/gemini.svg")
                    .await
                    .is_none()
            );
        }
        // Too big to be an icon.
        let big = format!("<svg>{}</svg>", " ".repeat(MAX_ICON_BYTES));
        assert!(clean_svg(&big).is_none());
    }

    fn tar_of(entries: &[(&str, Option<&str>)]) -> Vec<u8> {
        let mut built = tar::Builder::new(Vec::new());
        for (name, link) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o755);
            if name.ends_with('/') {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_cksum();
                built.append_data(&mut header, name, &b""[..]).unwrap();
                continue;
            }
            match link {
                Some(target) => {
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_size(0);
                    header.set_cksum();
                    built.append_link(&mut header, name, target).unwrap();
                }
                None => {
                    header.set_size(4);
                    header.set_cksum();
                    built.append_data(&mut header, name, &b"#!/x"[..]).unwrap();
                }
            }
        }
        built.into_inner().unwrap()
    }

    /// Real builds ship links among their own files (harn's `harn-dap -> harn`, a Python
    /// framework's `Versions/Current -> 3.12`): those land; one that points out never does.
    #[test]
    fn tar_links_that_stay_inside_the_archive_unpack_and_the_rest_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ok = tar_of(&[
            // (Archives often start with their own folder.)
            ("./", None),
            ("harn", None),
            ("harn-dap", Some("harn")),
            ("fw/Versions/3.12/Py", None),
            ("fw/Versions/Current", Some("3.12")),
            ("fw/Py", Some("Versions/Current/Py")),
            ("fw/Versions/3.12/up", Some("../../Versions/Current/Py")),
        ]);
        extract_tar(&ok[..], dir.path()).unwrap();
        assert_eq!(std::fs::read(dir.path().join("harn-dap")).unwrap(), b"#!/x");
        assert_eq!(std::fs::read(dir.path().join("fw/Py")).unwrap(), b"#!/x");
        assert!(dir.path().join("fw/Versions/3.12/up").is_file());

        for (what, bad) in [
            ("absolute", tar_of(&[("l", Some("/etc/passwd"))])),
            ("above the root", tar_of(&[("l", Some("../x"))])),
            ("above the root, deep", tar_of(&[("a/l", Some("../../x"))])),
            ("a .. after a name", tar_of(&[("l", Some("a/../../x"))])),
            // `a` is the root itself, so `a/b/l` really sits one level down.
            (
                "through a link",
                tar_of(&[("a", Some(".")), ("a/b/l", Some("../../x"))]),
            ),
            (
                "a file through a link out",
                tar_of(&[("a", Some("..")), ("a/x", None)]),
            ),
        ] {
            let out = tempfile::tempdir().unwrap();
            let dest = out.path().join("dest");
            std::fs::create_dir(&dest).unwrap();
            assert!(extract_tar(&bad[..], &dest).is_err(), "{what} was accepted");
            assert!(!out.path().join("x").exists(), "{what} wrote outside");
        }
    }

    #[test]
    fn tar_archives_unpack_inside_and_refuse_links() {
        let dir = tempfile::tempdir().unwrap();
        let mut built = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o755);
        header.set_cksum();
        built
            .append_data(&mut header, "pkg/agent", &b"#!/x"[..])
            .unwrap();
        let bytes = built.into_inner().unwrap();
        extract_tar(&bytes[..], dir.path()).unwrap();
        assert!(dir.path().join("pkg/agent").is_file());

        let mut linked = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_cksum();
        linked
            .append_link(&mut header, "evil", "/etc/passwd")
            .unwrap();
        let bytes = linked.into_inner().unwrap();
        assert!(extract_tar(&bytes[..], dir.path()).is_err());
        assert!(enclosed(dir.path(), Path::new("../x")).is_err());
        assert!(enclosed(dir.path(), Path::new("/etc/x")).is_err());
        assert_eq!(
            relative_cmd("./goose-package\\goose.exe"),
            Some(PathBuf::from("goose-package/goose.exe"))
        );
        assert_eq!(relative_cmd("../sh"), None);
        assert_eq!(relative_cmd("C:/x"), None);
    }

    /// A fake `uv` that records how it was run and, like uv under `UV_NO_BUILD`, fails for the
    /// machine's default Python (no wheel) and installs once a Python is pinned.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_uv_install_falls_back_to_a_pinned_python_kept_under_the_agents_root() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (root, tmp) = (dir.path().join("agents"), dir.path().join("tmp"));
        std::fs::create_dir_all(&tmp).unwrap();
        let log = dir.path().join("uv.log");
        let uv = dir.path().join("uv");
        std::fs::write(
            &uv,
            format!(
                "#!/bin/sh\necho \"$*|$UV_PYTHON_INSTALL_DIR|$UV_CACHE_DIR|$UV_NO_BUILD\" >> '{}'\n\
                 case \"$*\" in *\"--python 3.12\"*) mkdir -p \"$UV_TOOL_BIN_DIR\"; : > \"$UV_TOOL_BIN_DIR/fake-tool\"; exit 0;; esac\n\
                 echo 'error: no wheel for the default Python' >&2\nexit 1\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&uv, std::fs::Permissions::from_mode(0o755)).unwrap();

        uv_tool_install(&uv, "fake-tool==1.0.0", &tmp, &root, "Fake")
            .await
            .unwrap();

        let calls: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(
            !calls[0].contains("--python"),
            "uv's own pick first: {calls:?}"
        );
        assert!(calls[1].contains("--python 3.12"), "{calls:?}");
        for call in &calls {
            let parts: Vec<&str> = call.split('|').collect();
            assert_eq!(
                parts[1],
                root.join(".uv-python").to_str().unwrap(),
                "{call}"
            );
            assert_eq!(parts[2], root.join(".uv-cache").to_str().unwrap(), "{call}");
            assert_eq!(parts[3], "1", "no source builds: {call}");
        }
        assert!(tmp.join("bin/fake-tool").is_file());
    }

    /// When the default Python works there is no second try, and no Python is pinned.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_uv_install_that_works_the_first_time_pins_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let log = dir.path().join("uv.log");
        let uv = dir.path().join("uv");
        std::fs::write(
            &uv,
            format!("#!/bin/sh\necho \"$*\" >> '{}'\nexit 0\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&uv, std::fs::Permissions::from_mode(0o755)).unwrap();
        uv_tool_install(&uv, "fake-tool==1.0.0", &tmp, dir.path(), "Fake")
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
    }

    fn entry_with(distribution: serde_json::Value) -> Entry {
        let index = serde_json::json!({ "agents": [{
            "id": "demo", "name": "Demo", "version": "2.0.0", "description": "d",
            "distribution": distribution
        }] });
        parse_index(&serde_json::to_vec(&index).unwrap())
            .unwrap()
            .remove(0)
    }

    #[test]
    fn the_view_says_what_the_device_lacks_and_how_to_fix_it() {
        use reno_proto::{RegistryDistribution, RegistryReadiness};
        let uv = entry_with(serde_json::json!({ "uvx": { "package": "demo==2.0.0" } }));
        let view = entry_view(&uv, None, None, Some("darwin-aarch64"), true, false).unwrap();
        assert_eq!(view.kind, Some(RegistryDistribution::Uvx));
        assert_eq!(view.readiness, RegistryReadiness::NeedsUv);
        assert_eq!(view.fix.as_deref(), Some("brew install uv"));
        assert_eq!((view.checksum, view.host.clone()), (None, None));
        let ready = entry_view(&uv, None, None, Some("darwin-aarch64"), false, true).unwrap();
        assert_eq!(ready.readiness, RegistryReadiness::Ready);

        let npx = entry_with(serde_json::json!({ "npx": { "package": "demo@2.0.0" } }));
        let view = entry_view(&npx, None, None, None, false, true).unwrap();
        assert_eq!(view.readiness, RegistryReadiness::NeedsNode);
        assert_eq!(view.fix.as_deref(), Some("brew install node"));

        let other = entry_with(serde_json::json!({ "binary": { "linux-x86_64": {
            "archive": "https://example.com/a.tar.gz", "cmd": "./demo" } } }));
        let view = entry_view(&other, None, None, Some("darwin-aarch64"), true, true).unwrap();
        assert_eq!(view.readiness, RegistryReadiness::NoBuild);
        assert_eq!(view.kind, None);
    }

    #[test]
    fn a_binary_says_where_it_downloads_from_and_whether_a_checksum_is_published() {
        let sha = "a".repeat(64);
        let binary = |sha256: Option<&str>| {
            let mut target = serde_json::json!({
                "archive": "https://downloads.example.com/demo.tar.gz", "cmd": "./demo" });
            if let Some(sha) = sha256 {
                target["sha256"] = sha.into();
            }
            entry_with(serde_json::json!({ "binary": { "darwin-aarch64": target } }))
        };
        let view =
            entry_view(&binary(Some(&sha)), None, None, Some("darwin-aarch64"), true, true).unwrap();
        assert_eq!(view.host.as_deref(), Some("downloads.example.com"));
        assert_eq!(view.checksum, Some(true));
        let view = entry_view(&binary(None), None, None, Some("darwin-aarch64"), true, true).unwrap();
        assert_eq!(view.checksum, Some(false));
    }

    #[test]
    fn added_says_which_version_and_whether_the_index_differs() {
        let entry = entry_with(serde_json::json!({ "npx": { "package": "demo@2.0.0" } }));
        let record = |version: &str| -> Record {
            serde_json::from_value(serde_json::json!({
                "id": "demo", "name": "Demo", "version": version, "kind": "npx", "program": "/x"
            }))
            .unwrap()
        };
        let view = entry_view(&entry, Some(&record("1.0.0")), None, None, true, true).unwrap();
        let added = view.added.unwrap();
        assert_eq!(added.version, "1.0.0");
        assert!(added.update_available);
        let view = entry_view(&entry, Some(&record("2.0.0")), None, None, true, true).unwrap();
        assert!(!view.added.unwrap().update_available);
    }

    #[test]
    fn an_agent_reno_drives_itself_is_listed_with_its_executor() {
        let make = |id: &str| {
            let index = serde_json::json!({ "agents": [{
                "id": id, "name": id, "version": "1.0.0",
                "distribution": { "npx": { "package": "x@1.0.0" } }
            }] });
            parse_index(&serde_json::to_vec(&index).unwrap())
                .unwrap()
                .remove(0)
        };
        let view = |id: &str| entry_view(&make(id), None, None, None, true, true).unwrap();
        assert_eq!(view("claude-acp").built_in.unwrap().as_str(), "claude-code");
        assert_eq!(view("devin").built_in.unwrap().as_str(), "acp.devin");
        assert_eq!(view("opencode-v2").built_in.unwrap().as_str(), "opencode");
        assert_eq!(view("goose").built_in, None);
    }

    #[test]
    fn a_record_the_index_lacks_is_delisted_and_one_of_your_own_is_not() {
        let record = |id: &str, kind: &str| -> Record {
            serde_json::from_value(serde_json::json!({
                "id": id, "name": id, "version": "1.0.0", "kind": kind, "program": "/x"
            }))
            .unwrap()
        };
        let gone = record_view(&record("gone", "binary")).unwrap();
        assert!(gone.delisted && !gone.own);
        let mine = record_view(&record("own.mine", "own")).unwrap();
        assert!(mine.own && !mine.delisted);
        assert_eq!(mine.kind, None);
    }
}
