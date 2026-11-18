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
            // Offered only by a daemon given the means to install and update (`executor_jobs`).
            if *feature == reno_proto::FEATURE_EXECUTOR_MAINTAIN {
                continue;
            }
            assert!(fx.client.supports(feature), "{feature} was not negotiated");
        }

        let run = within(fx.client.run_task(TaskRunParams { thread_id: None, model_options: Default::default(),
            effort: None,
            prompt: "say hello\nand then goodbye".into(),
            executor: Some(id("mock")),
            bot: None,
            cwd: None,
            model: Some("mock-1".into()),
            in_place: false,
            attachments: Vec::new(),
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
        assert_eq!(names, [
                "mock",
                "mock-approval",
                "mock-burst",
                "mock-danger",
                "mock-hold",
                "mock-model"
            ]);
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
    async fn an_effort_no_model_lists_is_refused_as_invalid_params_and_nothing_runs() {
        let fx = Fixture::start().await;
        let e = within(fx.client.run_task(TaskRunParams { thread_id: None, model_options: Default::default(),
            effort: Some(reno_proto::Effort::High),
            prompt: "think hard".into(),
            executor: Some(id("mock")),
            bot: None,
            cwd: None,
            model: Some("mock-1".into()),
            in_place: false,
            attachments: Vec::new(),
        }))
        .await
        .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams);
        let message = format!("{e}");
        assert!(message.contains("mock-1") && message.contains("high"), "{message}");
        assert!(fx.client.supports(reno_proto::FEATURE_RUN_EFFORT));
    }
}

daemon_test! {
    async fn set_model_switches_a_live_run_and_refuses_a_model_the_executor_lacks() {
        let fx = Fixture::start().await;
        let run = fx.run("mock-model", "switch me").await;
        let mut events = Box::pin(fx.client.run_events(run.clone(), None));
        loop {
            let item = within(events.next()).await.unwrap().unwrap();
            if matches!(item.event, ExecEvent::Text { .. }) {
                break;
            }
        }
        // A model the executor does not list is refused, and nothing changes.
        let e = within(fx.client.set_run_model(&run, "no-such-model".into())).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams);
        assert_eq!(fx.client.get_run(&run).await.unwrap().model, None);

        let switched = within(fx.client.set_run_model(&run, "mock-2".into())).await.unwrap();
        assert_eq!(switched.model, "mock-2");
        assert_eq!(fx.client.get_run(&run).await.unwrap().model.as_deref(), Some("mock-2"));
        let mut rest = Vec::new();
        while let Some(item) = within(events.next()).await {
            rest.push(item.unwrap().event);
        }
        let changes: Vec<_> = rest
            .iter()
            .filter(|e| matches!(e, ExecEvent::ModelChanged { model } if model == "mock-2"))
            .collect();
        assert_eq!(changes.len(), 1, "{rest:?}");
        assert!(is_done(rest.last().unwrap(), StopReason::Completed));
        fx.stop().await;
    }
}

daemon_test! {
    async fn cancel_ends_a_live_run_cancelled_and_steer_reaches_it() {
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
        // The reply carries the status at answer time; a fast executor may already have stopped.
        assert!(matches!(cancelled, RunStatus::Running | RunStatus::Cancelled), "{cancelled:?}");
        while let Some(item) = within(events.next()).await {
            seen.push(item.unwrap().event);
        }
        assert!(is_done(seen.last().unwrap(), StopReason::Interrupted));
        assert_eq!(fx.status(&held).await, RunStatus::Cancelled);

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

        // The decision is on record: who made it, and the transcript says so.
        let history = within(fx.client.approval_history(Some(run.clone()), None)).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, pending.id);
        assert_eq!(history[0].status, reno_proto::ApprovalStatus::Approved);
        assert_eq!(history[0].decided_by, Some(reno_proto::ResolvedBy::Human));
        assert!(events.iter().any(|e| matches!(
            &e.event,
            ExecEvent::ApprovalResolved { request_id, decision: reno_proto::ApprovalOutcome::Approved, by: reno_proto::ResolvedBy::Human, .. }
                if request_id == "req-1"
        )));
        fx.stop().await;
    }
}

