//! A person-started sign-in of an added agent (`Kernel::start_signin`): the events a client is to
//! show and act on reach it in order, the end is a full probe that turns the health signed in,
//! cancelling or dropping the stream stops the agent, and nothing installs, removes or doubles it
//! meanwhile. The agent is a script; nothing is started, opened or asked of the desktop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::HarnessError;
use reno_harness::exec::{ExecControls, ExecError, Executor, ProbeDepth, SignInEvents};
use reno_proto::{
    AuthMethod, AuthMethodKind, ExecEvent, ExecSpec, ExecutorDescriptor, ExecutorSource,
    HealthProbe, JobPhase, SignedIn, SigninEvent,
};
use tokio::sync::Notify;

use super::{Agent, World, id, until};
use crate::KernelError;

/// How the scripted sign-in ends.
#[derive(Clone)]
enum Ending {
    /// The agent says it is signed in (and a probe from now on agrees).
    SignedIn,
    /// The agent says it is signed in, but a probe still finds it signed out.
    Lies,
    /// It fails with these words.
    Fails(&'static str),
    /// It waits until `release` is notified, then signs in.
    Waits,
}

#[derive(Clone)]
struct Scripted {
    inner: Agent,
    /// What the sign-in reports, in order.
    events: Vec<SigninEvent>,
    ending: Ending,
    signed_in: Arc<AtomicBool>,
    release: Arc<Notify>,
    /// Set when the sign-in's future is dropped, whichever way it ended.
    dropped: Arc<AtomicBool>,
    /// The methods the agent used, per call.
    methods: Arc<Mutex<Vec<Option<String>>>>,
    /// Methods a full probe's handshake saw; empty until then.
    seen: Arc<Mutex<Vec<AuthMethod>>>,
    /// The sink of every call, kept for good: a reader task that outlives the sign-in (a
    /// descendant of the agent holding its pipe) still holds one, and `done` must end the stream.
    held: Arc<Mutex<Vec<SignInEvents>>>,
}

struct Marks(Arc<AtomicBool>);

impl Drop for Marks {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Scripted {
    fn new(inner: Agent, events: Vec<SigninEvent>, ending: Ending) -> Self {
        Self {
            inner,
            events,
            ending,
            signed_in: Arc::default(),
            release: Arc::default(),
            dropped: Arc::default(),
            methods: Arc::default(),
            seen: Arc::default(),
            held: Arc::default(),
        }
    }
}

fn offered() -> Vec<AuthMethod> {
    vec![
        AuthMethod {
            id: "browser".into(),
            name: "Sign in in the browser".into(),
            description: None,
            kind: AuthMethodKind::Agent,
            vars: vec![],
            link: None,
            command: None,
            unusable: None,
        },
        AuthMethod {
            id: "key".into(),
            name: "API key".into(),
            description: None,
            kind: AuthMethodKind::EnvVar,
            vars: vec!["DEMO_API_KEY".into()],
            link: None,
            command: None,
            unusable: None,
        },
    ]
}

#[async_trait]
impl Executor for Scripted {
    fn descriptor(&self) -> ExecutorDescriptor {
        self.inner.descriptor()
    }

    fn source(&self) -> ExecutorSource {
        ExecutorSource::Registry
    }

    async fn probe_at(&self, depth: ProbeDepth) -> HealthProbe {
        let mut probe = self.inner.probe().await;
        if depth == ProbeDepth::Full {
            *self.seen.lock().unwrap() = offered();
            probe.signed_in = if self.signed_in.load(Ordering::SeqCst) {
                SignedIn::Yes { account: None }
            } else {
                SignedIn::No {
                    hint: "signed out".into(),
                }
            };
        }
        probe
    }

    async fn probe(&self) -> HealthProbe {
        self.probe_at(ProbeDepth::Full).await
    }

    fn auth_methods(&self) -> Vec<AuthMethod> {
        self.seen.lock().unwrap().clone()
    }

