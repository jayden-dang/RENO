//! `#[gpui::test]`s over the real store, the real views and a fake client.
//!
//! The fake daemon runs on a real tokio runtime (`gpui_tokio`), so these tests wait on real
//! time, in 10 ms steps, until the state they expect arrives or a deadline passes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AppContext as _, Entity, EntityInputHandler as _, Modifiers, TestAppContext, VisualTestContext,
    px, size,
};
use reno_client::{ConnectionStatus, Update};
use reno_proto::{ApprovalDecision, RunId, RunStatus, StopReason};

use crate::app::RootView;
use crate::nav::Nav;
use crate::screens::home::Headline;
use crate::store::Store;
use crate::summary::{self, Attention, Finding};
use crate::transcript::{Outcome, Row};
use crate::ui::theme::Appearance;
use crate::ui::toast;
use crate::vm::routing::Class;

pub(crate) fn wait_until(
    cx: &mut TestAppContext,
    mut done: impl FnMut(&mut TestAppContext) -> bool,
) {
    cx.executor().allow_parking();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for the store");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The window tests run one at a time. Each starts its own GPUI test app and a two-worker
/// tokio runtime for the fake daemon; dozens of them at once, on a machine other work also
/// loads, starve the fake daemon's tasks for seconds (seen as 8 s+ stalls) and the real-time
/// waits lose. Taking this lock in the fixture serialises them; the guard lives in the test's
/// own thread and is released when the test ends, passed or failed.
pub(crate) fn one_window_test_at_a_time() {
    use std::cell::RefCell;
    use std::sync::{Mutex, MutexGuard, PoisonError};
    static SERIAL: Mutex<()> = Mutex::new(());
    thread_local! {
        static HELD: RefCell<Option<MutexGuard<'static, ()>>> = const { RefCell::new(None) };
    }
    HELD.with(|held| {
        let mut held = held.borrow_mut();
        if held.is_none() {
            *held = Some(SERIAL.lock().unwrap_or_else(PoisonError::into_inner));
        }
    });
}

pub(crate) fn fake_store(cx: &mut TestAppContext) -> Entity<Store> {
    one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(instant_script(), cx);
        store
    })
}

/// The fake daemon with no pause between its script steps: the run goes as fast as the tasks are
/// scheduled, which does not depend on timers or on how busy the machine is.
pub(crate) fn instant_script() -> reno_client::fake::FakeConfig {
    reno_client::fake::FakeConfig {
        step: Duration::ZERO,
        ..reno_client::fake::FakeConfig::default()
    }
}

pub(crate) fn approvals(cx: &TestAppContext, store: &Entity<Store>) -> usize {
    store.read_with(cx, |s, _| s.state().approvals().len())
}

pub(crate) fn window<'a>(
    cx: &'a mut TestAppContext,
    store: &Entity<Store>,
    appearance: Appearance,
) -> (Entity<RootView>, &'a mut VisualTestContext) {
    let toasts = cx.update(toast::install);
    let store = store.clone();
    let (root, vcx) = cx.add_window_view(move |window, cx| {
        RootView::new(store, toasts, Some(appearance), window, cx)
    });
    // Tall enough that the whole sidebar is inside its scroll area and clickable.
    vcx.simulate_resize(size(px(1280.0), px(960.0)));
    (root, vcx)
}

pub(crate) fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
}

#[gpui::test]
fn home_renders_with_the_demo_state(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);

    for appearance in [Appearance::Dark, Appearance::Light] {
        let (root, vcx) = window(cx, &store, appearance);
        draw(vcx);
        let (home, nav, hidden) =
            root.read_with(vcx, |r, _| (r.home().clone(), r.nav(), r.sidebar_hidden()));
        assert_eq!(nav, Nav::Home);
        assert!(!hidden, "the sidebar shows on Home");
        let headline = home.read_with(vcx, |h, cx| h.headline(cx));
        let Headline::Finding(finding) = headline else {
            panic!("expected a finding, got {headline:?}");
        };
        assert!(matches!(finding, Finding::Held { .. }));
        assert_eq!(
            finding.sentence(),
            "2 actions are held for you — keeper's push has waited 6h."
        );
    }

    let counts = store.read_with(cx, |s, _| summary::attention(s.state()));
    assert_eq!(
        counts,
        Attention {
            need: 2,
            questions: 0,
            running: 1,
            bots: 5
        }
    );
    let connection = store.read_with(cx, |s, _| s.connection());
    assert_eq!(connection, ConnectionStatus::Connected);
}

