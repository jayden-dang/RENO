//! The daemon served in-process on a temp socket, on a multi-thread runtime, through the real
//! `reno-client`. No network, no real harness (ARCH-16).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Fixture, approval_subject, config, id, is_done, seqs, within};
use futures::StreamExt;
use reno_client::{Client, ClientError, ConnectionStatus, Mirror, MirrorConfig, State};
use reno_proto::{
    ApprovalDecision, DaemonHello, ErrorCode, ExecEvent, FEATURES, HelloParams, PROTOCOL_VERSION,
    RunListParams, RunStatus, Seq, StopReason, TaskRunParams,
};
use renod::lock::LockError;
use std::time::Duration;

use renod::{Daemon, DaemonError};

macro_rules! daemon_test {
    ($(#[$meta:meta])* async fn $name:ident() $body:block) => {
        $(#[$meta])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() $body
    };
}

fn code(error: &ClientError) -> ErrorCode {
    error
        .code()
        .unwrap_or_else(|| panic!("not a daemon error: {error:?}"))
}

daemon_test! {
    async fn hello_then_a_mock_run_streams_ordered_events_and_succeeds() {
        let fx = Fixture::start().await;

        let hello = fx.client.daemon();
        assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
        assert_eq!(hello.pid, std::process::id());
        assert_eq!(hello.server_version, env!("CARGO_PKG_VERSION"));
        for feature in FEATURES {
            assert!(fx.client.supports(feature), "{feature} was not negotiated");
        }

        let run = within(fx.client.run_task(TaskRunParams {
            prompt: "say hello\nand then goodbye".into(),
            executor: id("mock"),
            cwd: None,
            model: Some("mock-1".into()),
        }))
        .await
        .unwrap();
        let events = fx.events(&run, None).await;

        // Journals count from 1, without holes, and end in exactly one Done.
        assert_eq!(seqs(&events), (1..=events.len() as u64).collect::<Vec<_>>());
        assert!(matches!(events[0].event, ExecEvent::Started { .. }));
        let texts = events.iter().filter(|e| matches!(e.event, ExecEvent::Text { .. })).count();
        assert!(texts >= 3, "{texts} text deltas");
        assert!(events.iter().any(|e| matches!(e.event, ExecEvent::Tool { .. })));
        assert!(events.iter().any(|e| matches!(e.event, ExecEvent::ToolResult { ok: true, .. })));
        assert!(is_done(&events.last().unwrap().event, StopReason::Completed));
        assert_eq!(
            events.iter().filter(|e| matches!(e.event, ExecEvent::Done { .. })).count(),
            1
        );

        // The state commits before the Done reaches a subscriber.
        let summary = within(fx.client.get_run(&run)).await.unwrap();
        assert_eq!(summary.status, RunStatus::Succeeded);
        assert_eq!(summary.stop_reason, Some(StopReason::Completed));
        assert_eq!(summary.title, "say hello");
        assert_eq!(summary.prompt.as_deref(), Some("say hello\nand then goodbye"));
        assert_eq!(summary.model.as_deref(), Some("mock-1"));
        assert_eq!(summary.executor, id("mock"));

        let listed = within(fx.client.list_runs(RunListParams::default())).await.unwrap();
        assert_eq!(listed, vec![summary]);
        let only_failed = within(fx.client.list_runs(RunListParams {
            status: Some(vec![RunStatus::Failed]),
            ..RunListParams::default()
        }))
        .await
        .unwrap();
        assert!(only_failed.is_empty());

        let status = within(fx.client.status()).await.unwrap();
        assert_eq!(status.pid, std::process::id());
        assert_eq!((status.active_runs, status.queued_runs), (0, 0));
        let mut names: Vec<_> = status.executors.iter().map(|e| e.as_str().to_owned()).collect();
        names.sort();
        assert_eq!(names, ["mock", "mock-approval", "mock-burst", "mock-hold"]);
        fx.stop().await;
    }
}

daemon_test! {
    async fn run_events_resumes_from_a_mid_seq_inclusive() {
        let fx = Fixture::start().await;
        let run = fx.run("mock", "resume me").await;
        let all = fx.events(&run, None).await;
        assert!(all.len() >= 6);

        let from_three = fx.events(&run, Some(Seq(3))).await;
        assert_eq!(from_three.first().map(|e| e.seq), Some(Seq(3)), "inclusive");
        assert_eq!(from_three, all[2..]);

        // 0 and "absent" both mean the start.
        assert_eq!(fx.events(&run, Some(Seq(0))).await, all);
        assert_eq!(all.first().map(|e| e.seq), Some(Seq(1)));

        // Past the end: nothing, and the stream still ends cleanly.
        let past = Seq(all.last().unwrap().seq.0 + 1);
        assert!(fx.events(&run, Some(past)).await.is_empty());
        fx.stop().await;
    }
}

daemon_test! {
    async fn a_subscriber_that_the_kernel_cuts_off_resumes_without_a_hole() {
        // A queue of one event per subscriber: the 400-event burst overruns it, and the daemon
        // has to resume the stream from the kernel's `resume_from` for the client to see 1..N.
        let fx = Fixture::with(|config| config.kernel.subscriber_queue = 1).await;
        let run = fx.run("mock-burst", "burst").await;
        let events = fx.events(&run, None).await;
        assert_eq!(events.len(), 402);
        assert_eq!(seqs(&events), (1..=402).collect::<Vec<_>>());
        assert!(is_done(&events.last().unwrap().event, StopReason::Completed));
        fx.stop().await;
    }
}

daemon_test! {
    async fn cancel_ends_a_live_run_interrupted_and_steer_reaches_it() {
        let fx = Fixture::start().await;

        // A live run, held after its first text.
        let held = fx.run("mock-hold", "wait for me").await;
        let mut events = Box::pin(fx.client.run_events(held.clone(), None));
        let mut seen = Vec::new();
        while !seen.iter().any(|e: &ExecEvent| matches!(e, ExecEvent::Text { .. })) {
            seen.push(within(events.next()).await.unwrap().unwrap().event);
        }
        let status = within(fx.client.status()).await.unwrap();
        assert_eq!((status.active_runs, status.queued_runs), (1, 0));

        let cancelled = within(fx.client.cancel_run(&held, Some("test".into()))).await.unwrap();
        assert_eq!(cancelled, RunStatus::Running);
        while let Some(item) = within(events.next()).await {
            seen.push(item.unwrap().event);
        }
        assert!(is_done(seen.last().unwrap(), StopReason::Interrupted));
        assert_eq!(fx.status(&held).await, RunStatus::Interrupted);

        // Steering lets a held run finish, and the message is in its journal.
        let steered = fx.run("mock-hold", "steer me").await;
        let mut events = Box::pin(fx.client.run_events(steered.clone(), None));
        loop {
            let item = within(events.next()).await.unwrap().unwrap();
            if matches!(item.event, ExecEvent::Text { .. }) {
                break;
            }
        }
        within(fx.client.steer(&steered, "carry on".into())).await.unwrap();
        let mut rest = Vec::new();
        while let Some(item) = within(events.next()).await {
            rest.push(item.unwrap().event);
        }
        assert!(rest.iter().any(|e| matches!(e, ExecEvent::UserMessage { text } if text == "carry on")));
        assert!(is_done(rest.last().unwrap(), StopReason::Completed));
        assert_eq!(fx.status(&steered).await, RunStatus::Succeeded);
        fx.stop().await;
    }
}

daemon_test! {
    async fn an_approval_is_listed_watched_decided_and_the_run_goes_on() {
        let fx = Fixture::start().await;
        let mut watch = within(fx.client.watch_approvals()).await.unwrap();
        let first = within(watch.next()).await.unwrap().unwrap();
        assert!(first.approvals.is_empty(), "the first item is the current list");

        let run = fx.run("mock-approval", "delete the build").await;
        let pending = loop {
            let snapshot = within(watch.next()).await.unwrap().unwrap();
            if let Some(approval) = snapshot.approvals.first() {
                break approval.clone();
            }
        };
        assert_eq!(pending.run_id, run);
        assert_eq!(pending.subject, approval_subject());
        assert_eq!(fx.status(&run).await, RunStatus::WaitingApproval);
        assert_eq!(within(fx.client.list_approvals()).await.unwrap(), vec![pending.clone()]);

        within(fx.client.decide(&pending.id, ApprovalDecision::ApproveOnce)).await.unwrap();
        let events = fx.events(&run, None).await;
        assert!(events.iter().any(|e| matches!(&e.event, ExecEvent::Text { text } if text == "answered.")));
        assert!(is_done(&events.last().unwrap().event, StopReason::Completed));
        assert_eq!(fx.status(&run).await, RunStatus::Succeeded);
        assert!(within(fx.client.list_approvals()).await.unwrap().is_empty());

        // The executor got the decision, once; deciding again finds nothing.
        let answers = fx.executors.approval.log().answers;
        assert_eq!(answers.len(), 1);
        let again = within(fx.client.decide(&pending.id, ApprovalDecision::Deny)).await.unwrap_err();
        assert_eq!(code(&again), ErrorCode::NotFound);
        fx.stop().await;
    }
}

daemon_test! {
    async fn runs_watch_starts_with_the_list_and_follows_changes() {
        let fx = Fixture::start().await;
        let mut watch = within(fx.client.watch_runs()).await.unwrap();
        let first = within(watch.next()).await.unwrap().unwrap();
        assert!(first.runs.is_empty());

        let run = fx.run("mock", "watch me").await;
        loop {
            let snapshot = within(watch.next()).await.unwrap().unwrap();
            if snapshot.runs.iter().any(|r| r.run_id == run && r.status == RunStatus::Succeeded) {
                break;
            }
        }

        // A late watcher is handed the current list first.
        let mut late = within(fx.client.watch_runs()).await.unwrap();
        let current = within(late.next()).await.unwrap().unwrap();
        assert!(current.runs.iter().any(|r| r.run_id == run));
        fx.stop().await;
    }
}

daemon_test! {
    async fn the_mirror_the_app_uses_converges_on_the_daemon() {
        let fx = Fixture::start().await;
        let (mirror, mut updates) = Mirror::start(fx.client.clone(), MirrorConfig::default());
        let mut state = State::new();
        let run = fx.run("mock", "mirror me").await;
        assert!(mirror.follow_run(run.clone(), None));

        within(async {
            while let Some(update) = updates.next().await {
                state.apply(update);
                let finished = state.run(&run).is_some_and(|r| r.status == RunStatus::Succeeded);
                let closed = state.transcript(&run).is_some_and(|t| {
                    t.events.last().is_some_and(|(_, e)| is_done(e, StopReason::Completed))
                });
                if finished && closed {
                    break;
                }
            }
        })
        .await;

        assert_eq!(state.connection(), ConnectionStatus::Connected);
        assert_eq!(state.daemon().unwrap().hello.pid, std::process::id());
        let summary = state.run(&run).unwrap();
        assert_eq!(summary.prompt.as_deref(), Some("mirror me"));
        let transcript = state.transcript(&run).unwrap();
        let seqs: Vec<u64> = transcript.events.iter().map(|(seq, _)| seq.0).collect();
        assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
        assert_eq!(transcript.resume_from(), Seq(seqs.len() as u64 + 1));
        mirror.shutdown().await;
        fx.stop().await;
    }
}

daemon_test! {
    async fn daemon_shutdown_ends_cleanly_and_the_next_start_finds_the_run_interrupted() {
        // A long grace: the reply and the last events must arrive however slow the machine is, and
        // the server ends as soon as the last connection closes, not when the grace does.
        let fx = Fixture::with(|config| config.connection_grace = Duration::from_secs(30)).await;
        let held = fx.run("mock-hold", "still going").await;
        let mut events = Box::pin(fx.client.run_events(held.clone(), None));
        while !matches!(
            within(events.next()).await.unwrap().unwrap().event,
            ExecEvent::Text { .. }
        ) {}

        // The reply arrives, then the sequence runs: runs end interrupted, the subscriber sees
        // that, the socket goes and the lock is released.
        within(fx.client.shutdown_daemon()).await.unwrap();
        let mut tail = Vec::new();
        while let Some(item) = within(events.next()).await {
            tail.push(item.unwrap().event);
        }
        assert!(is_done(tail.last().unwrap(), StopReason::Interrupted));

        let Fixture { dir, paths, executors, daemon, client } = fx;
        drop((client, events));
        let stopped = within(daemon.wait()).await.unwrap();
        assert!(!stopped.kernel_aborted && !stopped.server_aborted, "{stopped:?}");
        assert!(!paths.socket.exists(), "the socket is removed");

        // The lock is free, and the store kept the run.
        let again = within(Daemon::start(config(&dir, &executors))).await.unwrap();
        let client = within(Client::connect_unix(&paths.socket)).await.unwrap();
        assert_eq!(within(client.get_run(&held)).await.unwrap().status, RunStatus::Interrupted);
        again.request_shutdown();
        within(again.wait()).await.unwrap();
    }
}

daemon_test! {
    async fn a_second_daemon_on_the_same_data_dir_is_refused_and_leaves_the_first_alone() {
        let fx = Fixture::start().await;
        let second = Daemon::start(config(&fx.dir, &fx.executors)).await;
        match second {
            Err(DaemonError::Lock(LockError::Held { holder, .. })) => {
                assert_eq!(holder, Some(std::process::id()));
            }
            other => panic!("expected the lock to be held, got {other:?}"),
        }
        // The first one still answers.
        within(fx.client.status()).await.unwrap();
        assert!(fx.paths.socket.exists());
        fx.stop().await;
    }
}

daemon_test! {
    async fn every_handler_maps_its_errors_to_a_stable_code() {
        let fx = Fixture::start().await;
        let missing = reno_proto::RunId::from_suffix("nothere").unwrap();
        let done = fx.run("mock", "finish first").await;
        fx.events(&done, None).await;

        // run.get / run.cancel / run.steer / run.events on a run that does not exist.
        let e = within(fx.client.get_run(&missing)).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound);
        let e = within(fx.client.cancel_run(&missing, None)).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound);
        let e = within(fx.client.steer(&missing, "x".into())).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound);
        let first = within(Box::pin(fx.client.run_events(missing.clone(), None)).next()).await;
        let e = first.unwrap().unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound);

        // A run that is over has nothing to cancel or steer.
        let e = within(fx.client.cancel_run(&done, None)).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Conflict);
        let e = within(fx.client.steer(&done, "late".into())).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Conflict);
        let e = within(fx.client.steer(&done, "  ".into())).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams);

        // approval.decide on an approval that is not there.
        let nothing = reno_proto::ApprovalId::from_suffix("nothere").unwrap();
        let e = within(fx.client.decide(&nothing, ApprovalDecision::Deny)).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound);

        // task.run: unknown executor, blank prompt, bad cwd.
        let run = |prompt: &str, executor: &str, cwd: Option<&str>| TaskRunParams {
            prompt: prompt.into(),
            executor: id(executor),
            cwd: cwd.map(Into::into),
            model: None,
        };
        let e = within(fx.client.run_task(run("hi", "no-such-executor", None))).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound);
        assert!(e.to_string().contains("mock"), "names what is available: {e}");
        let e = within(fx.client.run_task(run("   ", "mock", None))).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams);
        let e = within(fx.client.run_task(run("hi", "mock", Some("relative/dir")))).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams);
        let gone = fx.dir.path().join("does-not-exist");
        let e = within(fx.client.run_task(run("hi", "mock", gone.to_str()))).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams);
        // A real directory is fine.
        let ok = fx.dir.path().to_str().unwrap().to_owned();
        within(fx.client.run_task(run("hi", "mock", Some(&ok)))).await.unwrap();

        // daemon.hello from a client that speaks no protocol this daemon knows.
        let rpc = reno_rpc::connect_unix(&fx.paths.socket, tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        let old = within(rpc.call::<DaemonHello>(HelloParams {
            client_name: "ancient".into(),
            protocol_version: 0,
            features: vec![],
        }))
        .await
        .unwrap_err();
        match old {
            reno_rpc::RpcError::Remote(object) => {
                assert_eq!(object.code, ErrorCode::ProtocolVersionMismatch);
            }
            other => panic!("expected a remote error, got {other:?}"),
        }
        // A client that offers fewer features is told only about those.
        let few = within(rpc.call::<DaemonHello>(HelloParams {
            client_name: "few".into(),
            protocol_version: PROTOCOL_VERSION,
            features: vec!["runs.watch".into(), "unknown.feature".into()],
        }))
        .await
        .unwrap();
        assert_eq!(few.features, ["runs.watch"]);
        fx.stop().await;
    }
}

daemon_test! {
    async fn the_socket_is_private_and_a_leftover_socket_is_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let fx = Fixture::start().await;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&fx.paths.socket), 0o600);
        assert_eq!(mode(&fx.paths.data_dir), 0o700);
        let (dir, paths, stopped) = fx.stop().await;
        assert!(!stopped.kernel_aborted);
        assert!(!paths.socket.exists());

        // A socket file left by a daemon that died does not stop the next one.
        let stale = std::os::unix::net::UnixListener::bind(&paths.socket).unwrap();
        drop(stale);
        assert!(paths.socket.exists());
        let executors = common::Executors::new();
        let daemon = within(Daemon::start(config(&dir, &executors))).await.unwrap();
        let client = within(Client::connect_unix(daemon.socket())).await.unwrap();
        within(client.status()).await.unwrap();
        daemon.request_shutdown();
        within(daemon.wait()).await.unwrap();
    }
}
