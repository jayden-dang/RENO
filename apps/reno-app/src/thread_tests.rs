//! Threads in the app, against the fake daemon: a follow-up continues its thread, the thread is
//! one transcript with a divider, the sidebar lists threads, Review compares a run or the
//! thread, a thread closes, and an older daemon keeps today's behaviour.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_client::fake::FakeConfig;
use reno_proto::{ErrorCode, ErrorObject, RunId, RunStatus, ThreadId};

use crate::app::RootView;
use crate::panel::PanelKey;
use crate::panel::saved::Saved;
use crate::panel::surface::Surface;
use crate::screens::run::RunScreen;
use crate::store::Store;
use crate::tests::{
    approvals, click, draw, fake_store, kinds, painted, rows_of, run_id, run_status,
    send_from_home, wait_drawn, wait_until, window,
};
use crate::transcript::Row;
use crate::ui::theme::Appearance;
use crate::vm::changes::{Loaded, ReviewScope};

fn thread(name: &str) -> ThreadId {
    ThreadId::from_suffix(name).unwrap()
}

fn store_with(cx: &mut TestAppContext, mut config: FakeConfig) -> Entity<Store> {
    crate::tests::one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    config.step = std::time::Duration::ZERO;
    cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(config, cx);
        store
    })
}

fn without_threads() -> FakeConfig {
    FakeConfig {
        without_features: vec![reno_proto::FEATURE_THREADS.to_owned()],
        ..FakeConfig::default()
    }
}

fn screen_of(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> Entity<RunScreen> {
    root.read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("a run screen")
}

fn current_run(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> RunId {
    let screen = screen_of(vcx, root);
    screen.read_with(vcx, |s, _| s.run_id().clone())
}

/// Sends `prompt` from Home, waits for the run to end, then sends `follow` from its box; returns
/// the first and the second run once the second one's screen is open.
fn two_runs(
    vcx: &mut VisualTestContext,
    root: &Entity<RootView>,
    store: &Entity<Store>,
    prompt: &str,
    follow: &str,
) -> (RunId, RunId) {
    let first = send_from_home(vcx, root, prompt);
    wait_drawn(vcx, |vcx| {
        run_status(vcx, store, &first).is_some_and(RunStatus::is_terminal)
    });
    let steer = screen_of(vcx, root).read_with(vcx, |s, _| s.steer_input().clone());
    steer.update_in(vcx, |c, window, cx| c.fill(follow, window, cx));
    draw(vcx);
    click(vcx, "send");
    wait_drawn(vcx, |vcx| current_run(vcx, root) != first);
    let second = current_run(vcx, root);
    (first, second)
}

/// Runs sent from Home have no repository: they are in "No project", closed by default.
fn open_scratch_folder(vcx: &mut VisualTestContext) {
    wait_drawn(vcx, |vcx| painted(vcx, "folder-No-project"));
    click(vcx, "folder-No-project");
    draw(vcx);
}

#[gpui::test]
fn a_follow_up_continues_the_thread_and_the_transcript_is_one(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let (first, second) = two_runs(vcx, &root, &store, "first task", "second task");

    // The daemon put the second run in the first one's thread.
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| {
            s.thread_of(&second).is_some() && s.thread_of(&second) == s.thread_of(&first)
        })
    });
    store.read_with(vcx, |s, _| {
        let thread = s.thread_of(&second).unwrap();
        assert_eq!(s.thread_run_ids(&second), [first.clone(), second.clone()]);
        assert_eq!(s.state().thread(&thread).unwrap().runs.len(), 2);
    });

    // One transcript: the first run's rows, a divider, then the second run's.
    wait_drawn(vcx, |vcx| {
        let kinds = kinds(&rows_of(vcx, &root));
        kinds.contains(&"divider") && kinds.last() == Some(&"done")
    });
    let rows = rows_of(vcx, &root);
    let at = rows
        .iter()
        .position(|r| matches!(r, Row::Divider { .. }))
        .unwrap();
    assert!(
        matches!(&rows[0], Row::User { text, .. } if text == "first task"),
        "{:?}",
        kinds(&rows)
    );
    assert!(
        matches!(&rows[at + 1], Row::User { text, .. } if text == "second task"),
        "the second run's prompt follows its divider"
    );
    assert_eq!(
        rows.iter()
            .filter(|r| matches!(r, Row::Done { .. }))
            .count(),
        2,
        "each run ends with its own line"
    );
    let mut keys: Vec<_> = rows.iter().map(Row::key).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), rows.len(), "no two rows share a key");
    assert!(painted(vcx, "run-divider"));
}

