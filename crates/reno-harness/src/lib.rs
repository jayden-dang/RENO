//! reno-harness: Reno's executors, one per agent CLI, behind the kernel's [`exec::Executor`] port.
//!
//! Each driver speaks its agent's own wire directly and emits `ExecEvent`s: Claude Code over
//! stream-json ([`ClaudeExecutor`]), Codex over the `codex app-server` JSON-RPC
//! ([`CodexExecutor`]) and, through the shared [`AcpExecutor`], every agent built ground-up on
//! ACP: Devin (`devin acp`), Grok (`grok agent stdio`), Hermes (`hermes acp`), Antigravity and
//! the agents of the ACP registry. Adapter-mediated ACP for Claude and Codex is not used: the
//! adapters hold prompt turns open for background work the CLIs themselves settle eagerly, which
//! manufactures done-status bugs the native wires do not have. [`exec::MockExecutor`] plays
//! scripted runs for kernel tests and the CLI's `--executor mock`.
//!
//! Layout: `exec/` is the executor port and its shared pieces; `wire/` is the sans-IO layer
//! (frame types, normalizers, permission mapping, name tables; ARCH-6); `drivers/` holds the I/O
//! shells that spawn and pump each harness; `jsonrpc` is the JSON-RPC client Codex and ACP share;
//! `proc/` owns process lifecycle, executable lookup, installs, signatures, shell environment and
//! scratch space; `agent_id` names the ACP agents and the harness kinds the drivers special-case.

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("harness binary not found: {0}")]
    NotInstalled(String),
    #[error("harness protocol error: {0}")]
    Protocol(String),
    /// A managed adapter install (npm) failed; carries npm's own output so
    /// the cause is diagnosable from the chat error alone.
    #[error("adapter install failed: {0}")]
    Install(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The host has no login for this run's provider (a Cloud device with
    /// nothing connected for it). The message says where to sign in.
    #[error("{0}")]
    NotSignedIn(String),
}

pub mod acp_registry;
mod agent_id;
pub mod child_env;
pub use agent_id::{HarnessId, REGISTRY_SLUG_PREFIX, RegistryAgent};
mod drivers;
pub use drivers::{acp, claude, codex, pi};
#[cfg(all(feature = "conformance", unix))]
pub mod conformance;
pub mod exec;
pub(crate) mod jsonrpc;
pub(crate) mod lossy_lines;
pub mod proc;
pub(crate) mod redact;
pub mod usage;
#[cfg(unix)]
pub use proc::{
    DEFAULT_SWEEP_GRACE, ProcessRecord, SkipReason, SweepReport, sweep_orphans,
    sweep_orphans_with_grace,
};
pub(crate) use proc::{adapter_install, code_signature, executable};
pub use proc::{archive_install, process, shell_env};
pub use redact::{Held, MIN_SECRET_VALUE_LEN, ValueRedactor};
pub(crate) mod wire;

/// Compose the child's PATH: the resolved executable's directory first, then
/// the user's own PATH (the login shell's), then this process's — deduped.
/// npm-shim CLIs are `#!/usr/bin/env node` scripts whose `node` lives beside
/// them in the version manager's bin dir, and the CLIs themselves shell out
/// to tools (git, rg, node) that a GUI/service launch's own PATH may lack.
/// Also hands the child the login shell's TLS, SSH-agent and locale
/// variables when this process has none of its own.
pub fn compose_child_path(cmd: &mut process::Command, exe: &std::path::Path) {
    compose_path(
        cmd.as_std_mut(),
        exe.parent().filter(|d| !d.as_os_str().is_empty()),
    );
    apply_login_shell_env(cmd.as_std_mut());
}

/// Apply the host-resolved run environment ([`RunRequest::env`] — credential
/// broker output on a Cloud device) to an agent child. Call it after the
/// driver's own env setup so the host's values win. Values are never logged.
pub fn apply_run_env(cmd: &mut process::Command, env: &std::collections::BTreeMap<String, String>) {
    for (key, value) in env {
        cmd.env(key, value);
    }
}