#[gpui::test]
fn approving_through_the_view_model_removes_the_approval(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);

    let dock = root.read_with(vcx, |r, cx| r.home().read(cx).dock().clone());
    let first = store.read_with(vcx, |s, _| s.state().approvals()[0].clone());
    dock.update(vcx, |dock, cx| {
        dock.decide(&first, ApprovalDecision::ApproveOnce, cx);
    });
    assert!(
        store.read_with(vcx, |s, _| s.is_deciding(&first.id)),
        "the card is busy while the decision is in flight"
    );

    // The card leaves only when the fake reports the new list.
    wait_until(vcx, |cx| approvals(cx, &store) == 1);
    let left = store.read_with(vcx, |s, _| s.state().approvals()[0].clone());
    assert_ne!(left.id, first.id);
    assert!(!store.read_with(vcx, |s, _| s.is_deciding(&first.id)));
    draw(vcx);

    let second = left;
    dock.update(vcx, |dock, cx| {
        dock.decide(&second, ApprovalDecision::Deny, cx);
    });
    wait_until(vcx, |cx| approvals(cx, &store) == 0);
    draw(vcx);
    let headline = root.read_with(vcx, |r, cx| r.home().clone().read(cx).headline(cx));
    let Headline::Finding(finding) = headline else {
        panic!("expected a finding");
    };
    assert!(matches!(finding, Finding::Calm { .. }), "{finding:?}");
}

#[gpui::test]
fn a_home_approval_card_hands_a_wider_answer_to_the_inbox(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let first = store.read_with(vcx, |s, _| s.state().approvals()[0].clone());
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Home);
    click(vcx, &format!("more-{}", first.id));
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Inbox);
    // Nothing was decided on the way: the Inbox card holds the scope choice.
    assert_eq!(approvals(vcx, &store), 2);
}

#[gpui::test]
fn a_disconnected_daemon_shows_the_not_running_state(cx: &mut TestAppContext) {
    let store = cx.new(|_| Store::connecting());
    store.update(cx, |s, cx| {
        s.go_offline("Nothing is listening on /tmp/reno.sock.", cx);
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let home = root.read_with(vcx, |r, _| r.home().clone());
    let headline = home.read_with(vcx, |h, cx| h.headline(cx));
    assert_eq!(
        headline,
        Headline::Offline {
            reason: "Nothing is listening on /tmp/reno.sock.".to_owned()
        }
    );
    // Sending is refused and the draft is kept.
    let composer = home.read_with(vcx, |h, _| h.composer().clone());
    composer.update_in(vcx, |c, window, cx| {
        c.fill("do a thing", window, cx);
        c.submit(window, cx);
    });
    assert_eq!(
        composer.read_with(vcx, |c, _| c.text().to_owned()),
        "do a thing"
    );
    draw(vcx);
}

#[gpui::test]
fn no_daemon_at_the_socket_says_how_to_start_one(cx: &mut TestAppContext) {
    one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.connect_socket("/nonexistent-reno-dir/reno.sock".into(), cx);
        store
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.offline_reason().is_some())
    });
    store.read_with(cx, |s, _| {
        assert_eq!(s.connection(), ConnectionStatus::Disconnected);
        assert_eq!(
            s.offline_reason().as_deref(),
            Some("Daemon not running — run `reno daemon start`")
        );
    });
}

#[gpui::test]
fn home_offers_the_same_retry_as_every_other_screen_when_offline(cx: &mut TestAppContext) {
    one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.connect_socket("/nonexistent-reno-dir/reno.sock".into(), cx);
        store
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.offline_reason().is_some())
    });
    let (root, vcx) = window(cx, &store, Appearance::Light);
    draw(vcx);
    let headline = root.read_with(vcx, |r, cx| r.home().read(cx).headline(cx));
    assert!(matches!(headline, Headline::Offline { .. }));
    assert!(painted(vcx, "retry-now"), "Home offers Retry now");
}

