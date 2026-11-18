//! The fake daemon's sign-in of an added agent (`executor.signin.start`, `executor.signin.cancel`),
//! so an app can be built against the whole flow without an agent, a browser or a terminal.
//!
//! The fake never opens, runs or asks for anything: it says what the real daemon would say, on
//! tokio time (tests pause the clock). The offered methods are on the executors' `authMethods`;
//! the agents the fake adds from the registry offer these:
//!
//! | executor | offers |
//! |---|---|
//! | `acp.cline` | `browser` (the agent signs in itself) and `key` (needs `CLINE_API_KEY`) |
//! | `acp.amp-acp` | `key` alone (needs `AMP_API_KEY`) |
//! | `acp.old-agent` | `login`, a terminal command |
//!
//! # What a sign-in does
//!
//! By the method it is started with (the agent's first own method when none is named):
//!
//! - an agent method: `page` (a device page with a code in it), `output` ("Your code: FAKE-1234"),
//!   then, after [`super::FakeConfig::job_duration`] (the person pairing), `done` signed in; the
//!   executor's health turns signed in and `executors.watch` carries it;
//! - a terminal method: `needs_terminal` with its command, then `done` not signed in;
//! - a key method: `done` not signed in, "needs a key: set VAR in the agent's environment".
//!
//! A test scripts the next sign-in of an executor with [`super::FakeDaemon::script_signin`], ends a
//! running one that has no scripted end with [`super::FakeDaemon::finish_signin`], and reads what
//! the app started with [`super::FakeDaemon::signins_started`]. A running sign-in is ended by
//! `executor.signin.cancel` or by dropping the subscription, which makes it `done` not signed in,
//! "the sign-in was cancelled". One runs at a time per executor.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, Stream};
use reno_proto::{
    AuthMethod, AuthMethodKind, ErrorCode, ErrorObject, ExecutorId, ExecutorSource, SignedIn,
    SigninEvent,
};
use reno_rpc::SubItem;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::Shared;
use super::world::World;

const CANCELLED: &str = "the sign-in was cancelled";

/// What the fake remembers of sign-ins.
#[derive(Default)]
pub(super) struct Signins {
    /// Running sign-ins: how to end each, and how a test finishes one that has no scripted end.
    running: HashMap<ExecutorId, Running>,
    /// What the next sign-in of an executor says instead of the default.
    scripts: HashMap<ExecutorId, Vec<SigninEvent>>,
    /// Every sign-in started, oldest first.
    started: Vec<(ExecutorId, Option<String>)>,
}

struct Running {
    cancel: CancellationToken,
    finish: Option<oneshot::Sender<(bool, Option<String>)>>,
}

/// One step of what a sign-in does.
enum Step {
    /// Say this.
    Say(SigninEvent),
    /// The person is doing something: wait.
    Wait,
}

/// A sign-in that is to run.
pub(super) struct Plan {
    id: ExecutorId,
    steps: Vec<Step>,
    cancel: CancellationToken,
    finish: oneshot::Receiver<(bool, Option<String>)>,
    /// The person pairs by themselves after the job time (the default flow), instead of the test
    /// finishing the sign-in.
    auto_pair: bool,
}

fn err(code: ErrorCode, message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(code, message)
}

impl Signins {
    pub(super) fn script(&mut self, id: &ExecutorId, events: Vec<SigninEvent>) {
        self.scripts.insert(id.clone(), events);
    }

    pub(super) fn started(&self) -> Vec<(ExecutorId, Option<String>)> {
        self.started.clone()
    }

    /// Ends the running sign-in of `id` that waits for an end. `false` when there is none.
    pub(super) fn finish(
        &mut self,
        id: &ExecutorId,
        signed_in: bool,
        message: Option<String>,
    ) -> bool {
        self.running
            .get_mut(id)
            .and_then(|r| r.finish.take())
            .is_some_and(|tx| tx.send((signed_in, message)).is_ok())
    }

    pub(super) fn cancel(&mut self, id: &ExecutorId) -> bool {
        match self.running.get(id) {
            Some(running) => {
                running.cancel.cancel();
                true
            }
            None => false,
        }
    }
}

/// The method a sign-in uses: the one named, else the agent's first of its own, else its first
/// terminal one.
fn chosen<'a>(
    methods: &'a [AuthMethod],
    wanted: Option<&str>,
) -> Result<Option<&'a AuthMethod>, String> {
    match wanted {
        Some(id) => methods
            .iter()
            .find(|m| m.id == id)
            .map(Some)
            .ok_or_else(|| format!("the agent offers no sign-in method {id}")),
        None => Ok(methods
            .iter()
            .find(|m| m.kind == AuthMethodKind::Agent)
            .or_else(|| methods.iter().find(|m| m.kind == AuthMethodKind::Terminal))
            .or_else(|| methods.first())),
    }
}

