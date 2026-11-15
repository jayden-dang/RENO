//! Process ownership, including descendants that outlive their agent.
//!
//! # Spawning
//!
//! This is the one place that puts a child in its own process group: [`lead_group`],
//! [`spawn_grouped`] and [`configure`] (which adds the Claude session-marker scrub). Every
//! driver spawn of an agent runtime or helper goes through it. Not through it, deliberately:
//! package-manager runs that complete with `.output()` (`npm_repair`, the registry's `uv`), which
//! have no long-lived owner to hold, and the short probes in `executable.rs` and `shell_env.rs`,
//! which already start their own group or session.
//!
//! # Stopping
//!
//! Stopping is group-aware wherever the child came from here, in two layers over one primitive
//! (`crate::send_signal`):
//!
//! - [`Child::shutdown`]: SIGTERM to the group, a grace period, SIGKILL to the group, then
//!   reap, plus every descendant traced while the child lived.
//! - `crate::shutdown_child` and `crate::shutdown_agent`, which Codex and ACP call on the
//!   underlying child: `process::signal_target` names the group for a group leader, so the same
//!   TERM, grace, KILL, reap sequence reaches the whole tree.

#![expect(
    unsafe_code,
    reason = "reaping and signalling an owned process group needs libc"
)]
use std::ops::{Deref, DerefMut};

use crate::process::{Child as ProcessChild, Command};

/// A private process group, and none of the markers a Claude Code session
/// leaves in its children's environment. Reno is often launched from one
/// (or runs inside one, on a Cloud machine): inherited, they make an agent's
/// own Claude believe it is nested in that session — it reports to its
/// socket, IDE port and session id, and takes its effort level. The user's
/// own configuration (`CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_USE_BEDROCK`/`VERTEX`,
/// `CLAUDE_CODE_GIT_BASH_PATH`, `ANTHROPIC_*`) is kept. Every Claude Reno
/// starts, directly or through an adapter, reports Reno as its entrypoint.
pub(crate) fn configure(command: &mut Command) {
    lead_group(command);
    for key in INHERITED_SESSION_MARKERS {
        command.env_remove(key);
    }
    command.env("CLAUDE_CODE_ENTRYPOINT", "reno");
}

/// Make the child the leader of its own process group, so a signal to `-pid` reaches everything
/// it starts that did not leave the group.
pub(crate) fn lead_group(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(not(unix))]
    let _ = command;
}

/// Spawn `command` as the leader of its own process group, owned by a [`Child`] that ends the
/// whole group on [`Child::shutdown`] and on drop.
pub(crate) fn spawn_grouped(command: &mut Command) -> std::io::Result<Child> {
    lead_group(command);
    command.spawn().map(Child::new)
}

const INHERITED_SESSION_MARKERS: [&str; 11] = [
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CLAUDE_AGENT_SDK_VERSION",
];

pub(crate) struct Child {
    inner: ProcessChild,
    #[cfg(unix)]
    group: Option<i32>,
}

impl Child {
    pub(crate) fn new(inner: ProcessChild) -> Self {
        Self {
            #[cfg(unix)]
            group: inner
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .map(|id| -id),
            inner,
        }
    }

    pub(crate) async fn shutdown(&mut self, grace: std::time::Duration) {
        self.shutdown_with(Vec::new(), grace).await;
    }