fn compose_path<'a>(
    cmd: &mut std::process::Command,
    executable_dir: impl IntoIterator<Item = &'a std::path::Path>,
) {
    let executable_dir: Vec<std::path::PathBuf> =
        executable_dir.into_iter().map(Into::into).collect();
    let own = std::env::var_os("PATH");
    let user = shell_env::user_path();
    let paths = child_path_entries(executable_dir, own.as_deref(), user.as_deref());
    if let Ok(joined) = std::env::join_paths(paths) {
        cmd.env("PATH", joined);
    }
}

/// The child PATH's entries in precedence order. The login shell's PATH goes
/// before our own only when ours is a bare system PATH: a Dock/launchd
/// launch inherits just that, whose `/usr/bin` stubs would otherwise shadow
/// the user's Homebrew or nvm toolchain. A PATH set up on purpose (a venv,
/// nix, direnv or nvm in the launching terminal, a service unit's own) keeps
/// precedence (see [`executable::is_bare_system_path`]).
fn child_path_entries(
    executable_dir: Vec<std::path::PathBuf>,
    own: Option<&std::ffi::OsStr>,
    user: Option<&std::ffi::OsStr>,
) -> Vec<std::path::PathBuf> {
    let split = |path: Option<&std::ffi::OsStr>| -> Vec<std::path::PathBuf> {
        path.map(|p| std::env::split_paths(p).collect())
            .unwrap_or_default()
    };
    let mut paths = executable_dir;
    if executable::is_bare_system_path(own) {
        paths.extend(split(user));
        paths.extend(split(own));
    } else {
        paths.extend(split(own));
        paths.extend(split(user));
    }
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| {
        let key = p
            .components()
            .collect::<std::path::PathBuf>()
            .to_string_lossy()
            .into_owned();
        !key.is_empty() && seen.insert(key)
    });
    paths
}

/// Login-shell variables (TLS trust, SSH agent, locale) the child lacks:
/// absent from our own environment and not set or removed by the driver.
fn apply_login_shell_env(cmd: &mut std::process::Command) {
    let defaults = shell_env::child_env_defaults();
    if defaults.is_empty() {
        return;
    }
    let explicit: Vec<std::ffi::OsString> = cmd.get_envs().map(|(k, _)| k.to_owned()).collect();
    for (key, value) in defaults {
        if !explicit.contains(&key) {
            cmd.env(key, value);
        }
    }
}

/// Rolling tail of a child's stderr, shared between the reader task and the
/// crash-message composer: an unexpected exit surfaces "<name> exited
/// unexpectedly (<status>): <last stderr lines>" instead of a bare shrug —
/// the proper background-crash message old reno showed (user requirement).
#[derive(Clone, Default)]
pub(crate) struct StderrTail(
    std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    std::sync::Arc<tokio::sync::Notify>,
    /// The run's own secret values, which never reach the tail or a log line.
    std::sync::Arc<ValueRedactor>,
);

impl StderrTail {
    /// A tail for the run whose environment is `env`: the values of what the host put there
    /// (a bot's secrets; not Reno's own `RENO_*` variables) are redacted from every line.
    pub(crate) fn for_run(env: &std::collections::BTreeMap<String, String>) -> Self {
        let secrets = env
            .iter()
            .filter(|(name, _)| !name.starts_with("RENO_"))
            .map(|(name, value)| (name.as_str(), value.as_str()));
        Self(
            Default::default(),
            Default::default(),
            std::sync::Arc::new(ValueRedactor::new(secrets)),
        )
    }

    /// `line` with the run's secret values replaced: what to log and what to keep.
    pub(crate) fn scrub<'a>(&self, line: &'a str) -> std::borrow::Cow<'a, str> {
        self.2.redact_str(line)
    }

    pub(crate) fn close(&self) {
        self.1.notify_one();
    }

    pub(crate) async fn wait_closed(&self) {
        let _ =
            tokio::time::timeout(std::time::Duration::from_millis(200), self.1.notified()).await;
    }

    const KEEP_LINES: usize = 6;
    const KEEP_BYTES: usize = 700;

    pub(crate) fn push(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let line = self.scrub(line);
        let line = line.as_ref();
        let mut tail = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tail.push_back(line.chars().take(Self::KEEP_BYTES).collect());
        while tail.len() > Self::KEEP_LINES {
            tail.pop_front();
        }
    }

    /// The captured tail as one display string, `None` when nothing arrived.
    pub(crate) fn snapshot(&self) -> Option<String> {
        let tail = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tail.is_empty() {
            return None;
        }
        let mut joined = tail.iter().cloned().collect::<Vec<_>>().join("\n");
        let mut start = joined.len().saturating_sub(Self::KEEP_BYTES * 2);
        while !joined.is_char_boundary(start) {
            start += 1;
        }
        joined.drain(..start);
        Some(joined)
    }
}