#[gpui::test]
fn the_older_runs_are_not_built_again_on_every_change(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let (_first, _second) = two_runs(vcx, &root, &store, "first task", "second task");
    wait_drawn(vcx, |vcx| {
        let kinds = kinds(&rows_of(vcx, &root));
        kinds.contains(&"divider") && kinds.last() == Some(&"done")
    });
    let screen = screen_of(vcx, &root);
    let before = screen.read_with(vcx, |s, _| s.older_builds());
    assert!(before >= 1);
    for _ in 0..5 {
        store.update(vcx, |_, cx| cx.notify());
        draw(vcx);
    }
    assert_eq!(
        screen.read_with(vcx, |s, _| s.older_builds()),
        before,
        "a finished run's rows come from the cache"
    );
}

#[gpui::test]
fn a_follow_up_keeps_the_panel_of_its_thread(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let dir = tempfile::tempdir().unwrap();
    let at = crate::panel::persist::path_in(dir.path());
    cx.update(|cx| Saved::install(at, cx));
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let first = send_from_home(vcx, &root, "first task");
    wait_drawn(vcx, |vcx| {
        run_status(vcx, &store, &first).is_some_and(RunStatus::is_terminal)
    });
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| s.thread_of(&first).is_some())
    });
    let key = store.read_with(vcx, |s, _| {
        PanelKey(s.thread_of(&first).unwrap().to_string())
    });
    wait_drawn(vcx, |vcx| {
        screen_of(vcx, &root).read_with(vcx, |s, _| s.panel_key() == &key)
    });
    // A tab the person opened.
    root.update_in(vcx, |r, window, cx| {
        let screen = r.run_screen().cloned().unwrap();
        screen.update(cx, |s, cx| {
            s.open_surface(Surface::File("a.rs".into()), window, cx)
        });
    });
    draw(vcx);

    let steer = screen_of(vcx, &root).read_with(vcx, |s, _| s.steer_input().clone());
    steer.update_in(vcx, |c, window, cx| c.fill("more", window, cx));
    draw(vcx);
    click(vcx, "send");
    wait_drawn(vcx, |vcx| current_run(vcx, &root) != first);

    let screen = screen_of(vcx, &root);
    assert_eq!(screen.read_with(vcx, |s, _| s.panel_key().clone()), key);
    assert!(
        screen.read_with(vcx, |s, _| s
            .panel_state()
            .tabs()
            .contains(&Surface::File("a.rs".into()))),
        "the tabs carry over to the follow-up run"
    );
}

#[gpui::test]
fn an_old_daemon_starts_a_new_run_and_keys_the_panel_by_the_run(cx: &mut TestAppContext) {
    let store = store_with(cx, without_threads());
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let first = send_from_home(vcx, &root, "first task");
    wait_drawn(vcx, |vcx| {
        run_status(vcx, &store, &first).is_some_and(RunStatus::is_terminal)
    });
    let screen = screen_of(vcx, &root);
    assert_eq!(
        screen.read_with(vcx, |s, _| s.panel_key().clone()),
        PanelKey(first.to_string())
    );
    // The note says what it said before threads.
    assert!(painted(vcx, "composer-note"));
    let steer = screen.read_with(vcx, |s, _| s.steer_input().clone());
    steer.update_in(vcx, |c, window, cx| c.fill("again", window, cx));
    draw(vcx);
    click(vcx, "send");
    wait_drawn(vcx, |vcx| current_run(vcx, &root) != first);
    let second = current_run(vcx, &root);
    store.read_with(vcx, |s, _| {
        assert!(!s.supports_threads());
        assert_eq!(s.thread_run_ids(&second), std::slice::from_ref(&second));
    });
    // Its transcript is its own: no divider.
    assert!(!kinds(&rows_of(vcx, &root)).contains(&"divider"));
}

