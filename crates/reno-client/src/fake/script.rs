//! The scripted behaviour of the fake daemon: what a run does after `task.run`, and after an
//! approval is decided. Each step waits [`FakeConfig::step`](super::FakeConfig::step) on tokio
//! time, so tests with paused time control the pace exactly.

use std::sync::Arc;

use reno_proto::{ApprovalDecision, ExecEvent, RunId, RunStatus, StopReason};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::Shared;
use super::data::{self, Scenario};
use super::world::World;

/// One run's script: a cancel-aware sequence of paced steps.
struct Script {
    shared: Arc<Shared>,
    run: RunId,
    token: CancellationToken,
}

impl Script {
    /// A script for `run`, or `None` if the run is unknown.
    fn new(shared: Arc<Shared>, run: RunId) -> Option<Self> {
        let token = shared.read(|w| w.run(&run).map(|r| r.cancel.clone()))?;
        Some(Self { shared, run, token })
    }

    /// Waits one step, then applies `f`. False when the run was cancelled meanwhile or `f`
    /// declined, in which case the script stops.
    async fn step(&self, f: impl FnOnce(&mut World, &RunId) -> bool) -> bool {
        tokio::select! {
            biased;
            () = self.token.cancelled() => return false,
            () = tokio::time::sleep(self.shared.config.step) => {}
        }
        self.apply(f)
    }

    fn apply(&self, f: impl FnOnce(&mut World, &RunId) -> bool) -> bool {
        !self.token.is_cancelled() && self.shared.with(|w| f(w, &self.run))
    }

    async fn say(&self, text: &str) -> bool {
        self.step(|w, run| {
            w.push(run, data::text(text));
            true
        })
        .await
    }

    async fn status(&self, status: RunStatus) -> bool {
        self.step(|w, run| w.set_status(run, status)).await
    }

    /// Runs the closing steps of a successful run.
    async fn succeed(&self, result: &str) -> bool {
        self.say("All done.").await
            && self.status(RunStatus::Evaluating).await
            && self
                .step(|w, run| {
                    let done = data::done(StopReason::Completed, Some(result), None);
                    w.finish(run, RunStatus::Succeeded, done)
                })
                .await
    }

    /// Holds the run on an approval: status, pending approval and the `NeedsApproval` event.
    async fn hold(&self, subject: reno_proto::ApprovalSubject) -> bool {
        self.step(move |w, run| {
            if !w.set_status(run, RunStatus::WaitingApproval) {
                return false;
            }
            let Ok(approval) = w.hold(run, subject.clone()) else {
                return false;
            };
            w.push(
                run,
                ExecEvent::NeedsApproval {
                    request_id: format!("req-{}", approval.id),
                    subject,
                },
            );
            true
        })
        .await
    }
}

/// The script of a run created by `task.run`. It ends when the run finishes, is held for an
/// approval (then [`resume`] continues it), or is cancelled.
pub(super) async fn run_new(shared: Arc<Shared>, run: RunId, prompt: String) {
    let Some(script) = Script::new(shared, run) else {
        return;
    };
    let _ = work(&script, &prompt).await;
}

async fn work(script: &Script, prompt: &str) -> bool {
    let scenario = Scenario::of(prompt);
    let opened = script.status(RunStatus::Leased).await
        && script
            .step(|w, run| {
                w.set_status(run, RunStatus::Running) && {
                    w.push(run, data::started());
                    true
                }
            })
            .await
        && script
            .say("I will start by looking at the repository layout.")
            .await
        && script
            .step(|w, run| {
                w.push(run, data::tool("t1", "bash", json!({ "command": "ls" })));
                true
            })
            .await
        && script
            .step(|w, run| {
                w.push(run, data::tool_ok("t1", "Cargo.toml, crates/, docs/"));
                w.push(
                    run,
                    ExecEvent::Usage {
                        input_tokens: 1200,
                        output_tokens: 340,
                    },
                );
                true
            })
            .await;
    if !opened {
        return false;
    }
    match scenario {
        Scenario::Plain => script.succeed("The task is complete.").await,
        Scenario::Burst(n) => {
            let burst = script.apply(|w, run| {
                for i in 0..n {
                    w.push(run, data::text(&format!("burst {i}")));
                }
                true
            });
            burst && script.succeed("The burst is complete.").await
        }
        Scenario::HoldPush => {
            let cmd = "git push -u origin HEAD";
            script.say("The change is ready. Pushing the branch.").await
                && script
                    .step(|w, run| {
                        w.push(run, data::tool("t2", "bash", json!({ "command": cmd })));
                        true
                    })
                    .await
                && script.hold(data::command_subject(cmd)).await
        }
        Scenario::HoldFetch => {
            let url = data::SCOUT_FETCH_URL;
            script.say("I need to download a reference file.").await
                && script
                    .step(|w, run| {
                        w.push(run, data::tool("t2", "web_fetch", json!({ "url": url })));
                        true
                    })
                    .await
                && script.hold(data::network_subject(url)).await
        }
    }
}

/// Continues a run held on an approval, after the human decided.
pub(super) async fn resume(shared: Arc<Shared>, run: RunId, decision: ApprovalDecision) {
    let Some(script) = Script::new(shared, run) else {
        return;
    };
    let approved = decision == ApprovalDecision::ApproveOnce;
    let resumed = script.status(RunStatus::Running).await
        && script
            .step(move |w, run| {
                let id = w.run(run).map(|r| r.last_tool_id()).unwrap_or_default();
                let summary = if approved {
                    "approved once"
                } else {
                    "denied by the user"
                };
                w.push(
                    run,
                    ExecEvent::ToolResult {
                        id,
                        ok: approved,
                        summary: Some(summary.to_owned()),
                    },
                );
                true
            })
            .await;
    if !resumed {
        return;
    }
    if approved {
        let _ = script.say("Approved. Carrying on.").await
            && script.succeed("The approved action ran.").await;
    } else {
        let _ = script
            .say("That action was denied, so I am stopping here.")
            .await
            && script.status(RunStatus::Evaluating).await
            && script
                .step(|w, run| {
                    let done = data::done(StopReason::Error, None, Some("approval denied"));
                    w.finish(run, RunStatus::Failed, done)
                })
                .await;
    }
}

/// The short scripted reply to a steering message, one step after it. A run that finished or
/// was cancelled meanwhile gets no reply.
pub(super) async fn reply_to_steer(shared: Arc<Shared>, run: RunId) {
    let Some(script) = Script::new(shared, run) else {
        return;
    };
    let _ = script
        .say("Understood. I will take that into account.")
        .await;
}