/// "exit code 137" / "signal 9 (killed)" / "unknown" — the status half of a
/// crash message, from a `try_wait` result after the stream ended.
pub(crate) fn describe_exit(status: Option<std::process::ExitStatus>) -> String {
    let Some(status) = status else {
        return "still running".into();
    };
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    "unknown exit".into()
}

/// Remove recognizable credentials at the boundary where diagnostics become UI text.
pub(crate) fn redact_secrets(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let markers = ["bearer ", "basic ", "sk-", "ghp_", "xox", "api_key="];
    let mut result = String::new();
    let mut offset = 0;
    while let Some((start, marker)) = markers
        .iter()
        .filter_map(|marker| {
            lower[offset..]
                .find(marker)
                .map(|at| (offset + at, *marker))
        })
        .min_by_key(|(at, _)| *at)
    {
        let credential = if marker.ends_with(' ') || marker.ends_with('=') {
            start + marker.len()
        } else {
            start
        };
        let credential = credential + text[credential..].len()
            - text[credential..]
                .trim_start_matches(|c: char| c.is_whitespace() || c == '\"' || c == '\'')
                .len();
        let end = text[credential..]
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '\"' | '\'' | ',' | ';' | '&' | '<' | '>')
            })
            .map_or(text.len(), |at| credential + at);
        result.push_str(&text[offset..credential]);
        result.push_str("[REDACTED]");
        // Empty credentials still advance past the marker.
        offset = end.max(start + marker.len());
    }
    result.push_str(&text[offset..]);
    result
}

#[cfg(test)]
#[test]
fn crash_diagnostics_redact_credentials_but_keep_context() {
    let raw = "request failed: Bearer secret-one Basic secret-two sk-private ghp-private ghp_private xoxp-private api_key=private&code=401 café";
    let clean = redact_secrets(raw);
    assert_eq!(
        clean,
        "request failed: Bearer [REDACTED] Basic [REDACTED] [REDACTED] ghp-private [REDACTED] [REDACTED] api_key=[REDACTED]&code=401 café"
    );
    assert_eq!(
        redact_secrets("Authorization: bEaReR token"),
        "Authorization: bEaReR [REDACTED]"
    );
    assert_eq!(
        redact_secrets("Bearer   hidden api_key=\"secret\""),
        "Bearer   [REDACTED] api_key=\"[REDACTED]\""
    );
    let tail = StderrTail::default();
    tail.push(raw);
    let message = crash_message("agent", None, &tail);
    assert!(message.ends_with(&clean));
    assert!(!message.contains("secret-one"));
}

/// The full crash message: status plus the stderr tail when there is one.
pub(crate) fn crash_message(
    name: &str,
    status: Option<std::process::ExitStatus>,
    stderr: &StderrTail,
) -> String {
    let status = describe_exit(status);
    match stderr.snapshot() {
        Some(tail) => format!(
            "{name} exited unexpectedly ({status}): {}",
            redact_secrets(&tail)
        ),
        None => format!("{name} exited unexpectedly ({status})"),
    }
}

pub use acp::AcpExecutor;
pub use claude::ClaudeExecutor;
pub use codex::CodexExecutor;
pub use pi::PiExecutor;

// ---------------------------------------------------------------------------
// Child lifecycle (shared by the codex and ACP harnesses)
// ---------------------------------------------------------------------------

/// End an agent runtime together with every process it started. `earlier`
/// is a descendant snapshot taken when the teardown began, in case the
/// agent exits before this one: its orphans can no longer be traced then.
pub(crate) async fn shutdown_agent(
    child: &mut process::Child,
    earlier: Vec<process::Tracked>,
    kill_grace: std::time::Duration,
) {
    let mut tree = earlier;
    if let Some(pid) = child.id() {
        for found in process::descendants(pid).await {
            if !tree.contains(&found) {
                tree.push(found);
            }
        }
    }
    shutdown_child(child, kill_grace).await;
    process::terminate_tree(&tree, kill_grace).await;
}