#[gpui::test]
fn a_still_connecting_store_says_so(cx: &mut TestAppContext) {
    let store = cx.new(|_| Store::connecting());
    let (root, vcx) = window(cx, &store, Appearance::Light);
    draw(vcx);
    let headline = root.read_with(vcx, |r, cx| r.home().read(cx).headline(cx));
    assert_eq!(headline, Headline::Connecting);
}

#[gpui::test]
fn sending_a_task_creates_a_run_through_the_client(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let before = store.read_with(cx, |s, _| s.state().runs().len());
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    composer.update_in(vcx, |c, window, cx| {
        c.fill("write a haiku", window, cx);
        c.submit(window, cx);
    });
    assert_eq!(composer.read_with(vcx, |c, _| c.text().to_owned()), "");
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| s.state().runs().len()) == before + 1
    });
}

#[gpui::test]
fn the_store_notifies_only_for_what_changed(cx: &mut TestAppContext) {
    let store = cx.new(|_| Store::connecting());
    let notified = Rc::new(Cell::new(0));
    let seen = notified.clone();
    let _sub = cx.update(|cx| cx.observe(&store, move |_, _| seen.set(seen.get() + 1)));

    store.update(cx, |s, cx| {
        s.apply(Update::Connection(ConnectionStatus::Connected), cx);
    });
    assert_eq!(notified.get(), 1);
    // The same update again is a no-op: no notify.
    store.update(cx, |s, cx| {
        s.apply(Update::Connection(ConnectionStatus::Connected), cx);
    });
    assert_eq!(notified.get(), 1);
    // A transcript event touches no list Home shows: no notify.
    store.update(cx, |s, cx| {
        let changed = s.apply(
            Update::RunEvent {
                run: RunId::from_suffix("r1").unwrap(),
                seq: reno_proto::Seq(0),
                event: reno_proto::ExecEvent::Text {
                    text: "hello".into(),
                },
            },
            cx,
        );
        assert!(!changed.is_empty());
    });
    assert_eq!(notified.get(), 1);
}

#[gpui::test]
fn palette_navigation_and_actions(cx: &mut TestAppContext) {
    let store = cx.new(|_| Store::connecting());
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);

    root.update_in(vcx, |r, window, cx| r.open_palette(window, cx));
    draw(vcx);
    assert!(root.read_with(vcx, |r, _| r.palette_open()));
    root.update_in(vcx, |r, window, cx| r.close_palette(window, cx));
    assert!(!root.read_with(vcx, |r, _| r.palette_open()));

    root.update_in(vcx, |r, _, cx| r.toggle_sidebar(cx));
    assert!(root.read_with(vcx, |r, _| r.sidebar_hidden()));
    draw(vcx);
    root.update_in(vcx, |r, window, cx| r.toggle_theme(window, cx));
    draw(vcx);
}

// --- Simulated mouse clicks -----------------------------------------------------------------
//
// These tests click at the centre of an element's painted bounds with GPUI's input simulation, so
// they go through hit testing, the active-element rules and `on_click`, not through a method the
// view happens to expose. Elements are found by `debug_selector`.

pub(crate) fn click(vcx: &mut VisualTestContext, selector: &str) {
    // `debug_bounds` wants a `'static` key; the strings here are few and the test is short-lived.
    let key: &'static str = Box::leak(selector.to_owned().into_boxed_str());
    let bounds = vcx
        .debug_bounds(key)
        .unwrap_or_else(|| panic!("`{selector}` was not painted"));
    vcx.simulate_click(bounds.center(), Modifiers::none());
    vcx.run_until_parked();
}

pub(crate) fn painted(vcx: &mut VisualTestContext, selector: &str) -> bool {
    let key: &'static str = Box::leak(selector.to_owned().into_boxed_str());
    vcx.debug_bounds(key).is_some()
}

pub(crate) fn run_id(name: &str) -> RunId {
    RunId::from_suffix(name).unwrap()
}

