//! Protected paths, enforced after the fact where the harness cannot be asked.
//!
//! The built-in protection (`reno_proto::defs::protect`) refuses a write to `.claude`, `.codex`,
//! `.reno`, `.mcp.json` or git internals when the harness **asks** first. A harness that cannot
//! ask (Grok, Devin and Hermes, answered in-driver, and Pi, which asks nothing), or a run whose edits are accepted without
//! a question, would write them unseen. So the supervisor also looks at what the run did.
//!
//! # Two detectors
//!
//! 1. **The worktree** (a run with a Reno-managed worktree). After a tool call that could have
//!    written (anything but a read or a network call) and again when the run ends, the
//!    `Workspace` port reports the protected paths that differ from the run's base commit
//!    (`git diff` against the base, plus untracked and ignored files). This is the truth: it
//!    sees a shell redirect, a script and a rename as well as a file tool, and it cannot be
//!    fooled by a denied request, because a denied write changes nothing.
//! 2. **The tool call** (a run that works in place, where there is no base to compare with). A
//!    tool call that names a protected path (`reno_proto::defs::protected_path_of_call`) and
//!    whose result says it succeeded. A call that was refused by the approval step fails, so a
//!    supervised run is unaffected.
//!
//! # What happens
//!
//! The paths are reverted in the run's worktree (never in a run that works in place: nothing is
//! deleted there, the run is stopped and the paths are reported), the audit trail gets a denied
//! approval row and a `NeedsApproval`/`ApprovalResolved` pair in the journal (decided by
//! `built-in protected paths`, so `reno approval history` and the transcript show "blocked: ...
//! is protected"), and the run is cancelled. It ends `interrupted` with
//! `StopDetail::ProtectedPathWrite`, even if its harness was about to finish normally.
//! A scan that cannot run (git failed) is logged and the run goes on: the scan at its end is
//! the last look.

use reno_proto::defs::{PROTECTED_RULE, is_protected_relative, protected_path_of_call};
use reno_proto::{
    ActionClass, ApprovalOutcome, ApprovalSubject, ExecEvent, ResolvedBy, StopDetail, ToolCall,
    WorkspaceInfo, WorkspaceStatus,
};
use reno_store::NewApproval;

use super::Supervisor;

/// The most open tool calls remembered for the checks; beyond it the oldest knowledge is
/// dropped (a harness that never reports results must not grow this without bound).
const MAX_OPEN_TOOLS: usize = 512;

/// What the supervisor remembers of a tool call until its result comes.
#[derive(Debug)]
pub(super) struct OpenTool {
    class: ActionClass,
    /// The protected path the call names, if it does.
    protected: Option<String>,
}

/// The run's protected-path violations so far.
#[derive(Debug)]
pub(super) struct Blocked {
    paths: Vec<String>,
    reverted: bool,
}

impl Blocked {
    pub(super) fn detail(&self) -> StopDetail {
        StopDetail::ProtectedPathWrite {
            paths: self.paths.clone(),
            reverted: self.reverted,
        }
    }
}

/// Whether `path`, as a call wrote it, is outside the run's worktree. A relative path is the
/// run's own.
fn outside(workspace: &WorkspaceInfo, path: &str) -> bool {
    let path = std::path::Path::new(path);
    path.is_absolute() && !path.starts_with(&workspace.path)
}

impl Supervisor {
    /// A tool call began: remember what the result will need.
    pub(super) fn note_tool(&mut self, id: &str, call: &ToolCall) {
        if self.open_tools.len() >= MAX_OPEN_TOOLS {
            self.open_tools.clear();
        }
        self.open_tools.insert(
            id.to_owned(),
            OpenTool {
                class: call.class,
                protected: protected_path_of_call(&self.info.cwd, call),
            },
        );
    }

    /// A tool call finished (its result is already journaled): look for a protected write.
    pub(super) async fn tool_finished(&mut self, id: &str, ok: bool) {
        let Some(tool) = self.open_tools.remove(id) else {
            return;
        };
        if self.lost || self.blocked.is_some() {
            return;
        }
        let named = tool.protected.filter(|_| ok);
        match self.workspace_of_run().await {
            Some(workspace) => {
                if !matches!(tool.class, ActionClass::Read | ActionClass::Network)
                    && self.check_worktree(&workspace).await
                {
                    return;
                }
                // A write the worktree cannot show: a path outside it (the owner's own
                // `~/.claude`, a repository's git hooks, Reno's data directory).
                if let Some(path) = named.filter(|path| outside(&workspace, path)) {
                    self.block(vec![path], false).await;
                }
            }
            // No base to compare with: the call's own word, once it succeeded.
            None => {
                if let Some(path) = named {
                    self.block(vec![path], false).await;
                }
            }
        }
    }