daemon_test! {
    async fn a_denial_with_a_reason_reaches_the_executor_the_journal_and_the_history() {
        let fx = Fixture::start().await;
        let mut watch = within(fx.client.watch_approvals()).await.unwrap();
        let _ = within(watch.next()).await;
        let run = fx.run("mock-approval", "delete the build").await;
        let pending = loop {
            let snapshot = within(watch.next()).await.unwrap().unwrap();
            if let Some(approval) = snapshot.approvals.first() {
                break approval.clone();
            }
        };

        // A reason with an approval, or none that fits, is refused and the approval stays open.
        let with_approve = within(fx.client.decide_with_message(
            &pending.id,
            ApprovalDecision::ApproveOnce,
            Some("because".into()),
        ))
        .await
        .unwrap_err();
        assert_eq!(code(&with_approve), ErrorCode::InvalidParams);
        assert_eq!(within(fx.client.list_approvals()).await.unwrap().len(), 1);

        within(fx.client.decide_with_message(
            &pending.id,
            ApprovalDecision::Deny,
            Some("push to a branch, not main".into()),
        ))
        .await
        .unwrap();
        let events = fx.events(&run, None).await;
        let answers = fx.executors.approval.log().answers;
        assert_eq!(
            answers[0].answer,
            reno_harness::exec::Answer::Decision(
                reno_harness::exec::Decision::Deny,
                Some("push to a branch, not main".into())
            )
        );
        assert!(events.iter().any(|e| matches!(
            &e.event,
            ExecEvent::ApprovalResolved { decision: reno_proto::ApprovalOutcome::Denied, by: reno_proto::ResolvedBy::Human, message: Some(m), .. }
                if m == "push to a branch, not main"
        )));
        let history = within(fx.client.approval_history(Some(run), None)).await.unwrap();
        assert_eq!(history[0].message.as_deref(), Some("push to a branch, not main"));
        fx.stop().await;
    }
}