    /// [`Self::shutdown`], also ending `earlier`: descendants snapshotted
    /// while the agent was alive. Once it has exited and been reaped, its
    /// orphans can no longer be traced from it.
    pub(crate) async fn shutdown_with(
        &mut self,
        #[cfg_attr(not(unix), allow(unused_mut))] mut earlier: Vec<crate::process::Tracked>,
        grace: std::time::Duration,
    ) {
        // Everything the agent started, taken while it is still attached:
        // a command the agent ran in its own session (Devin does) is not in
        // the agent's process group and would outlive the runtime.
        #[cfg(unix)]
        let tree = {
            if let (Some(_), Some(pid)) = (self.group, self.inner.id()) {
                for found in crate::process::descendants(pid).await {
                    if !earlier.contains(&found) {
                        earlier.push(found);
                    }
                }
            }
            earlier
        };
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            crate::send_signal(&group, crate::Signal::Term);
            // Agents that run commands in detached groups clean them in their TERM handler.
            // Give descendants their grace even when the adapter exited first.
            let deadline = tokio::time::Instant::now() + grace;
            loop {
                let _ = self.inner.try_wait();
                // SAFETY: signal 0 only checks the private process group.
                if unsafe { libc::kill(group, 0) } != 0 {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    crate::send_signal(&group, crate::Signal::Kill);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        crate::shutdown_child(&mut self.inner, grace).await;
        #[cfg(unix)]
        crate::process::terminate_tree(&tree, grace).await;
    }

    /// The pid, group and start time to persist for this child, while it is unreaped.
    ///
    /// The native Claude executor reports it through `ExecControls::processes` as soon as it
    /// spawns; the run supervisor persists it.
    #[cfg(unix)]
    pub(crate) fn record(&self) -> Option<crate::proc::record::ProcessRecord> {
        crate::proc::record::ProcessRecord::of_pid(self.inner.id()?)
    }

    pub(crate) fn request_group_shutdown(&self) {
        #[cfg(unix)]
        if let Some(group) = self.group {
            crate::send_signal(&group, crate::Signal::Term);
        }
    }

    pub(crate) fn terminate_group(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            crate::send_signal(&group, crate::Signal::Kill);
        }
    }
}

impl Deref for Child {
    type Target = ProcessChild;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // Tokio's kill_on_drop covers the direct child. Keep the group id
        // even after wait() has reaped it, so descendants cannot escape cleanup.
        self.terminate_group();
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, reason = "test helpers unwrap freely")]
mod tests {
    use super::*;
    use crate::process::Stdio;

