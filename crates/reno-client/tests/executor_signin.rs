//! Signing in an added agent against the fake daemon (feature `executor.signin`): the methods an
//! agent offers, the default flow for each kind of method, a scripted flow, cancelling, dropping
//! the stream, and one sign-in at a time. Time is tokio's, paused.

#![cfg(feature = "fake")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures::StreamExt;
use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_client::{Client, ClientError};
use reno_proto::{AuthMethodKind, ErrorCode, ExecutorId, ExecutorInfo, SignedIn, SigninEvent};

async fn daemon() -> (FakeDaemon, Client) {
    let daemon = FakeDaemon::start(FakeConfig {
        registry_agents: true,
        ..FakeConfig::default()
    })
    .unwrap();
    let client = daemon.connect().await.unwrap();
    assert!(client.supports(reno_proto::FEATURE_EXECUTOR_SIGNIN));
    (daemon, client)
}

fn id(name: &str) -> ExecutorId {
    name.parse().unwrap()
}

async fn info(client: &Client, name: &str) -> ExecutorInfo {
    client
        .list_executors()
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.descriptor.id.as_str() == name)
        .unwrap()
}

async fn collect(client: &Client, name: &str, method: Option<&str>) -> Vec<SigninEvent> {
    client
        .signin_executor(id(name), method.map(str::to_owned))
        .await
        .unwrap()
        .map(|e| e.unwrap())
        .collect()
        .await
}

#[tokio::test(start_paused = true)]
async fn the_executors_list_what_each_agent_offers() {
    let (_daemon, client) = daemon().await;
    let cline = info(&client, "acp.cline").await;
    assert_eq!(cline.auth_methods.len(), 2);
    assert_eq!(cline.auth_methods[0].kind, AuthMethodKind::Agent);
    assert_eq!(cline.auth_methods[1].kind, AuthMethodKind::EnvVar);
    assert_eq!(cline.auth_methods[1].vars, ["CLINE_API_KEY"]);
    let old = info(&client, "acp.old-agent").await;
    assert_eq!(
        old.auth_methods[0].command.as_deref(),
        Some("/fake/agents/old-agent/bin login")
    );
    // The built-in executors have none.
    assert!(info(&client, "codex").await.auth_methods.is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_agent_method_hands_over_a_page_and_a_code_then_signs_the_executor_in() {
    let (daemon, client) = daemon().await;
    assert!(!matches!(
        info(&client, "acp.cline").await.health.unwrap().signed_in,
        SignedIn::Yes { .. }
    ));
    let events = collect(&client, "acp.cline", Some("browser")).await;
    assert_eq!(
        events,
        [
            SigninEvent::Page {
                url: "https://auth.example.com/device?user_code=FAKE-1234".into()
            },
            SigninEvent::Output {
                line: "Your code: FAKE-1234".into()
            },
            SigninEvent::Done {
                signed_in: true,
                message: None
            },
        ]
    );
    assert!(matches!(
        info(&client, "acp.cline").await.health.unwrap().signed_in,
        SignedIn::Yes { .. }
    ));
    assert_eq!(
        daemon.signins_started(),
        [(id("acp.cline"), Some("browser".to_owned()))]
    );
}

#[tokio::test(start_paused = true)]
async fn a_terminal_method_hands_over_its_command_and_a_key_method_says_what_to_set() {
    let (_daemon, client) = daemon().await;
    let events = collect(&client, "acp.old-agent", None).await;
    assert_eq!(
        events[0],
        SigninEvent::NeedsTerminal {
            command: "/fake/agents/old-agent/bin login".into()
        }
    );
    assert!(matches!(
        events[1],
        SigninEvent::Done {
            signed_in: false,
            ..
        }
    ));

    let events = collect(&client, "acp.cline", Some("key")).await;
    assert_eq!(
        events,
        [SigninEvent::Done {
            signed_in: false,
            message: Some("needs a key: set CLINE_API_KEY in the agent's environment".into())
        }]
    );
}

#[tokio::test(start_paused = true)]
async fn a_scripted_sign_in_stays_open_until_the_test_finishes_it() {
    let (daemon, client) = daemon().await;
    daemon.script_signin(
        &id("acp.amp-acp"),
        vec![SigninEvent::Output {
            line: "waiting".into(),
        }],
    );
    let mut stream = client
        .signin_executor(id("acp.amp-acp"), None)
        .await
        .unwrap();
    assert_eq!(
        stream.next().await.unwrap().unwrap(),
        SigninEvent::Output {
            line: "waiting".into()
        }
    );
    assert!(daemon.finish_signin(&id("acp.amp-acp"), false, Some("nope")));
    assert_eq!(
        stream.next().await.unwrap().unwrap(),
        SigninEvent::Done {
            signed_in: false,
            message: Some("nope".into())
        }
    );
    assert!(stream.next().await.is_none());
    assert!(
        !daemon.finish_signin(&id("acp.amp-acp"), true, None),
        "none left"
    );
}

#[tokio::test(start_paused = true)]
async fn cancelling_ends_it_and_one_runs_at_a_time() {
    let (daemon, client) = daemon().await;
    daemon.script_signin(&id("acp.cline"), vec![]);
    let mut stream = client.signin_executor(id("acp.cline"), None).await.unwrap();
    // A second start on the executor is a conflict.
    let second = client.signin_executor(id("acp.cline"), None).await;
    let Err(ClientError::Remote(refused)) = second else {
        panic!("a second sign-in was accepted");
    };
    assert_eq!(refused.code, ErrorCode::Conflict);
    assert!(client.cancel_signin(&id("acp.cline")).await.unwrap());
    assert_eq!(
        stream.next().await.unwrap().unwrap(),
        SigninEvent::Done {
            signed_in: false,
            message: Some("the sign-in was cancelled".into())
        }
    );
    assert!(!client.cancel_signin(&id("acp.cline")).await.unwrap());
    // Free again.
    daemon.script_signin(&id("acp.cline"), vec![]);
    drop(client.signin_executor(id("acp.cline"), None).await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn dropping_the_stream_cancels_and_frees_the_executor() {
    let (daemon, client) = daemon().await;
    daemon.script_signin(&id("acp.cline"), vec![]);
    let stream = client.signin_executor(id("acp.cline"), None).await.unwrap();
    drop(stream);
    // The slot frees once the daemon has seen the unsubscribe.
    for _ in 0..50 {
        daemon.script_signin(&id("acp.cline"), vec![]);
        if client.signin_executor(id("acp.cline"), None).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the executor stayed busy after its stream was dropped");
}

#[tokio::test(start_paused = true)]
async fn a_builtin_executor_and_an_unknown_one_are_refused() {
    let (_daemon, client) = daemon().await;
    for (name, code) in [
        ("codex", ErrorCode::Conflict),
        ("acp.nope", ErrorCode::NotFound),
    ] {
        let Err(ClientError::Remote(refused)) = client.signin_executor(id(name), None).await else {
            panic!("{name} was accepted");
        };
        assert_eq!(refused.code, code, "{name}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_daemon_without_the_feature_is_refused_by_the_client() {
    let daemon = FakeDaemon::start(FakeConfig {
        without_features: vec![reno_proto::FEATURE_EXECUTOR_SIGNIN.to_owned()],
        registry_agents: true,
        ..FakeConfig::default()
    })
    .unwrap();
    let client = daemon.connect().await.unwrap();
    assert!(client.signin_executor(id("acp.cline"), None).await.is_err());
}