/// Waits (in real time, the fake daemon is on tokio) for `done` and redraws each time.
pub(crate) fn wait_drawn(
    vcx: &mut VisualTestContext,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    vcx.executor().allow_parking();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        draw(vcx);
        if done(vcx) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the screen"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn rows_of(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> Vec<Row> {
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned());
    screen.map_or_else(Vec::new, |s| s.read_with(vcx, |s, _| s.rows().to_vec()))
}

fn kinds(rows: &[Row]) -> Vec<&'static str> {
    rows.iter()
        .map(|r| match r {
            Row::User { steer: false, .. } => "prompt",
            Row::User { steer: true, .. } => "steer",
            Row::Prose { .. } => "prose",
            Row::Reasoning { .. } => "reasoning",
            Row::Tools { .. } => "tools",
            Row::Question { .. } => "question",
            Row::Approval { .. } => "approval",
            Row::Asked { .. } => "asked",
            Row::Routing { .. } => "routing",
            Row::Done { .. } => "done",
        })
        .collect()
}

fn run_status(vcx: &VisualTestContext, store: &Entity<Store>, run: &RunId) -> Option<RunStatus> {
    store.read_with(vcx, |s, _| s.state().run(run).map(|r| r.status))
}

/// Sends `prompt` from Home and returns the opened run's id once the Run screen shows.
fn send_from_home(vcx: &mut VisualTestContext, root: &Entity<RootView>, prompt: &str) -> RunId {
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    composer.update_in(vcx, |c, window, cx| c.fill(prompt, window, cx));
    draw(vcx);
    click(vcx, "send");
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, _| r.run_screen().is_some())
    });
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    screen.read_with(vcx, |s, _| s.run_id().clone())
}

#[gpui::test]
fn clicking_approve_on_the_home_card_approves(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "approve-appr_keeperpush");
    assert!(
        store.read_with(vcx, |s, _| s
            .is_deciding(&"appr_keeperpush".parse().unwrap()))
            || approvals(vcx, &store) == 1,
        "the click reached the store"
    );
    wait_until(vcx, |cx| approvals(cx, &store) == 1);
    assert_eq!(
        store.read_with(vcx, |s, _| s.state().approvals()[0].id.to_string()),
        "appr_scoutfetch"
    );
}

#[gpui::test]
fn clicking_deny_on_the_home_card_denies(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "deny-appr_scoutfetch");
    wait_until(vcx, |cx| approvals(cx, &store) == 1);
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| {
            s.state().run(&run_id("scout1")).map(|r| r.status) == Some(RunStatus::Failed)
        })
    });
}

#[gpui::test]
fn clicking_the_class_chips_selects_a_class(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    for (selector, class) in [
        ("class-review", Class::Review),
        ("class-research", Class::Research),
        ("class-triage", Class::Triage),
        ("class-implement", Class::Implement),
    ] {
        click(vcx, selector);
        assert_eq!(
            composer.read_with(vcx, |c, _| c.class()),
            class,
            "{selector}"
        );
        draw(vcx);
    }
}

#[gpui::test]
fn clicking_send_sends_only_with_a_draft(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let before = store.read_with(cx, |s, _| s.state().runs().len());
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);

    // Greyed and inert while the draft is empty.
    click(vcx, "send");
    std::thread::sleep(Duration::from_millis(50));
    vcx.run_until_parked();
    assert_eq!(store.read_with(vcx, |s, _| s.state().runs().len()), before);
    assert!(root.read_with(vcx, |r, _| r.run_screen().is_none()));

    // With a draft the click sends, and the app opens the new run.
    let run = send_from_home(vcx, &root, "write a haiku");
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Runs);
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| s.state().runs().len()) == before + 1
    });
    assert!(store.read_with(vcx, |s, _| s.state().run(&run).is_some()));
}

#[gpui::test]
fn the_prompt_bubble_shows_the_whole_prompt_the_daemon_sent(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let run = send_from_home(vcx, &root, "first line\nsecond line");
    // The title is only the first line; `RunSummary.prompt` carries the rest.
    let summary = store
        .read_with(vcx, |s, _| s.state().run(&run).cloned())
        .unwrap();
    assert_eq!(summary.title, "first line");
    assert_eq!(summary.prompt.as_deref(), Some("first line\nsecond line"));
    wait_drawn(vcx, |vcx| {
        rows_of(vcx, &root)
            .iter()
            .any(|r| matches!(r, Row::User { steer: false, text, .. } if text == "first line\nsecond line"))
    });
}