daemon_test! {
    async fn the_default_policy_denies_a_dangerous_command_without_asking_anyone() {
        let fx = Fixture::start().await;
        let mut watch = within(fx.client.watch_approvals()).await.unwrap();
        assert!(within(watch.next()).await.unwrap().unwrap().approvals.is_empty());

        let run = fx.run("mock-danger", "clean up").await;
        let events = fx.events(&run, None).await;
        assert!(is_done(&events.last().unwrap().event, StopReason::Completed));
        // The harness was told no, by the rule, and never waited for a person.
        assert_eq!(fx.executors.danger.log().answers.len(), 1);
        let resolved: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.event {
                ExecEvent::ApprovalResolved { decision, by, message, .. } => Some((*decision, by.clone(), message.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            resolved,
            [(
                reno_proto::ApprovalOutcome::Denied,
                reno_proto::ResolvedBy::Rule { name: "recursive force delete".into() },
                Some("denied by policy: recursive force delete".into()),
            )]
        );
        assert!(within(fx.client.list_approvals()).await.unwrap().is_empty());
        let history = within(fx.client.approval_history(None, Some(10))).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, reno_proto::ApprovalStatus::Denied);
        assert_eq!(
            history[0].decided_by,
            Some(reno_proto::ResolvedBy::Rule { name: "recursive force delete".into() })
        );
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
    async fn daemon_shutdown_ends_cleanly_and_the_next_start_resumes_the_run() {
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
        assert!(matches!(
            tail.last(),
            Some(ExecEvent::Done { reason: Some(reno_proto::StopDetail::Shutdown), .. })
        ));

        let Fixture { dir, paths, executors, daemon, client } = fx;
        drop((client, events));
        let stopped = within(daemon.wait()).await.unwrap();
        assert!(!stopped.kernel_aborted && !stopped.server_aborted, "{stopped:?}");
        assert!(!paths.socket.exists(), "the socket is removed");

        // The lock is free, and the store kept the run: nobody asked it to stop, so the next
        // start put it back, and its journal says so.
        let again = within(Daemon::start(config(&dir, &executors))).await.unwrap();
        let client = within(Client::connect_unix(&paths.socket)).await.unwrap();
        let journal: Vec<_> =
            within(client.run_events(held.clone(), None).take(4).collect::<Vec<_>>()).await;
        let resumed = journal.into_iter().map(|i| i.unwrap().event).next_back().unwrap();
        assert!(matches!(resumed, ExecEvent::Resumed { from_attempt: 1, .. }), "{resumed:?}");
        assert_eq!(within(client.get_run(&held)).await.unwrap().attempt, 2);
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

        // A run that is over has nothing to cancel or steer. Its journal ends with `Done` a
        // moment before the daemon lets go of the run, and a cancel that lands in between is
        // still accepted, so ask until the daemon has.
        let e = within(async {
            loop {
                match fx.client.cancel_run(&done, None).await {
                    Ok(_) => tokio::task::yield_now().await,
                    Err(error) => return error,
                }
            }
        })
        .await;
        assert_eq!(code(&e), ErrorCode::Conflict);
        let e = within(fx.client.steer(&done, "late".into())).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Conflict);
        let e = within(fx.client.steer(&done, "  ".into())).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams);

        // run.set_model on a run that does not exist, and on one that is over.
        let e = within(fx.client.set_run_model(&missing, "mock-2".into())).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound);
        let e = within(fx.client.set_run_model(&done, "mock-2".into())).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Conflict);
        let e = within(fx.client.set_run_model(&done, " ".into())).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams);

        // run.configure on a run that does not exist, one that is over, and with nothing to change.
        let clear = Some(reno_proto::EffortChange::Clear);
        let e = within(fx.client.configure_run(&missing, clear, Default::default())).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound);
        let e = within(fx.client.configure_run(&done, clear, Default::default())).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Conflict);
        let e = within(fx.client.configure_run(&done, None, Default::default())).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams);

        // approval.decide on an approval that is not there.
        let nothing = reno_proto::ApprovalId::from_suffix("nothere").unwrap();
        let e = within(fx.client.decide(&nothing, ApprovalDecision::Deny)).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound);

        // task.run: unknown executor, blank prompt, bad cwd.
        let run = |prompt: &str, executor: &str, cwd: Option<&str>| TaskRunParams { thread_id: None, model_options: Default::default(),
            effort: None,
            prompt: prompt.into(),
            executor: Some(id(executor)),
            bot: None,
            cwd: cwd.map(Into::into),
            model: None,
            in_place: false,
            attachments: Vec::new(),
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

daemon_test! {
    async fn a_remembered_answer_is_a_rule_the_next_run_meets_until_it_is_revoked() {
        use reno_proto::{ApprovalStatus, Remember, RememberScope, ResolvedBy, StandingEffect, StandingScope};
        let fx = Fixture::start().await;
        assert!(fx.client.supports(reno_proto::FEATURE_RULES));
        assert!(within(fx.client.list_rules(true)).await.unwrap().is_empty());

        let mut watch = within(fx.client.watch_approvals()).await.unwrap();
        let _ = within(watch.next()).await;
        let first = fx.run("mock-approval", "delete the build").await;
        let pending = loop {
            let snapshot = within(watch.next()).await.unwrap().unwrap();
            if let Some(approval) = snapshot.approvals.first() {
                break approval.clone();
            }
        };
        // A scope that cannot be kept (the run has no bot) is refused and the approval stays held.
        let refused = within(fx.client.decide_remembering(
            &pending.id,
            ApprovalDecision::ApproveOnce,
            None,
            Some(Remember { scope: RememberScope::Bot, expires_in_secs: None }),
        ))
        .await
        .unwrap_err();
        assert_eq!(code(&refused), ErrorCode::InvalidParams);
        assert_eq!(within(fx.client.list_approvals()).await.unwrap().len(), 1);

        within(fx.client.decide_remembering(
            &pending.id,
            ApprovalDecision::ApproveOnce,
            None,
            Some(Remember { scope: RememberScope::Always, expires_in_secs: Some(3600) }),
        ))
        .await
        .unwrap();
        let _ = fx.events(&first, None).await;
        let rules = within(fx.client.list_rules(false)).await.unwrap();
        assert_eq!(rules.len(), 1);
        let rule = rules[0].clone();
        assert_eq!((rule.scope.clone(), rule.effect), (StandingScope::Always, StandingEffect::Allow));
        assert_eq!(rule.source_approval, Some(pending.id.clone()));
        assert_eq!(rule.matcher.tool, approval_subject().tool);

        // The same action in the next run is answered by the rule: nobody is held.
        let second = fx.run("mock-approval", "delete the build again").await;
        let events = fx.events(&second, None).await;
        assert!(is_done(&events.last().unwrap().event, StopReason::Completed));
        assert!(within(fx.client.list_approvals()).await.unwrap().is_empty());
        let history = within(fx.client.approval_history(Some(second.clone()), None)).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, ApprovalStatus::Approved);
        assert_eq!(
            history[0].decided_by,
            Some(ResolvedBy::Rule { name: format!("standing:{}", rule.id) })
        );
        assert_eq!(history[0].subject_digest.as_deref(), Some(approval_subject().digest().as_str()));

        // Revoked, it no longer answers: the third run is held again.
        within(fx.client.revoke_rule(&rule.id)).await.unwrap();
        assert!(within(fx.client.list_rules(false)).await.unwrap().is_empty());
        assert!(within(fx.client.list_rules(true)).await.unwrap()[0].revoked_at_ms.is_some());
        let unknown = reno_proto::RuleId::from_suffix("nope").unwrap();
        let missing = within(fx.client.revoke_rule(&unknown)).await.unwrap_err();
        assert_eq!(code(&missing), ErrorCode::NotFound);
        let third = fx.run("mock-approval", "delete the build a third time").await;
        let held = loop {
            let snapshot = within(watch.next()).await.unwrap().unwrap();
            if let Some(approval) = snapshot.approvals.first() {
                break approval.clone();
            }
        };
        assert_eq!(held.run_id, third);
        fx.stop().await;
    }
}