/// Reap the child: SIGTERM, then SIGKILL after `kill_grace`.
pub(crate) async fn shutdown_child(child: &mut process::Child, kill_grace: std::time::Duration) {
    let target = process::signal_target(child);
    if matches!(child.try_wait(), Ok(Some(_))) {
        if let Some(group) = target.filter(|pid| *pid < 0) {
            send_signal(&group, Signal::Kill);
        }
        return;
    }
    if let Some(pid) = target {
        send_signal(&pid, Signal::Term);
        if tokio::time::timeout(kill_grace, child.wait()).await.is_ok() {
            if pid < 0 {
                send_signal(&pid, Signal::Kill);
            }
            return;
        }
    }
    if let Some(pid) = target {
        send_signal(&pid, Signal::Kill);
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[derive(Clone, Copy)]
pub(crate) enum Signal {
    Term,
    Kill,
}

#[expect(
    unsafe_code,
    reason = "kill(2) has no safe std equivalent for process groups"
)]
pub(crate) fn send_signal(pid: &i32, signal: Signal) {
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: kill(2) targets an owned child or its private process group.
    // Negative targets include descendants after the group leader exits.
    unsafe {
        libc::kill(*pid, sig);
    }
}

#[cfg(test)]
mod path_tests {
    use super::child_path_entries;
    use std::path::PathBuf;

    fn joined(entries: &[&str]) -> std::ffi::OsString {
        std::env::join_paths(entries).unwrap()
    }

    #[test]
    fn unix_child_path_puts_the_login_shell_before_a_gui_launch_path() {
        let own = joined(&["/usr/bin", "/bin"]);
        let user = joined(&["/opt/homebrew/bin", "/usr/bin/", "/bin"]);
        let paths = child_path_entries(
            vec![PathBuf::from("/Users/a/.nvm/versions/node/v22/bin")],
            Some(&own),
            Some(&user),
        );
        assert_eq!(
            paths,
            [
                "/Users/a/.nvm/versions/node/v22/bin",
                "/opt/homebrew/bin",
                "/usr/bin/",
                "/bin"
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn unix_child_path_keeps_a_deliberate_process_path_first() {
        let own = joined(&["/home/a/project/.venv/bin", "/usr/bin", "/bin"]);
        let user = joined(&["/opt/homebrew/bin", "/usr/bin"]);
        let paths = child_path_entries(Vec::new(), Some(&own), Some(&user));
        assert_eq!(
            paths,
            [
                "/home/a/project/.venv/bin",
                "/usr/bin",
                "/bin",
                "/opt/homebrew/bin"
            ]
            .map(PathBuf::from)
        );
        // A service's default PATH is still a bare one.
        let systemd = joined(&[
            "/usr/local/sbin",
            "/usr/local/bin",
            "/usr/sbin",
            "/usr/bin",
            "/sbin",
            "/bin",
        ]);
        let paths = child_path_entries(Vec::new(), Some(&systemd), Some(&user));
        assert_eq!(paths[0], PathBuf::from("/opt/homebrew/bin"));
    }
}

#[cfg(test)]
mod stderr_tests {
    #[test]
    fn stderr_tail_truncates_at_utf8_boundaries() {
        let tail = super::StderrTail::default();
        tail.push(&"界".repeat(700));
        tail.push(&"界".repeat(700));
        tail.push("last stderr line");
        let snapshot = tail.snapshot().unwrap();
        assert!(snapshot.len() <= 1400);
        assert!(snapshot.ends_with("last stderr line"));
    }

    #[test]
    fn a_runs_secret_values_never_reach_its_stderr_tail_or_its_log_line() {
        let env = [
            (
                "GITHUB_TOKEN".to_owned(),
                "ghp_exampleExample1234".to_owned(),
            ),
            ("RENO_RUN_ID".to_owned(), "run_01HZX9".to_owned()),
        ]
        .into();
        let tail = super::StderrTail::for_run(&env);
        tail.push("fatal: auth failed for ghp_exampleExample1234 on run_01HZX9");
        let snapshot = tail.snapshot().unwrap();
        assert_eq!(
            snapshot,
            "fatal: auth failed for [secret:GITHUB_TOKEN] on run_01HZX9"
        );
        assert_eq!(
            tail.scrub("again ghp_exampleExample1234"),
            "again [secret:GITHUB_TOKEN]"
        );
    }
}