#[gpui::test]
fn clicking_sidebar_rows_navigates(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);

    click(vcx, "recent-run_keeper1");
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Runs);
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    assert_eq!(
        screen.read_with(vcx, |s, _| s.run_id().clone()),
        run_id("keeper1")
    );
    draw(vcx);

    // A nav row that has a screen goes there; "+" does too.
    click(vcx, "nav-Home");
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Home);
    draw(vcx);
    click(vcx, "recent-run_scout1");
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Runs);
    draw(vcx);
    click(vcx, "new-task");
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Home);
    draw(vcx);

    // Every nav row has a screen; Settings is a row now.
    click(vcx, "nav-Settings");
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Settings);
}

// --- The Run screen -------------------------------------------------------------------------

#[gpui::test]
fn opening_a_run_from_the_sidebar_shows_its_transcript(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| {
        let rows = rows_of(vcx, &root);
        rows.iter().any(|r| matches!(r, Row::Approval { .. }))
    });
    let rows = rows_of(vcx, &root);
    assert_eq!(
        kinds(&rows),
        ["prompt", "routing", "prose", "tools", "approval"]
    );
    let Row::Prose { text, .. } = &rows[2] else {
        panic!("expected prose");
    };
    assert!(text.starts_with("Three crates have minor updates"));
    // The header, the details pane and the card are on screen.
    assert!(painted(vcx, "approve"));
    assert!(painted(vcx, "stop"));
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    let facts = screen.read_with(vcx, |s, _| s.facts().clone());
    assert_eq!(facts.model.as_deref(), Some("opus-5.5"));
}

#[gpui::test]
fn streaming_events_append_rows_in_order(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let run = send_from_home(vcx, &root, "write a haiku");

    // Rows only ever grow at the end while the run streams; the final shape is fixed.
    let mut seen: Vec<Vec<&'static str>> = Vec::new();
    wait_drawn(vcx, |vcx| {
        let k = kinds(&rows_of(vcx, &root));
        if seen.last() != Some(&k) {
            seen.push(k.clone());
        }
        k.last() == Some(&"done")
    });
    assert_eq!(
        seen.last().unwrap(),
        &["prompt", "routing", "prose", "tools", "prose", "done"]
    );
    for pair in seen.windows(2) {
        assert!(
            pair[1].len() >= pair[0].len() && pair[1].starts_with(&pair[0][..pair[0].len() - 1]),
            "rows changed out of order: {pair:?}"
        );
    }
    let rows = rows_of(vcx, &root);
    let Row::Prose { text, .. } = &rows[2] else {
        panic!("expected prose");
    };
    assert_eq!(
        text.trim_end(),
        "I will start by looking at the repository layout."
    );
    // The run list is its own feed and can trail the run's own events.
    wait_drawn(vcx, |vcx| {
        run_status(vcx, &store, &run) == Some(RunStatus::Succeeded)
    });
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    let usage = screen.read_with(vcx, |s, _| s.facts().usage).unwrap();
    assert_eq!((usage.input, usage.output), (1200, 340));
    assert!(
        screen.read_with(vcx, |s, _| s.is_following_tail()),
        "the list follows the tail while streaming"
    );
}

#[gpui::test]
fn approving_inline_with_a_click_resumes_the_run(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "approve"));

    click(vcx, "approve");
    wait_drawn(vcx, |vcx| {
        kinds(&rows_of(vcx, &root)).last() == Some(&"done")
    });
    assert_eq!(
        run_status(vcx, &store, &run_id("keeper1")),
        Some(RunStatus::Succeeded)
    );
    assert_eq!(approvals(vcx, &store), 1, "only scout's approval is left");
    let rows = rows_of(vcx, &root);
    assert!(!rows.iter().any(|r| matches!(r, Row::Approval { .. })));
    assert!(rows.iter().any(|r| matches!(r, Row::Asked { .. })));
    assert!(!painted(vcx, "approve"), "the card is gone");
    // The tool call that was held got its result.
    let tools_outcome = rows.iter().find_map(|r| match r {
        Row::Tools { calls, .. } => calls.last().map(|c| c.outcome.clone()),
        _ => None,
    });
    assert_eq!(
        tools_outcome,
        Some(Outcome::Ok(Some("approved once".to_owned())))
    );
}