daemon_test! {
    async fn project_rules_are_made_listed_matched_and_revoked_over_rpc() {
        use reno_proto::{
            ApprovalStatus, Remember, RememberScope, ResolvedBy, RuleAddParams, StandingEffect,
            StandingScope,
        };
        let fx = Fixture::start().await;
        assert!(fx.client.supports(reno_proto::FEATURE_PROJECT_RULES));
        // A repository root (the daemon looks for `.git`) with a directory inside it, and a
        // directory that is in no repository.
        let dir = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(dir.path()).unwrap().join("porch");
        let inside = repo.join("src");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&inside).unwrap();
        let task = |cwd: &std::path::Path, prompt: &str| TaskRunParams { thread_id: None, model_options: Default::default(),
            effort: None,
            prompt: prompt.into(),
            executor: Some(id("mock-approval")),
            bot: None,
            cwd: Some(cwd.to_path_buf()),
            model: None,
            in_place: true,
            attachments: Vec::new(),
        };

        // Not in a repository: `rule.add` and `remember: project` are refused; listing is empty.
        let nowhere_dir = tempfile::tempdir().unwrap();
        let nowhere = std::fs::canonicalize(nowhere_dir.path()).unwrap();
        let refused = within(fx.client.add_rule(RuleAddParams {
            project: nowhere.clone(),
            effect: StandingEffect::Allow,
            command: "ls".into(),
            tool: None,
            expires_in_secs: None,
        }))
        .await
        .unwrap_err();
        assert_eq!(code(&refused), ErrorCode::InvalidParams);
        assert!(within(fx.client.list_project_rules(&nowhere, true)).await.unwrap().is_empty());

        let mut watch = within(fx.client.watch_approvals()).await.unwrap();
        let _ = within(watch.next()).await;
        let first = within(fx.client.run_task(task(&inside, "delete the build"))).await.unwrap();
        let pending = loop {
            let snapshot = within(watch.next()).await.unwrap().unwrap();
            if let Some(approval) = snapshot.approvals.first() {
                break approval.clone();
            }
        };
        within(fx.client.decide_remembering(
            &pending.id,
            ApprovalDecision::ApproveOnce,
            None,
            Some(Remember { scope: RememberScope::Project, expires_in_secs: None }),
        ))
        .await
        .unwrap();
        let _ = fx.events(&first, None).await;

        // The rule holds the canonical repository root, however the run named its directory.
        let rules = within(fx.client.list_project_rules(&repo, false)).await.unwrap();
        assert_eq!(rules.len(), 1);
        let rule = rules[0].clone();
        assert_eq!(rule.scope, StandingScope::Project { root: repo.clone() });
        assert_eq!(
            within(fx.client.list_project_rules(&inside, false)).await.unwrap(),
            rules
        );
        assert_eq!(within(fx.client.list_rules(false)).await.unwrap(), rules);

        // The next run in the project, from the root, meets it; one outside does not.
        let second = within(fx.client.run_task(task(&repo, "again"))).await.unwrap();
        let events = fx.events(&second, None).await;
        assert!(is_done(&events.last().unwrap().event, StopReason::Completed));
        let history = within(fx.client.approval_history(Some(second.clone()), None)).await.unwrap();
        assert_eq!(history[0].status, ApprovalStatus::Approved);
        assert_eq!(
            history[0].decided_by,
            Some(ResolvedBy::Rule { name: format!("standing:{}", rule.id) })
        );
        let elsewhere = within(fx.client.run_task(task(&nowhere, "elsewhere"))).await.unwrap();
        let held = loop {
            let snapshot = within(watch.next()).await.unwrap().unwrap();
            if let Some(approval) = snapshot.approvals.first() {
                break approval.clone();
            }
        };
        assert_eq!(held.run_id, elsewhere);
        // `remember: project` for a run in no project is refused and the approval stays held.
        let refused = within(fx.client.decide_remembering(
            &held.id,
            ApprovalDecision::ApproveOnce,
            None,
            Some(Remember { scope: RememberScope::Project, expires_in_secs: None }),
        ))
        .await
        .unwrap_err();
        assert_eq!(code(&refused), ErrorCode::InvalidParams);
        assert_eq!(within(fx.client.list_approvals()).await.unwrap().len(), 1);
        within(fx.client.decide(&held.id, ApprovalDecision::Deny)).await.unwrap();
        let _ = fx.events(&elsewhere, None).await;

        // `rule.add`: a deny for the project, made from any directory in it, beats the allow.
        let added = within(fx.client.add_rule(RuleAddParams {
            project: inside.clone(),
            effect: StandingEffect::Deny,
            command: "git push origin main".into(),
            tool: Some("bash".into()),
            expires_in_secs: Some(3600),
        }))
        .await
        .unwrap();
        assert_eq!(added.scope, StandingScope::Project { root: repo.clone() });
        assert_eq!(added.effect, StandingEffect::Deny);
        assert!(added.expires_at_ms.is_some());
        assert_eq!(within(fx.client.list_project_rules(&repo, false)).await.unwrap().len(), 2);
        let third = within(fx.client.run_task(task(&inside, "a third time"))).await.unwrap();
        let _ = fx.events(&third, None).await;
        let history = within(fx.client.approval_history(Some(third.clone()), None)).await.unwrap();
        assert_eq!(history[0].status, ApprovalStatus::Denied);
        assert_eq!(
            history[0].decided_by,
            Some(ResolvedBy::Rule { name: format!("standing:{}", added.id) })
        );

        // Revoked rules leave the project's list (and come back with `include_inactive`).
        within(fx.client.revoke_rule(&added.id)).await.unwrap();
        within(fx.client.revoke_rule(&rule.id)).await.unwrap();
        assert!(within(fx.client.list_project_rules(&repo, false)).await.unwrap().is_empty());
        assert_eq!(within(fx.client.list_project_rules(&repo, true)).await.unwrap().len(), 2);
    }
}
