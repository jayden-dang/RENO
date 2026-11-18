//! Signing an added agent in, from the app (`executor.signin.start`, feature `executor.signin`).
//!
//! The daemon never opens a page or a terminal: it tells the app, and the app opens them on the
//! device the person is at. This file reads the sign-in's events, opens the page (web links only)
//! and Terminal through the injected openers, keeps the lines it printed (a device code shows
//! there) and what became of it. A sign-in that ends signed in is a toast and leaves nothing
//! behind; one that does not stays on the agent's line, in the daemon's words, until the next try.

use std::collections::BTreeSet;

use futures::StreamExt as _;
use futures::channel::mpsc::unbounded;
use gpui::{AppContext as _, Context, Task};
use gpui_tokio::Tokio;
use reno_client::ClientError;
use reno_proto::{ExecutorId, SigninEvent};

use super::Store;
use crate::ui::toast;

/// Where a sign-in is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SigninPhase {
    /// Started: waiting on the agent and the person.
    Running,
    /// Ended without signing in: why, in words.
    Failed(String),
}

/// What the agent's line shows of its sign-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SigninView {
    pub(crate) phase: SigninPhase,
    /// The lines it printed, secrets removed by the daemon (a device code shows here).
    pub(crate) lines: Vec<String>,
    /// A page was opened in the browser.
    pub(crate) page: bool,
    /// The command it opened in Terminal.
    pub(crate) terminal: Option<String>,
}

impl SigninView {
    pub(crate) fn running(&self) -> bool {
        self.phase == SigninPhase::Running
    }
}

/// A sign-in the store follows.
pub(super) struct SigninRun {
    view: SigninView,
    /// A person asked it to stop: its `done` leaves nothing on the line.
    cancelled: bool,
    /// Reads the subscription on tokio; dropping it drops the subscription.
    _stream: Task<Result<(), gpui_tokio::JoinError>>,
    /// Applies what it read.
    _apply: Task<()>,
}

enum Msg {
    Event(SigninEvent),
    Failed(String),
}

fn reason(error: ClientError) -> String {
    match error {
        ClientError::Remote(error) => error.message.clone(),
        other => other.to_string(),
    }
}

impl Store {
    /// The sign-in of `id`, running or failed.
    pub(crate) fn signin(&self, id: &ExecutorId) -> Option<&SigninView> {
        self.signins.get(id).map(|run| &run.view)
    }

    /// Executors with a sign-in command running in Terminal that the app looks at again once the
    /// person is back: taken, so each is looked at once.
    pub(crate) fn take_signin_terminals(&mut self) -> BTreeSet<ExecutorId> {
        std::mem::take(&mut self.signin_terminals)
    }

    /// A sign-in command is open in Terminal.
    pub(crate) fn waiting_on_signin_terminal(&self) -> bool {
        !self.signin_terminals.is_empty()
    }