#[gpui::test]
fn denying_inline_with_a_click_fails_the_run(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_scout1");
    wait_drawn(vcx, |vcx| painted(vcx, "deny"));
    click(vcx, "deny");
    wait_drawn(vcx, |vcx| {
        kinds(&rows_of(vcx, &root)).last() == Some(&"done")
    });
    assert_eq!(
        run_status(vcx, &store, &run_id("scout1")),
        Some(RunStatus::Failed)
    );
}

#[gpui::test]
fn stop_cancels_the_run(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let run = send_from_home(vcx, &root, "/long job");
    wait_drawn(vcx, |vcx| painted(vcx, "stop"));
    click(vcx, "stop");
    wait_drawn(vcx, |vcx| {
        run_status(vcx, &store, &run) == Some(RunStatus::Cancelled)
    });
    wait_drawn(vcx, |vcx| !painted(vcx, "stop"));
    let rows = rows_of(vcx, &root);
    assert!(matches!(
        rows.last(),
        Some(Row::Done {
            stop: StopReason::Interrupted,
            ..
        })
    ));
}

#[gpui::test]
fn steering_appends_the_message_and_a_reply(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let _run = send_from_home(vcx, &root, "/long job");
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    let steer = screen.read_with(vcx, |s, _| s.steer_input().clone());

    // The fake's `/long` run opens with a few paced steps (a message, a tool call and its
    // result). Steer once the last of them is in: before that, the run is still talking, and
    // its own rows land around the steer and its reply.
    wait_drawn(vcx, |vcx| {
        rows_of(vcx, &root).iter().any(|r| {
            matches!(r, Row::Tools { calls, .. }
                if calls.last().is_some_and(|c| matches!(c.outcome, Outcome::Ok(_))))
        })
    });

    // Greyed while empty.
    assert!(!steer.read_with(vcx, |c, cx| c.can_send(cx)));
    steer.update_in(vcx, |c, window, cx| c.fill("use fewer words", window, cx));
    draw(vcx);
    assert!(steer.read_with(vcx, |c, cx| c.can_send(cx)));
    click(vcx, "send");
    assert_eq!(steer.read_with(vcx, |c, _| c.text().to_owned()), "");

    wait_drawn(vcx, |vcx| {
        let rows = rows_of(vcx, &root);
        rows.iter()
            .any(|r| matches!(r, Row::User { steer: true, .. }))
            && rows
                .iter()
                .any(|r| matches!(r, Row::Prose { text, .. } if text.contains("Understood")))
    });
    let rows = rows_of(vcx, &root);
    let steer_at = rows
        .iter()
        .position(|r| matches!(r, Row::User { steer: true, text, .. } if text == "use fewer words"))
        .unwrap();
    assert!(
        matches!(&rows[steer_at + 1], Row::Prose { text, .. } if text.starts_with("Understood")),
        "the reply follows the steer: {:?}",
        kinds(&rows)
    );
}

#[gpui::test]
fn a_finished_run_cannot_be_steered(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_reviewer1");
    wait_drawn(vcx, |vcx| {
        kinds(&rows_of(vcx, &root)).last() == Some(&"done")
    });
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    let steer = screen.read_with(vcx, |s, _| s.steer_input().clone());
    steer.update_in(vcx, |c, window, cx| c.fill("one more thing", window, cx));
    draw(vcx);
    assert!(
        !steer.read_with(vcx, |c, cx| c.can_send(cx)),
        "follow-up is disabled in Phase 0"
    );
    // The box says why instead of offering a button, and takes no typing.
    assert!(!painted(vcx, "send") && !painted(vcx, "stop"));
    assert!(painted(vcx, "composer-note"));
    steer.update_in(vcx, |c, window, cx| {
        c.replace_text_in_range(None, "typed", window, cx);
        c.submit(window, cx);
    });
    assert_eq!(
        steer.read_with(vcx, |c, _| c.text().to_owned()),
        "one more thing",
        "the draft stays, and nothing typed gets in"
    );
    let rows = rows_of(vcx, &root);
    assert!(
        !rows
            .iter()
            .any(|r| matches!(r, Row::User { steer: true, .. }))
    );
}