    /// The run is over (or being ended): the last look at its worktree.
    pub(super) async fn last_look(&mut self) {
        if self.lost {
            return;
        }
        if let Some(workspace) = self.workspace_of_run().await {
            self.check_worktree(&workspace).await;
        }
    }

    /// The run's worktree, when it has an open one; `None` for a run that works in place.
    async fn workspace_of_run(&mut self) -> Option<WorkspaceInfo> {
        self.shared.workspace.as_ref()?;
        if self.workspace.is_none() {
            let found = match self.shared.db.get_workspace(&self.run).await {
                Ok(found) => found,
                Err(error) => {
                    tracing::warn!(run = %self.run, %error, "could not read the run's workspace");
                    return None;
                }
            };
            self.workspace = Some(found);
        }
        self.workspace
            .clone()
            .flatten()
            .filter(|info| info.status == WorkspaceStatus::Active)
    }

    /// Looks for protected changes in the worktree; reverts and blocks when there are some.
    /// Returns whether it found any.
    async fn check_worktree(&mut self, workspace: &WorkspaceInfo) -> bool {
        let Some(port) = self.shared.workspace.clone() else {
            return false;
        };
        let found = match port.protected_changes(workspace).await {
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(run = %self.run, %error, "could not look for protected paths");
                return false;
            }
        };
        // The port reports what it saw; this side decides what is protected.
        let paths: Vec<String> = found
            .into_iter()
            .filter(|path| is_protected_relative(path))
            .collect();
        if paths.is_empty() {
            return false;
        }
        // On record first (ARCH-9: intent before effect), then undone.
        self.audit_blocked(&paths).await;
        let reverted = match port.revert_protected(workspace, &paths).await {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(run = %self.run, %error, "could not revert protected paths");
                false
            }
        };
        self.stop_blocked(paths, reverted);
        true
    }

    /// Record a violation the worktree cannot undo and stop the run.
    async fn block(&mut self, paths: Vec<String>, reverted: bool) {
        self.audit_blocked(&paths).await;
        self.stop_blocked(paths, reverted);
    }

    fn stop_blocked(&mut self, paths: Vec<String>, reverted: bool) {
        tracing::warn!(run = %self.run, ?paths, reverted, "stopping the run: it wrote a protected path");
        match self.blocked.as_mut() {
            Some(blocked) => {
                blocked.paths.extend(paths);
                blocked.reverted &= reverted;
            }
            None => self.blocked = Some(Blocked { paths, reverted }),
        }
        self.cancel.cancel();
    }

    async fn audit_blocked(&mut self, paths: &[String]) {
        for path in paths {
            self.audit_one(path).await;
        }
    }

    /// A denied approval, on record and in the journal, so the history and the transcript say
    /// what happened in the words a denied request would have had.
    async fn audit_one(&mut self, path: &str) {
        self.protected_seq += 1;
        let request_id = format!("protected-{}", self.protected_seq);
        let subject = ApprovalSubject {
            action: ActionClass::Edit,
            tool: "reno:protected-path-check".to_owned(),
            command: None,
            paths: vec![path.into()],
            raw: serde_json::json!({ "detected": "after the fact" }),
        };
        let message = format!(
            "blocked: `{path}` is protected (git internals, agent and Reno configuration cannot \
             be changed by a run); the run was stopped"
        );
        let by = ResolvedBy::Rule {
            name: PROTECTED_RULE.to_owned(),
        };
        let recorded = match self.mint() {
            Ok(id) => {
                let new = NewApproval {
                    id,
                    run_id: self.run.clone(),
                    request_id: request_id.clone(),
                    subject: subject.clone(),
                    expires_at: self.shared.db.now_ms(),
                };
                self.shared
                    .db
                    .record_approval(new, ApprovalOutcome::Denied, by.clone(), self.fence)
                    .await
                    .map_err(|error| error.to_string())
            }
            Err(why) => Err(why),
        };
        if let Err(why) = recorded {
            tracing::error!(run = %self.run, %why, "could not record a protected-path block");
        }
        self.publish(&ExecEvent::NeedsApproval {
            request_id: request_id.clone(),
            subject,
        });
        self.publish(&ExecEvent::ApprovalResolved {
            request_id,
            decision: ApprovalOutcome::Denied,
            by,
            message: Some(message),
        });
    }
}