#[gpui::test]
fn a_thread_that_went_busy_or_gone_says_so_under_the_box(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let daemon = store.read_with(cx, |s, _| s.fake().cloned().unwrap());
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let first = send_from_home(vcx, &root, "first task");
    wait_drawn(vcx, |vcx| {
        run_status(vcx, &store, &first).is_some_and(RunStatus::is_terminal)
    });
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| s.thread_of(&first).is_some())
    });
    let screen = screen_of(vcx, &root);
    let steer = screen.read_with(vcx, |s, _| s.steer_input().clone());

    daemon.fail(
        "task.run",
        ErrorObject::new(
            ErrorCode::Conflict,
            "thread thr_x has a run in progress (run_y)",
        ),
    );
    steer.update_in(vcx, |c, window, cx| c.fill("again", window, cx));
    draw(vcx);
    click(vcx, "send");
    wait_drawn(vcx, |vcx| {
        steer.read_with(vcx, |c, _| {
            c.refusal_note() == Some(crate::vm::threads::Refusal::Busy)
        })
    });
    assert_eq!(steer.read_with(vcx, |c, _| c.text().to_owned()), "again");
    assert!(painted(vcx, "composer-note"));

    daemon.fail(
        "task.run",
        ErrorObject::new(
            ErrorCode::Conflict,
            "thread thr_x is closed and its worktree is gone; start a new one",
        ),
    );
    click(vcx, "send");
    wait_drawn(vcx, |vcx| {
        steer.read_with(vcx, |c, _| {
            c.refusal_note() == Some(crate::vm::threads::Refusal::Gone)
        })
    });
    // The next message starts a thread of its own.
    daemon.clear_failures();
    click(vcx, "send");
    wait_drawn(vcx, |vcx| current_run(vcx, &root) != first);
    let second = current_run(vcx, &root);
    store.read_with(vcx, |s, _| {
        assert_ne!(s.thread_of(&second), s.thread_of(&first));
    });
}

#[gpui::test]
fn a_thread_whose_worktree_is_gone_says_a_new_thread_starts(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    // builder0's worktree was removed.
    root.update_in(vcx, |r, window, cx| {
        r.open_run(run_id("builder0"), window, cx)
    });
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| {
            s.state()
                .thread(&thread("builder0"))
                .is_some_and(crate::vm::threads::worktree_gone)
        })
    });
    let note =
        screen_of(vcx, &root).read_with(vcx, |s, cx| s.steer_input().read(cx).note_for_test(cx));
    assert!(note.contains("starts a new thread"), "{note}");
}

#[test]
fn the_ended_note_says_what_a_message_does() {
    use crate::ui::composer::ended_note;
    assert_eq!(
        ended_note("This run passed.", "porch", true, false),
        "This run passed. What you send continues this thread in porch."
    );
    assert_eq!(
        ended_note("This run passed.", "porch", false, false),
        "This run passed. What you send starts a new run in porch."
    );
    assert!(ended_note("x", "porch", true, true).contains("starts a new thread in porch"));
}

#[gpui::test]
fn the_sidebar_lists_a_thread_once_and_a_click_opens_its_latest_run(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let (first, second) = two_runs(vcx, &root, &store, "first task", "second task");
    open_scratch_folder(vcx);
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| s.thread_of(&first) == s.thread_of(&second))
            && painted(vcx, &format!("recent-{second}"))
    });
    assert!(
        !painted(vcx, &format!("recent-{first}")),
        "the first run is not a row of its own"
    );
    // Leave the thread, then come back through its row.
    click(vcx, "new-task");
    draw(vcx);
    click(vcx, &format!("recent-{second}"));
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, _| r.run_screen().is_some())
    });
    assert_eq!(current_run(vcx, &root), second);
    assert!(kinds(&rows_of(vcx, &root)).contains(&"divider"));
}

#[gpui::test]
fn review_compares_this_thread_or_this_run(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let (_first, second) = two_runs(vcx, &root, &store, "first task", "second task");
    let screen = screen_of(vcx, &root);
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| {
            s.changes(&second)
                .is_some_and(|c| matches!(c.diff, Loaded::Ready(_)))
        })
    });
    root.update_in(vcx, |r, window, cx| {
        let screen = r.run_screen().cloned().unwrap();
        screen.update(cx, |s, cx| s.open_surface(Surface::Review, window, cx));
    });
    wait_drawn(vcx, |vcx| painted(vcx, "review-scope"));
    assert_eq!(screen.read_with(vcx, |s, _| s.scope()), ReviewScope::Thread);

    let files = |vcx: &mut VisualTestContext, scope| {
        store.read_with(vcx, |s, _| match s.changes_in(&second, scope) {
            Some(c) => match &c.diff {
                Loaded::Ready(d) => Some(d.files.len()),
                _ => None,
            },
            None => None,
        })
    };
    let thread_files = files(vcx, ReviewScope::Thread).unwrap();
    click(vcx, "scope-run");
    assert_eq!(screen.read_with(vcx, |s, _| s.scope()), ReviewScope::Run);
    wait_drawn(vcx, |vcx| files(vcx, ReviewScope::Run).is_some());
    let run_files = files(vcx, ReviewScope::Run).unwrap();
    assert!(
        run_files < thread_files,
        "the run alone changed {run_files} of the thread's {thread_files} files"
    );
    // A patch for the run's scope is its own read.
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| s.review_row_count() > 0)
    });
    click(vcx, "scope-thread");
    assert_eq!(screen.read_with(vcx, |s, _| s.scope()), ReviewScope::Thread);
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| s.review_row_count() > 0)
    });
}