    #[test]
    fn acp_environment_removes_nested_claude_markers() {
        let mut command = Command::new("unused");
        configure(&mut command);
        let removed: Vec<_> = command
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_str().unwrap())
            .collect();
        assert_eq!(
            removed,
            vec![
                "CLAUDECODE",
                "CLAUDE_AGENT_SDK_VERSION",
                "CLAUDE_CODE_CHILD_SESSION",
                "CLAUDE_CODE_EXECPATH",
                "CLAUDE_CODE_MESSAGING_SOCKET",
                "CLAUDE_CODE_MESSAGING_TOKEN",
                "CLAUDE_CODE_SESSION_ATTENDED",
                "CLAUDE_CODE_SESSION_ID",
                "CLAUDE_CODE_SSE_PORT",
                "CLAUDE_EFFORT",
                "CLAUDE_PID",
            ]
        );
        assert_eq!(
            command
                .as_std()
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_ENTRYPOINT")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("reno"))
        );
        // The user's own configuration is never touched.
        for kept in [
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_GIT_BASH_PATH",
            "ANTHROPIC_API_KEY",
        ] {
            assert!(!removed.contains(&kept), "{kept}");
        }
    }

    /// A child that has exited and been reaped can no longer be traced:
    /// what was snapshotted while it ran is still ended.
    #[tokio::test]
    async fn shutdown_ends_an_earlier_snapshot_after_the_agent_exited() {
        use std::os::unix::process::CommandExt as _;
        let mut orphan = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let orphan_pid = orphan.id() as i32;
        let mut command = Command::new("true");
        configure(&mut command);
        let mut child = Child::new(command.spawn().unwrap());
        child.wait().await.unwrap();
        child
            .shutdown_with(
                vec![crate::process::Tracked::now(orphan_pid)],
                std::time::Duration::from_secs(2),
            )
            .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while orphan.try_wait().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "orphan survived");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Three levels: this shell (the child), a second shell, and its `sleep`. Each reports its
    /// pid through `dir` once it is running. `ignore_term` makes every level ignore SIGTERM, so
    /// only SIGKILL can end it (ignored signals survive `exec`).
    async fn spawn_tree(dir: &std::path::Path, ignore_term: bool) -> (Child, [i32; 3]) {
        let inner = dir.join("inner.sh");
        let trap = if ignore_term { "trap '' TERM\n" } else { "" };
        std::fs::write(
            &inner,
            format!(
                "{trap}echo $$ > \"$1/mid.tmp\" && mv \"$1/mid.tmp\" \"$1/mid\"\n\
                 sleep 60 &\n\
                 echo $! > \"$1/leaf.tmp\" && mv \"$1/leaf.tmp\" \"$1/leaf\"\n\
                 wait\n"
            ),
        )
        .unwrap();
        let outer = format!("{trap}sh \"$0/inner.sh\" \"$0\" & wait");
        let mut command = Command::new("sh");
        command.arg("-c").arg(outer).arg(dir).stdin(Stdio::null());
        let child = spawn_grouped(&mut command).unwrap();
        let top = child.id().unwrap() as i32;
        let read = |name: &str| {
            std::fs::read_to_string(dir.join(name))
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
        };
        let started = std::time::Instant::now();
        loop {
            if let (Some(mid), Some(leaf)) = (read("mid"), read("leaf")) {
                return (child, [top, mid, leaf]);
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the process tree did not start"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks for existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Time until every pid is gone, polling, or `None` after `limit`.
    async fn until_gone(pids: &[i32], limit: std::time::Duration) -> Option<std::time::Duration> {
        let started = std::time::Instant::now();
        while pids.iter().any(|&pid| alive(pid)) {
            if started.elapsed() > limit {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Some(started.elapsed())
    }

    /// The acceptance test for T0.6: terminating the group ends a shell, its child shell and
    /// that shell's `sleep`, all of them, within the grace plus a margin.
    #[tokio::test]
    async fn shutdown_ends_a_three_level_process_tree() {
        let dir = tempfile::tempdir().unwrap();
        let (mut child, pids) = spawn_tree(dir.path(), false).await;
        let group = child.record().unwrap();
        assert_eq!(group.pgid, pids[0], "the top shell leads the group");
        for pid in &pids[1..] {
            assert_eq!(
                crate::proc::record::ProcessRecord::of_pid(*pid as u32)
                    .unwrap()
                    .pgid,
                group.pgid,
                "pid {pid} is in the child's group"
            );
        }
        let grace = std::time::Duration::from_millis(500);
        let began = std::time::Instant::now();
        child.shutdown(grace).await;
        let gone = until_gone(&pids, std::time::Duration::from_millis(500)).await;
        let total = began.elapsed();
        eprintln!("tree kill (TERM sufficed): {total:?} for pids {pids:?}");
        assert!(gone.is_some(), "survivors among {pids:?}");
        assert!(
            total < grace + std::time::Duration::from_millis(500),
            "{total:?}"
        );
    }

    /// Levels that ignore SIGTERM are ended by the SIGKILL that follows the grace.
    #[tokio::test]
    async fn shutdown_escalates_to_sigkill_for_a_tree_that_ignores_term() {
        let dir = tempfile::tempdir().unwrap();
        let (mut child, pids) = spawn_tree(dir.path(), true).await;
        let grace = std::time::Duration::from_millis(400);
        let began = std::time::Instant::now();
        child.shutdown(grace).await;
        let gone = until_gone(&pids, std::time::Duration::from_millis(500)).await;
        let total = began.elapsed();
        eprintln!("tree kill (TERM ignored, KILL after grace): {total:?} for pids {pids:?}");
        assert!(gone.is_some(), "survivors among {pids:?}");
        assert!(
            total >= grace,
            "KILL cannot come before the grace: {total:?}"
        );
        assert!(
            total < grace + std::time::Duration::from_secs(1),
            "{total:?}"
        );
    }

    /// The path Codex and ACP use, `shutdown_agent` on the owned child, ends the same tree.
    #[tokio::test]
    async fn shutdown_agent_ends_a_three_level_process_tree() {
        for ignore_term in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (mut child, pids) = spawn_tree(dir.path(), ignore_term).await;
            let grace = std::time::Duration::from_millis(400);
            let began = std::time::Instant::now();
            crate::shutdown_agent(&mut child, Vec::new(), grace).await;
            let gone = until_gone(&pids, std::time::Duration::from_millis(500)).await;
            let total = began.elapsed();
            eprintln!("shutdown_agent (ignore TERM: {ignore_term}): {total:?} for pids {pids:?}");
            assert!(gone.is_some(), "survivors among {pids:?}");
            assert!(
                total < grace + std::time::Duration::from_millis(600),
                "{total:?}"
            );
        }
    }

    /// Dropping the owner without `shutdown` still ends the group.
    #[tokio::test]
    async fn dropping_the_child_ends_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let (child, pids) = spawn_tree(dir.path(), false).await;
        drop(child);
        assert!(
            until_gone(&pids, std::time::Duration::from_secs(2))
                .await
                .is_some(),
            "survivors among {pids:?}"
        );
    }
}