    async fn sign_in(
        &self,
        method: Option<String>,
        events: SignInEvents,
    ) -> Result<(), HarnessError> {
        let _marks = Marks(self.dropped.clone());
        self.methods.lock().unwrap().push(method);
        self.held.lock().unwrap().push(events.clone());
        for event in &self.events {
            events(event.clone());
        }
        match &self.ending {
            Ending::SignedIn => {
                self.signed_in.store(true, Ordering::SeqCst);
                Ok(())
            }
            Ending::Lies => Ok(()),
            Ending::Fails(why) => Err(HarnessError::Protocol((*why).into())),
            Ending::Waits => {
                self.release.notified().await;
                self.signed_in.store(true, Ordering::SeqCst);
                Ok(())
            }
        }
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        self.inner.start(spec, controls).await
    }
}

fn page() -> SigninEvent {
    SigninEvent::Page {
        url: "https://auth.example.com/device?user_code=WXYZ-9876".into(),
    }
}

fn line(text: &str) -> SigninEvent {
    SigninEvent::Output { line: text.into() }
}

/// An added, installed agent whose sign-in is `scripted`.
async fn added(w: &World, scripted: &Scripted) {
    w.index.list("demo", "1.0.0");
    w.fx.kernel
        .add_registry_executor(Arc::new(scripted.clone()))
        .await
        .unwrap();
    until(async || w.info("acp.demo")?.job.is_none().then_some(())).await;
}

fn scripted(w: &World, events: Vec<SigninEvent>, ending: Ending) -> Scripted {
    Scripted::new(w.agent("acp.demo"), events, ending)
}

async fn collect(mut stream: BoxStream<'static, SigninEvent>) -> Vec<SigninEvent> {
    let mut all = Vec::new();
    while let Some(event) = stream.next().await {
        all.push(event);
    }
    all
}

#[tokio::test(start_paused = true)]
async fn a_sign_in_relays_the_page_and_lines_then_ends_with_a_probe_that_shows_it_signed_in() {
    let w = World::new(2, vec![]).await;
    let agent = scripted(
        &w,
        vec![page(), line("Your code: WXYZ-9876")],
        Ending::SignedIn,
    );
    added(&w, &agent).await;
    assert!(matches!(
        w.info("acp.demo").unwrap().health.unwrap().signed_in,
        SignedIn::No { .. }
    ));

    let stream =
        w.fx.kernel
            .start_signin(&id("acp.demo"), Some("browser".into()))
            .unwrap();
    let events = collect(stream).await;
    assert_eq!(
        events,
        [
            page(),
            line("Your code: WXYZ-9876"),
            SigninEvent::Done {
                signed_in: true,
                message: None
            }
        ]
    );
    assert_eq!(
        agent.methods.lock().unwrap().as_slice(),
        [Some("browser".to_owned())]
    );
    // The health the clients see is the full probe's.
    let shown = w.info("acp.demo").unwrap().health.unwrap();
    assert!(matches!(shown.signed_in, SignedIn::Yes { .. }), "{shown:?}");
    // And the agent is free again: another sign-in may start.
    drop(w.fx.kernel.start_signin(&id("acp.demo"), None).unwrap());
}

#[tokio::test(start_paused = true)]
async fn a_sign_in_that_fails_ends_in_words_and_leaves_the_health_alone() {
    let w = World::new(2, vec![]).await;
    let agent = scripted(&w, vec![], Ending::Fails("the login page timed out"));
    added(&w, &agent).await;
    let events = collect(w.fx.kernel.start_signin(&id("acp.demo"), None).unwrap()).await;
    assert_eq!(
        events,
        [SigninEvent::Done {
            signed_in: false,
            message: Some("the login page timed out".into())
        }]
    );
    assert!(matches!(
        w.info("acp.demo").unwrap().health.unwrap().signed_in,
        SignedIn::No { .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn a_sign_in_the_probe_does_not_confirm_does_not_say_signed_in() {
    let w = World::new(2, vec![]).await;
    let agent = scripted(&w, vec![], Ending::Lies);
    added(&w, &agent).await;
    let events = collect(w.fx.kernel.start_signin(&id("acp.demo"), None).unwrap()).await;
    let Some(SigninEvent::Done { signed_in, message }) = events.last() else {
        panic!("{events:?}")
    };
    assert!(!signed_in);
    assert!(
        message
            .as_deref()
            .unwrap()
            .contains("still says it is signed out")
    );
}

#[tokio::test(start_paused = true)]
async fn a_terminal_only_agent_hands_the_command_to_the_client() {
    let w = World::new(2, vec![]).await;
    let command = SigninEvent::NeedsTerminal {
        command: "/agents/demo/demo login".into(),
    };
    let agent = scripted(
        &w,
        vec![command.clone()],
        Ending::Fails("Demo signs in in a terminal: run the command shown there"),
    );
    added(&w, &agent).await;
    let events = collect(w.fx.kernel.start_signin(&id("acp.demo"), None).unwrap()).await;
    assert_eq!(events[0], command);
    assert!(matches!(
        events[1],
        SigninEvent::Done {
            signed_in: false,
            ..
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn cancelling_stops_the_agent_and_frees_the_executor() {
    let w = World::new(2, vec![]).await;
    let agent = scripted(&w, vec![page()], Ending::Waits);
    added(&w, &agent).await;
    let mut stream = w.fx.kernel.start_signin(&id("acp.demo"), None).unwrap();
    assert_eq!(stream.next().await, Some(page()));
    // Running: a second one is refused, and so is an install, an update and a removal.
    assert!(matches!(
        w.fx.kernel.start_signin(&id("acp.demo"), None),
        Err(KernelError::SignInRefused { .. })
    ));
    assert!(matches!(
        w.fx.kernel.remove_registry_executor(&id("acp.demo")).await,
        Err(KernelError::ExecutorInUse { .. })
    ));
    assert!(w.fx.kernel.cancel_signin(&id("acp.demo")));
    let last = stream.next().await.unwrap();
    assert!(
        matches!(&last, SigninEvent::Done { signed_in: false, message: Some(m) } if m.contains("cancelled")),
        "{last:?}"
    );
    assert_eq!(stream.next().await, None);
    assert!(
        agent.dropped.load(Ordering::SeqCst),
        "the agent's sign-in was stopped"
    );
    assert!(!w.fx.kernel.cancel_signin(&id("acp.demo")), "none left");
    drop(w.fx.kernel.start_signin(&id("acp.demo"), None).unwrap());
}

#[tokio::test(start_paused = true)]
async fn dropping_the_stream_cancels_the_sign_in() {
    let w = World::new(2, vec![]).await;
    let agent = scripted(&w, vec![page()], Ending::Waits);
    added(&w, &agent).await;
    let mut stream = w.fx.kernel.start_signin(&id("acp.demo"), None).unwrap();
    assert_eq!(stream.next().await, Some(page()));
    drop(stream);
    until(async || agent.dropped.load(Ordering::SeqCst).then_some(())).await;
    // The claim is released too (the closing task has run).
    until(async || {
        w.fx.kernel
            .start_signin(&id("acp.demo"), None)
            .ok()
            .map(drop)
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn nothing_is_signed_in_that_is_not_an_added_agent_or_is_installing() {
    let disk = Default::default();
    let builtin = Agent::new("acp.devin", &disk).builtin();
    let w = World::new(2, vec![builtin]).await;
    assert!(matches!(
        w.fx.kernel.start_signin(&id("acp.devin"), None),
        Err(KernelError::SignInRefused { .. })
    ));
    assert!(matches!(
        w.fx.kernel.start_signin(&id("acp.nope"), None),
        Err(KernelError::ExecutorNotFound(_))
    ));
    // An install running on an added agent takes the sign-in's place.
    let agent = scripted(&w, vec![], Ending::SignedIn);
    w.index.list("demo", "1.0.0");
    let _hold = w.index.hold_installs();
    w.fx.kernel
        .add_registry_executor(Arc::new(agent))
        .await
        .unwrap();
    until(async || {
        matches!(w.info("acp.demo")?.job?.phase, JobPhase::Running { .. }).then_some(())
    })
    .await;
    assert!(matches!(
        w.fx.kernel.start_signin(&id("acp.demo"), None),
        Err(KernelError::SignInRefused { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn the_methods_an_agent_offered_show_in_its_info_after_a_full_probe_only() {
    let w = World::new(2, vec![]).await;
    let agent = scripted(&w, vec![], Ending::SignedIn);
    w.index.list("demo", "1.0.0");
    // Registered, never probed: nothing offered, and nothing started to find out.
    w.fx.kernel
        .register_executor(Arc::new(agent), false)
        .await
        .unwrap();
    assert!(w.info("acp.demo").unwrap().auth_methods.is_empty());
    w.fx.kernel
        .refresh_executors(Some(&id("acp.demo")))
        .await
        .unwrap();
    let methods = w.info("acp.demo").unwrap().auth_methods;
    assert_eq!(methods.len(), 2);
    assert_eq!(methods[1].kind, AuthMethodKind::EnvVar);
    assert_eq!(methods[1].vars, ["DEMO_API_KEY"]);
}
