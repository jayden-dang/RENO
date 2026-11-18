//! The fake daemon's Terminal tab: terminals that echo their input, replay from a sequence number
//! and say whether they are sandboxed. No PTY, no shell.

#![cfg(feature = "fake")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures::StreamExt;
use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_proto::{ErrorCode, FEATURE_TERMINALS, RunId, TermEvent, TermOpenParams};

fn run(name: &str) -> RunId {
    RunId::from_suffix(name).unwrap()
}

fn open(name: &str) -> TermOpenParams {
    TermOpenParams {
        run_id: run(name),
        cols: 80,
        rows: 24,
        command: None,
    }
}

#[tokio::test(start_paused = true)]
async fn input_is_echoed_back_and_exit_ends_the_terminal() {
    let client = FakeDaemon::spawn().await.unwrap();
    assert!(client.supports(FEATURE_TERMINALS));
    let opened = client.term_open(open("builder1")).await.unwrap();
    assert_eq!(opened.shell, "zsh");

    let mut out = client.term_attach(opened.id.clone(), None).await.unwrap();
    client.term_write(opened.id.clone(), b"ls\r").await.unwrap();
    let echoed = out.next().await.unwrap().unwrap();
    assert_eq!(echoed.bytes().as_deref(), Some(&b"ls\r"[..]));
    assert_eq!(echoed.seq(), 1);

    client
        .term_resize(opened.id.clone(), 100, 30)
        .await
        .unwrap();
    client
        .term_write(opened.id.clone(), b"exit\r")
        .await
        .unwrap();
    let mut last = None;
    while let Some(event) = out.next().await {
        last = Some(event.unwrap());
    }
    assert!(
        matches!(last, Some(TermEvent::Exit { code: Some(0), .. })),
        "{last:?}"
    );
    let status = client.term_status(opened.id.clone()).await.unwrap();
    assert!(status.exited && !status.busy);
    // An exited terminal takes no input; closing forgets it.
    let error = client
        .term_write(opened.id.clone(), b"x")
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Conflict));
    client.term_close(opened.id.clone()).await.unwrap();
    let error = client.term_status(opened.id).await.unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::NotFound));
}

#[tokio::test(start_paused = true)]
async fn an_attach_replays_after_the_seq_it_is_given() {
    let client = FakeDaemon::spawn().await.unwrap();
    let opened = client.term_open(open("builder1")).await.unwrap();
    for word in ["one", "two", "three"] {
        client
            .term_write(opened.id.clone(), word.as_bytes())
            .await
            .unwrap();
    }
    let mut out = client
        .term_attach(opened.id.clone(), Some(1))
        .await
        .unwrap();
    let first = out.next().await.unwrap().unwrap();
    assert_eq!(first.seq(), 2);
    assert_eq!(first.bytes().as_deref(), Some(&b"two"[..]));
    let second = out.next().await.unwrap().unwrap();
    assert_eq!(second.seq(), 3);
    // Detached and back: only what is new.
    drop(out);
    client.term_write(opened.id.clone(), b"four").await.unwrap();
    let mut again = client.term_attach(opened.id, Some(3)).await.unwrap();
    assert_eq!(again.next().await.unwrap().unwrap().seq(), 4);
}

#[tokio::test(start_paused = true)]
async fn an_attach_older_than_what_is_held_starts_with_a_gap() {
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    let opened = client.term_open(open("builder1")).await.unwrap();
    for n in 0..(reno_client::fake::terminals::HELD + 10) {
        daemon.term_output(&opened.id, &format!("line {n}\r\n"));
    }
    let mut out = client.term_attach(opened.id, None).await.unwrap();
    let first = out.next().await.unwrap().unwrap();
    let TermEvent::Gap { lost_from, seq } = first else {
        panic!("a gap first: {first:?}");
    };
    assert_eq!(lost_from, 1);
    assert_eq!(seq, 11);
    assert_eq!(out.next().await.unwrap().unwrap().seq(), 11);
}

#[tokio::test(start_paused = true)]
async fn sandboxed_follows_the_run_and_stays_after_it() {
    let client = FakeDaemon::spawn().await.unwrap();
    // builder1 is running: its terminal is limited.
    let limited = client.term_open(open("builder1")).await.unwrap();
    assert!(limited.sandboxed && !limited.in_place);
    // reviewer1 has no worktree: its own folder, flagged, never limited.
    let folder = client.term_open(open("reviewer1")).await.unwrap();
    assert!(folder.in_place && !folder.sandboxed);
    // The run ends; the old terminal keeps its limits and a new one has none.
    client.cancel_run(&run("builder1"), None).await.unwrap();
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    let free = client.term_open(open("builder1")).await.unwrap();
    assert!(!free.sandboxed);
    let listed = client.term_list(run("builder1")).await.unwrap();
    assert_eq!(
        listed.iter().map(|t| t.sandboxed).collect::<Vec<_>>(),
        [true, false]
    );
    assert!(client.term_status(limited.id).await.unwrap().sandboxed);
}

#[tokio::test(start_paused = true)]
async fn the_fake_answers_like_the_daemon_for_a_removed_worktree_an_unknown_run_and_the_cap() {
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    assert_eq!(
        client.term_open(open("builder0")).await.unwrap_err().code(),
        Some(ErrorCode::WorkspaceUnavailable)
    );
    assert_eq!(
        client.term_open(open("nonesuch")).await.unwrap_err().code(),
        Some(ErrorCode::NotFound)
    );
    let mut ids = Vec::new();
    for _ in 0..reno_proto::TERM_MAX_TERMINALS {
        ids.push(client.term_open(open("builder1")).await.unwrap().id);
    }
    assert_eq!(
        client.term_open(open("builder1")).await.unwrap_err().code(),
        Some(ErrorCode::TerminalLimit)
    );
    daemon.set_term_busy(&ids[0], true);
    assert!(client.term_status(ids[0].clone()).await.unwrap().busy);
    assert_eq!(daemon.terminals(), reno_proto::TERM_MAX_TERMINALS);
}

#[tokio::test(start_paused = true)]
async fn a_client_refuses_up_front_when_the_daemon_has_no_terminals() {
    let config = FakeConfig {
        without_features: vec![FEATURE_TERMINALS.to_owned()],
        ..FakeConfig::default()
    };
    let daemon = FakeDaemon::start(config).unwrap();
    let client = daemon.connect().await.unwrap();
    assert!(!client.supports(FEATURE_TERMINALS));
    let error = client.term_open(open("builder1")).await.unwrap_err();
    assert!(error.to_string().contains("terminals"), "{error}");
    assert!(client.term_list(run("builder1")).await.is_err());
    assert_eq!(daemon.terminals(), 0);
}
