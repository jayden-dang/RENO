//! The mocked answer contract against the fake daemon (shaped like ROAD-1).

#![cfg(feature = "fake")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures::StreamExt;
use reno_client::answers::{AnswerError, AnswerRequest, QuestionAnswer, QuestionDesk};
use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_proto::{ExecEvent, RunId, RunStatus};

fn run(name: &str) -> RunId {
    RunId::from_suffix(name).unwrap()
}

fn daemon() -> FakeDaemon {
    FakeDaemon::start(FakeConfig {
        question_run: true,
        question_demo_runs: true,
        ..FakeConfig::default()
    })
    .unwrap()
}

fn request(id: &str, question: &str, labels: &[&str]) -> AnswerRequest {
    AnswerRequest {
        question_id: id.to_owned(),
        answers: vec![QuestionAnswer {
            question_id: question.to_owned(),
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
        }],
    }
}

async fn journal(daemon: &FakeDaemon, run: &RunId, n: usize) -> Vec<ExecEvent> {
    let client = daemon.connect().await.unwrap();
    client
        .run_events(run.clone(), None)
        .take(n)
        .map(|i| i.unwrap().event)
        .collect()
        .await
}

#[test]
fn rules_follow_the_agent() {
    let d = daemon();
    assert!(d.rules(&run("asker1"), "req-asker-policy").text_allowed);
    assert!(!d.rules(&run("grok1"), "req-grok-format").text_allowed);
    assert_eq!(
        d.rules(&run("asker1"), "req-asker-policy").max_text_len,
        Some(500)
    );
}

#[test]
fn a_typed_answer_to_a_grok_question_is_refused_with_the_options() {
    let d = daemon();
    let err = d
        .answer(
            &run("grok1"),
            request("req-grok-format", "format", &["whatever you like"]),
        )
        .unwrap_err();
    match err {
        AnswerError::AnswerInvalid { allowed, .. } => {
            assert_eq!(allowed, ["JSON lines", "Plain text"]);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn an_answer_is_journaled_the_run_resumes_and_a_second_answer_is_not_pending() {
    let d = daemon();
    let id = run("asker1");
    d.answer(
        &id,
        request("req-asker-policy", "policy", &["Fail fast and alert"]),
    )
    .unwrap();
    let events = journal(&d, &id, 5).await;
    assert!(events.iter().any(|e| matches!(
        e,
        ExecEvent::ToolResult { summary: Some(s), .. } if s.contains("\"Fail fast and alert\"")
    )));
    let client = d.connect().await.unwrap();
    let got = client.get_run(&id).await.unwrap();
    assert_eq!(got.status, RunStatus::Running);
    assert_eq!(
        d.answer(
            &id,
            request("req-asker-policy", "policy", &["Fail fast and alert"])
        ),
        Err(AnswerError::QuestionNotPending)
    );
}

#[test]
fn typed_text_over_the_cap_is_refused() {
    let d = daemon();
    let too_long = "x".repeat(501);
    assert!(matches!(
        d.answer(
            &run("asker1"),
            request("req-asker-policy", "policy", &[&too_long])
        ),
        Err(AnswerError::AnswerInvalid { .. })
    ));
}

#[test]
fn a_single_select_question_takes_one_label() {
    let d = daemon();
    let err = d
        .answer(
            &run("asker1"),
            request(
                "req-asker-policy",
                "policy",
                &["Bounded retry with jitter", "Fail fast and alert"],
            ),
        )
        .unwrap_err();
    assert!(matches!(err, AnswerError::AnswerInvalid { .. }));
}