    /// `executor.signin.start`: signs `id` in with `method` (the agent's first when `None`). One
    /// runs at a time per executor.
    pub(crate) fn start_signin(
        &mut self,
        id: ExecutorId,
        name: String,
        method: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if !self.supports_browser_sign_in()
            || self.signins.get(&id).is_some_and(|run| run.view.running())
        {
            return;
        }
        let Some(client) = self.client() else {
            return;
        };
        let (tx, mut rx) = unbounded::<Msg>();
        let wanted = id.clone();
        let stream = Tokio::spawn(cx, async move {
            match client.signin_executor(wanted, method).await {
                Err(error) => {
                    tx.unbounded_send(Msg::Failed(reason(error))).ok();
                }
                Ok(mut events) => {
                    while let Some(item) = events.next().await {
                        let msg = match item {
                            Ok(event) => Msg::Event(event),
                            Err(error) => Msg::Failed(reason(error)),
                        };
                        let stop = matches!(msg, Msg::Failed(_));
                        if tx.unbounded_send(msg).is_err() || stop {
                            break;
                        }
                    }
                }
            }
        });
        let key = id.clone();
        let apply = cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.next().await {
                let applied = this.update(cx, |store, cx| store.signin_msg(&key, &name, msg, cx));
                if applied.is_err() {
                    return;
                }
            }
            // The stream ended: a sign-in that never said `done` did not finish.
            this.update(cx, |store, cx| {
                if store.signins.get(&key).is_some_and(|r| r.view.running()) {
                    store.signin_failed(&key, "the sign-in ended before it finished".to_owned());
                    cx.notify();
                }
            })
            .ok();
        });
        self.signins.insert(
            id,
            SigninRun {
                view: SigninView {
                    phase: SigninPhase::Running,
                    lines: Vec::new(),
                    page: false,
                    terminal: None,
                },
                cancelled: false,
                _stream: stream,
                _apply: apply,
            },
        );
        cx.notify();
    }

    /// `executor.signin.cancel`: stops the running sign-in of `id`. The line clears when the
    /// daemon says it ended.
    pub(crate) fn cancel_signin(&mut self, id: &ExecutorId, cx: &mut Context<Self>) {
        let Some(run) = self.signins.get_mut(id) else {
            return;
        };
        if !run.view.running() {
            // A failure on the line: dismissing it is all there is to do.
            self.signins.remove(id);
            cx.notify();
            return;
        }
        run.cancelled = true;
        if let Some(client) = self.client() {
            let id = id.clone();
            let call = Tokio::spawn(cx, async move { client.cancel_signin(&id).await });
            cx.background_spawn(async move {
                call.await.ok();
            })
            .detach();
        }
    }

    fn signin_failed(&mut self, id: &ExecutorId, why: String) {
        if let Some(run) = self.signins.get_mut(id) {
            run.view.phase = SigninPhase::Failed(why);
        }
    }

    fn signin_msg(&mut self, id: &ExecutorId, name: &str, msg: Msg, cx: &mut Context<Self>) {
        let event = match msg {
            Msg::Failed(why) => {
                if self.signins.get(id).is_some_and(|r| !r.cancelled) {
                    self.signin_failed(id, why);
                } else {
                    self.signins.remove(id);
                }
                cx.notify();
                return;
            }
            Msg::Event(event) => event,
        };
        match event {
            SigninEvent::Page { url } => {
                // Only a web link is opened, whatever the daemon says.
                let opened = crate::links::open(cx, &url);
                if let Some(run) = self.signins.get_mut(id) {
                    run.view.page |= opened;
                }
            }
            SigninEvent::Output { line } => {
                if let Some(run) = self.signins.get_mut(id) {
                    run.view.lines.push(line);
                }
            }
            SigninEvent::NeedsTerminal { command } => {
                match crate::terminal::open(cx, &format!("Sign in to {name}"), &command) {
                    Ok(()) => {
                        self.signin_terminals.insert(id.clone());
                        if let Some(run) = self.signins.get_mut(id) {
                            run.view.terminal = Some(command);
                        }
                    }
                    Err(why) => toast::push(cx, toast::Tone::Error, "Terminal", why),
                }
            }
            SigninEvent::Done { signed_in, message } => {
                let cancelled = self.signins.get(id).is_some_and(|r| r.cancelled);
                if signed_in {
                    self.signins.remove(id);
                    toast::push(
                        cx,
                        toast::Tone::Info,
                        "Signed in",
                        format!("{name} signed in"),
                    );
                } else if cancelled {
                    self.signins.remove(id);
                } else {
                    let in_terminal = self
                        .signins
                        .get(id)
                        .is_some_and(|r| r.view.terminal.is_some());
                    let why = message.unwrap_or_else(|| {
                        if in_terminal {
                            "Finish signing in in Terminal, then come back.".to_owned()
                        } else {
                            "The sign-in did not finish.".to_owned()
                        }
                    });
                    self.signin_failed(id, why);
                }
            }
        }
        cx.notify();
    }
}