impl World {
    /// `executor.signin.start`: checks the executor and plans the sign-in.
    pub(super) fn signin_start(
        &mut self,
        id: &ExecutorId,
        method: Option<String>,
        root: &CancellationToken,
    ) -> Result<Plan, ErrorObject> {
        let info = self
            .executors
            .iter()
            .find(|e| &e.descriptor.id == id)
            .ok_or_else(|| err(ErrorCode::NotFound, "no such executor"))?;
        if info.source != ExecutorSource::Registry {
            return Err(err(
                ErrorCode::Conflict,
                format!("{id} signs in with its own command"),
            ));
        }
        if info
            .job
            .as_ref()
            .is_some_and(|j| !matches!(j.phase, reno_proto::JobPhase::Failed { .. }))
        {
            return Err(err(
                ErrorCode::Conflict,
                format!("an install or update of {id} is running"),
            ));
        }
        if self.signins.running.contains_key(id) {
            return Err(err(
                ErrorCode::Conflict,
                format!("a sign-in of {id} is already running"),
            ));
        }
        let name = info
            .descriptor
            .adapter_version
            .rsplit(", ")
            .next()
            .unwrap_or("The agent")
            .to_owned();
        let (steps, auto_pair) = match self.signins.scripts.remove(id) {
            Some(events) => {
                let ends = matches!(events.last(), Some(SigninEvent::Done { .. }));
                let mut steps: Vec<Step> = events.into_iter().map(Step::Say).collect();
                if !ends {
                    steps.push(Step::Wait);
                }
                (steps, false)
            }
            None => default_steps(&info.auth_methods, method.as_deref(), &name),
        };
        let cancel = root.child_token();
        let (tx, finish) = oneshot::channel();
        self.signins.running.insert(
            id.clone(),
            Running {
                cancel: cancel.clone(),
                finish: Some(tx),
            },
        );
        self.signins.started.push((id.clone(), method));
        Ok(Plan {
            id: id.clone(),
            steps,
            cancel,
            finish,
            auto_pair,
        })
    }

    /// The sign-in ended: the executor is signed in when it says so, and the slot is free.
    fn signin_end(&mut self, id: &ExecutorId, signed_in: bool) {
        self.signins.running.remove(id);
        if signed_in
            && let Some(health) = self
                .executors
                .iter_mut()
                .find(|e| &e.descriptor.id == id)
                .and_then(|e| e.health.as_mut())
        {
            health.signed_in = SignedIn::Yes {
                account: Some("you@example.com".into()),
            };
        }
    }
}

/// What a sign-in does by default, and whether the person pairs by themselves at its end.
fn default_steps(methods: &[AuthMethod], wanted: Option<&str>, name: &str) -> (Vec<Step>, bool) {
    let done = |signed_in: bool, message: Option<String>| {
        Step::Say(SigninEvent::Done { signed_in, message })
    };
    let method = match chosen(methods, wanted) {
        Ok(method) => method,
        Err(why) => return (vec![done(false, Some(why))], false),
    };
    match method.map(|m| (m.kind, m)) {
        Some((AuthMethodKind::Terminal, method)) => (
            vec![
                Step::Say(SigninEvent::NeedsTerminal {
                    command: method
                        .command
                        .clone()
                        .unwrap_or_else(|| format!("{} login", name.to_ascii_lowercase())),
                }),
                done(
                    false,
                    Some(format!(
                        "{name} signs in in a terminal: run the command shown there"
                    )),
                ),
            ],
            false,
        ),
        Some((AuthMethodKind::EnvVar, method)) => (
            vec![done(
                false,
                Some(format!(
                    "needs a key: set {} in the agent's environment",
                    method.vars.join(" and ")
                )),
            )],
            false,
        ),
        _ => (
            vec![
                Step::Say(SigninEvent::Page {
                    url: "https://auth.example.com/device?user_code=FAKE-1234".into(),
                }),
                Step::Say(SigninEvent::Output {
                    line: "Your code: FAKE-1234".into(),
                }),
                // The person pairs the device; the fake does it for them after the job time.
                Step::Wait,
            ],
            true,
        ),
    }
}

/// Runs the plan, sending what it says to the client.
pub(super) fn serve(
    shared: &Arc<Shared>,
    plan: Plan,
    step: Duration,
    pairing: Duration,
) -> impl Stream<Item = SubItem<SigninEvent>> + Send + use<> {
    let (tx, rx) = mpsc::channel::<SigninEvent>(64);
    let shared = Arc::clone(shared);
    shared.tracker.clone().spawn(async move {
        let Plan {
            id,
            steps,
            cancel,
            finish,
            auto_pair,
        } = plan;
        let mut finish = Some(finish);
        let mut ended: Option<(bool, Option<String>)> = None;
        'script: for next in steps {
            match next {
                Step::Say(SigninEvent::Done { signed_in, message }) => {
                    ended = Some((signed_in, message));
                    break 'script;
                }
                Step::Say(event) => {
                    tokio::select! {
                        () = cancel.cancelled() => { break 'script; }
                        () = tokio::time::sleep(step) => {}
                    }
                    if tx.send(event).await.is_err() {
                        cancel.cancel();
                        break 'script;
                    }
                }
                Step::Wait => {
                    let Some(waiting) = finish.take() else {
                        break 'script;
                    };
                    // The default flow pairs by itself after `pairing`; a scripted one waits for
                    // the test (`finish_signin`).
                    let paired = async {
                        if auto_pair {
                            tokio::time::sleep(pairing).await;
                            Some((true, None))
                        } else {
                            std::future::pending().await
                        }
                    };
                    tokio::select! {
                        () = cancel.cancelled() => {}
                        () = tx.closed() => { cancel.cancel(); }
                        told = waiting => { ended = told.ok(); }
                        paired = paired => { ended = paired; }
                    }
                    break 'script;
                }
            }
        }
        let (signed_in, message) = match ended {
            Some(end) if !cancel.is_cancelled() => end,
            _ => (false, Some(CANCELLED.to_owned())),
        };
        shared.with(|w| w.signin_end(&id, signed_in));
        let _ = tx.send(SigninEvent::Done { signed_in, message }).await;
    });
    stream::unfold((rx, 0_u64), |(mut rx, n)| async move {
        let event = rx.recv().await?;
        Some((SubItem::seq(reno_proto::Seq(n + 1), event), (rx, n + 1)))
    })
}