#[gpui::test]
fn the_run_screen_draws_in_both_appearances_and_toggles_details(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    for appearance in [Appearance::Dark, Appearance::Light] {
        let (root, vcx) = window(cx, &store, appearance);
        draw(vcx);
        click(vcx, "recent-run_keeper1");
        wait_drawn(vcx, |vcx| painted(vcx, "approve"));
        click(vcx, "details-toggle");
        draw(vcx);
        click(vcx, "details-toggle");
        draw(vcx);
        assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Runs);
    }
}

#[gpui::test]
fn a_real_run_shows_its_repository_in_the_details_pane(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    // A real daemon's run: a plain title and the folder its task was submitted with.
    let mut real = store.read_with(cx, |s, _| {
        s.state().run(&run_id("keeper1")).cloned().unwrap()
    });
    real.title = "Fix the porch light".to_owned();
    real.cwd = Some("/Users/x/Developer/experiments/porch".into());
    store.update(cx, |s, cx| {
        s.apply(Update::RunUpserted(real), cx);
    });
    let (_root, vcx) = window(cx, &store, Appearance::Light);
    draw(vcx);
    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "details-repository"));
}

#[gpui::test]
fn the_details_pane_shows_what_the_agent_reported_when_it_did_not_go_well(cx: &mut TestAppContext) {
    use reno_proto::{ReportStatus, RunReport};
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let mut run = store.read_with(cx, |s, _| {
        s.state().run(&run_id("keeper1")).cloned().unwrap()
    });
    run.status = RunStatus::Succeeded;
    run.report = Some(RunReport {
        status: ReportStatus::Failed,
        summary: "the registry was unreachable".to_owned(),
    });
    store.update(cx, |s, cx| {
        s.apply(Update::RunUpserted(run.clone()), cx);
    });
    let (_root, vcx) = window(cx, &store, Appearance::Light);
    draw(vcx);
    click(vcx, "recent-run_keeper1");
    // The fake daemon's own copy of the run can arrive after the local upsert under load and
    // replace it without the report; put it back until the screen shows it.
    wait_drawn(vcx, |vcx| {
        let reported = store.read_with(vcx, |s, _| {
            s.state()
                .run(&run_id("keeper1"))
                .is_some_and(|r| r.report.is_some())
        });
        if !reported {
            store.update(vcx, |s, cx| s.apply(Update::RunUpserted(run.clone()), cx));
        }
        painted(vcx, "details-report")
    });
}

#[gpui::test]
fn a_daemon_that_will_not_start_is_explained_in_the_banner(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use reno_client::launcher::{DaemonKeeper, FakeLaunchctl, KeeperConfig, Launcher};

    one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let renod = root.path().join("renod");
    std::fs::write(
        &renod,
        "#!/bin/sh\necho 'renod: cannot open the store' >&2\nexit 3\n",
    )
    .unwrap();
    std::fs::set_permissions(&renod, std::fs::Permissions::from_mode(0o755)).unwrap();
    let socket = data.join("reno.sock");
    let launcher = Launcher::new(
        data,
        socket.clone(),
        root.path().join("LaunchAgents"),
        Arc::new(FakeLaunchctl::new()),
    )
    .with_renod(renod);
    let keeper = DaemonKeeper::new(socket.clone(), launcher, KeeperConfig::default());

    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.connect_keeper(socket, &keeper, cx);
        store
    });
    // The mirror's own backoff retries the connection; the second look finds the daemon gone.
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| {
            s.offline_reason()
                .is_some_and(|reason| reason.contains("did not start"))
        })
    });
    store.read_with(cx, |s, _| {
        let reason = s.offline_reason().unwrap();
        assert!(reason.contains("exit status: 3"), "{reason}");
        assert!(reason.contains("cannot open the store"), "{reason}");
    });
}