#[gpui::test]
fn an_old_daemon_has_no_scope_switch(cx: &mut TestAppContext) {
    let store = store_with(cx, without_threads());
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    root.update_in(vcx, |r, window, cx| {
        r.open_run(run_id("builder1"), window, cx)
    });
    root.update_in(vcx, |r, window, cx| {
        let screen = r.run_screen().cloned().unwrap();
        screen.update(cx, |s, cx| s.open_surface(Surface::Review, window, cx));
    });
    wait_drawn(vcx, |vcx| painted(vcx, "review"));
    wait_drawn(vcx, |vcx| painted(vcx, "review-branch"));
    assert!(!painted(vcx, "review-scope"));
}

#[gpui::test]
fn closing_a_thread_asks_when_work_is_unmerged_keeps_it_and_keeps_its_row(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let first = send_from_home(vcx, &root, "first task");
    wait_drawn(vcx, |vcx| {
        run_status(vcx, &store, &first).is_some_and(RunStatus::is_terminal)
    });
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| {
            s.thread_of(&first).is_some()
                && s.changes(&first)
                    .is_some_and(|c| matches!(&c.workspace, Loaded::Ready(Some(_))))
        })
    });
    open_scratch_folder(vcx);
    crate::tests::show_session(vcx);
    wait_drawn(vcx, |vcx| painted(vcx, "thread-close"));
    assert!(painted(vcx, &format!("recent-{first}")));

    // The fixture's worktree holds commits: closing says what stays, and asks.
    click(vcx, "thread-close");
    assert!(painted(vcx, "thread-confirm"));
    click(vcx, "thread-close-cancel");
    assert!(!painted(vcx, "thread-confirm"));
    click(vcx, "thread-close");
    click(vcx, "thread-close-confirm");

    wait_drawn(vcx, |vcx| painted(vcx, "thread-closed"));
    let result = store.read_with(vcx, |s, _| {
        s.close_result(&s.thread_of(&first).unwrap())
            .cloned()
            .unwrap()
    });
    assert_eq!(result.workspace, Some(reno_proto::CleanOutcome::Kept));
    // A closed thread is history: its worktree is released, its row stays.
    assert!(painted(vcx, &format!("recent-{first}")));
}

#[gpui::test]
fn an_active_thread_cannot_be_closed_from_the_session_tab(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let _run = send_from_home(vcx, &root, "/long job");
    wait_drawn(vcx, |vcx| crate::tests::painted(vcx, "panel-tab-session"));
    crate::tests::show_session(vcx);
    wait_drawn(vcx, |vcx| painted(vcx, "thread-busy"));
    assert!(!painted(vcx, "thread-close"));
}

#[gpui::test]
fn a_panel_saved_under_a_run_id_is_taken_over_by_its_one_run_thread(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.state().thread(&thread("keeper1")).is_some())
    });
    let dir = tempfile::tempdir().unwrap();
    let file = crate::panel::persist::path_in(dir.path());
    cx.update(|cx| Saved::install(file, cx));
    // What the version before threads saved: keeper1's tabs under its run id.
    let mut old = crate::panel::PanelState::default();
    old.open(Surface::File("old.rs".into()));
    cx.update(|cx| Saved::put(PanelKey("run_keeper1".to_owned()), old.snapshot(), cx));
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    root.update_in(vcx, |r, window, cx| {
        r.open_run(run_id("keeper1"), window, cx)
    });
    draw(vcx);
    let screen = screen_of(vcx, &root);
    assert_eq!(
        screen.read_with(vcx, |s, _| s.panel_key().clone()),
        PanelKey("thr_keeper1".to_owned())
    );
    assert!(
        screen.read_with(vcx, |s, _| s
            .panel_state()
            .tabs()
            .contains(&Surface::File("old.rs".into()))),
        "the old key's tabs carry over"
    );
}
