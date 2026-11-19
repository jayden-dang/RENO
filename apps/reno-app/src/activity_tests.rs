//! ROAD-35: following a run. The plan tray and the background tray over the composer, the
//! subagent chips, and the Session tab's Plan / Running / Agents sections, against the fake
//! daemon's mocked activity contract ("real" is the fake with the real daemon's capabilities).
//! These assert structure, words and state, never pixels.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_client::activity::{StopError, TaskState};

use crate::app::RootView;
use crate::panel::surface::Surface;
use crate::store::Store;
use crate::tests::{click, draw, painted, run_id, show_session, wait_drawn, wait_until, window};
use crate::ui::theme::Appearance;
use crate::vm::activity as words;
use crate::vm::capabilities::Capabilities;

/// A connected fake store with the activity demo runs (plan1 busy, plan0 empty).
fn following(cx: &mut TestAppContext) -> Entity<Store> {
    crate::tests::one_window_test_at_a_time();
    cx.update(|cx| {
        crate::actions::bind_keys(cx);
        crate::ui::composer::bind_keys(cx);
        crate::screens::run::bind_keys(cx);
    });
    cx.update(gpui_tokio::init);
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(
            reno_client::fake::FakeConfig {
                step: std::time::Duration::ZERO,
                activity_runs: true,
                ..reno_client::fake::FakeConfig::default()
            },
            cx,
        );
        store
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| {
            s.state().run(&run_id("plan1")).is_some() && !s.state().executors().is_empty()
        })
    });
    store
}

fn open_run<'a>(
    cx: &'a mut TestAppContext,
    store: &Entity<Store>,
    run: &str,
) -> (Entity<RootView>, &'a mut VisualTestContext) {
    let (root, vcx) = window(cx, store, Appearance::Dark);
    draw(vcx);
    click(vcx, &format!("recent-run_{run}"));
    wait_drawn(vcx, |vcx| painted(vcx, "composer"));
    (root, vcx)
}

fn screen(
    root: &Entity<RootView>,
    vcx: &VisualTestContext,
) -> Entity<crate::screens::run::RunScreen> {
    root.read_with(vcx, |r, _| r.run_screen().unwrap().clone())
}

fn activity_of(
    store: &Entity<Store>,
    cx: &impl gpui::AppContext,
    run: &str,
) -> Option<reno_client::activity::RunActivity> {
    store.read_with(cx, |s, _| s.run_activity(&run_id(run)))
}

fn advance(store: &Entity<Store>, cx: &impl gpui::AppContext, ms: u64) {
    store.read_with(cx, |s, _| s.fake_daemon().unwrap().advance_activity(ms));
}

#[gpui::test]
fn the_busy_run_says_the_finding_in_the_tray(cx: &mut TestAppContext) {
    let store = following(cx);
    let (_root, vcx) = open_run(cx, &store, "plan1");
    let a = activity_of(&store, vcx, "plan1").unwrap();
    assert_eq!(
        words::finding(&a).as_deref(),
        Some(
            "Step 3 of 5 \u{b7} Running the test suite \u{2014} 1 subagent researching the ACP v2 spec \u{2014} 1 background dev server on :5173 for 12 min \u{b7} +1 more"
        )
    );
    assert!(painted(vcx, "activity-finding-tray"));
}

#[gpui::test]
fn an_empty_run_shows_no_tray_no_chip_and_no_session_sections(cx: &mut TestAppContext) {
    let store = following(cx);
    let (_root, vcx) = open_run(cx, &store, "plan0");
    show_session(vcx);
    assert_eq!(activity_of(&store, vcx, "plan0"), None);
    for gone in [
        "activity-plan-tray",
        "activity-bg-chip",
        "activity-sections",
        "activity-finding-tray",
    ] {
        assert!(
            !painted(vcx, gone),
            "{gone} shows on a run with no activity"
        );
    }
}

#[gpui::test]
fn the_collapsed_tray_reads_progress_and_step_and_opens_to_the_checklist(cx: &mut TestAppContext) {
    let store = following(cx);
    let (_root, vcx) = open_run(cx, &store, "plan1");
    let a = activity_of(&store, vcx, "plan1").unwrap();
    assert_eq!(
        words::plan_line(&a),
        Some((
            "Plan \u{b7} 2/5 \u{b7}".to_owned(),
            "Running the test suite".to_owned()
        ))
    );
    assert!(painted(vcx, "activity-plan-line"));
    assert!(!painted(vcx, "activity-plan-list"), "collapsed");
    click(vcx, "activity-plan-tray");
    draw(vcx);
    assert!(painted(vcx, "activity-plan-list"), "opens to the checklist");
}

#[gpui::test]
fn the_chip_counts_what_is_running(cx: &mut TestAppContext) {
    let store = following(cx);
    let (_root, vcx) = open_run(cx, &store, "plan1");
    let a = activity_of(&store, vcx, "plan1").unwrap();
    assert_eq!(words::running_label(&a).as_deref(), Some("2 running"));
    assert!(painted(vcx, "activity-bg-chip"));
    assert!(!painted(vcx, "activity-bg-tray"), "collapsed");
    click(vcx, "activity-bg-chip");
    draw(vcx);
    assert!(painted(vcx, "activity-task-dev") && painted(vcx, "activity-task-watch"));
}

#[gpui::test]
fn stop_reads_stopping_then_stopped_and_the_tray_and_session_agree(cx: &mut TestAppContext) {
    let store = following(cx);
    let (root, vcx) = open_run(cx, &store, "plan1");
    let run = screen(&root, vcx);
    show_session(vcx);
    // Stop in the Session tab (the tray is closed).
    click(vcx, "activity-stop-dev");
    draw(vcx);
    let task = |vcx: &VisualTestContext| {
        activity_of(&store, vcx, "plan1")
            .unwrap()
            .tasks
            .into_iter()
            .find(|t| t.id == "dev")
            .unwrap()
    };
    assert_eq!(task(vcx).state, TaskState::Stopping);
    assert!(
        painted(vcx, "activity-stopping-dev"),
        "the press shows Stopping\u{2026}"
    );
    assert!(!painted(vcx, "activity-stop-dev"));
    // The tray, opened afterwards, says the same.
    run.update(vcx, |r, cx| r.open_activity_demo("tasks", cx));
    draw(vcx);
    assert!(painted(vcx, "activity-bg-tray") && painted(vcx, "activity-stopping-dev"));
    // 250 ms later it is stopped, in both places, and the chip counts one less.
    advance(&store, vcx, 250);
    store.update(vcx, |_, cx| cx.notify());
    draw(vcx);
    assert_eq!(task(vcx).state, TaskState::Stopped);
    assert!(!painted(vcx, "activity-stopping-dev") && !painted(vcx, "activity-stop-dev"));
    let a = activity_of(&store, vcx, "plan1").unwrap();
    assert_eq!(words::running_label(&a).as_deref(), Some("1 running"));
}

#[gpui::test]
fn a_stop_that_arrives_too_late_says_why_under_the_task(cx: &mut TestAppContext) {
    let store = following(cx);
    let (root, vcx) = open_run(cx, &store, "plan1");
    let run = screen(&root, vcx);
    run.update(vcx, |r, cx| r.open_activity_demo("tasks", cx));
    draw(vcx);
    click(vcx, "activity-stop-dev");
    draw(vcx);
    assert!(
        !painted(vcx, "activity-stop-error-dev"),
        "the first press was taken"
    );
    // A second Stop for a task that is already going: the daemon refuses, and the tray says so.
    let refused = store.update(vcx, |s, cx| s.stop_task(&run_id("plan1"), "dev", cx));
    assert_eq!(refused, Err(StopError::TaskNotRunning));
    draw(vcx);
    assert!(painted(vcx, "activity-stop-error-dev"));
    let a = activity_of(&store, vcx, "plan1").unwrap();
    let task = a.tasks.iter().find(|t| t.id == "dev").unwrap();
    assert_eq!(
        words::stop_error_text(task, &StopError::TaskNotRunning),
        "Could not stop npm run dev: it already ended."
    );
}

#[gpui::test]
fn a_subagent_lane_opens_its_transcript_as_a_tab(cx: &mut TestAppContext) {
    let store = following(cx);
    let (root, vcx) = open_run(cx, &store, "plan1");
    let run = screen(&root, vcx);
    show_session(vcx);
    click(vcx, "activity-lane-researcher");
    draw(vcx);
    let agent = reno_client::activity::agent_run_suffix("researcher");
    let want = Surface::SideChat(run_id(&agent));
    assert_eq!(
        run.read_with(vcx, |r, _| r.panel_state().active().clone()),
        want
    );
    assert_eq!(want.title(), "researcher");
    assert!(painted(vcx, "activity-agent-body"));
}

#[gpui::test]
fn a_chip_in_the_transcript_says_the_agent_and_opens_its_tab(cx: &mut TestAppContext) {
    let store = following(cx);
    let (root, vcx) = open_run(cx, &store, "plan1");
    let run = screen(&root, vcx);
    // The chip of the agent spawned after the second message is under that message.
    wait_drawn(vcx, |vcx| painted(vcx, "activity-chip-reviewer"));
    let a = activity_of(&store, vcx, "plan1").unwrap();
    let lane = a.agents.iter().find(|l| l.id == "reviewer").unwrap();
    assert_eq!(
        words::chip_label(lane),
        "reviewer \u{b7} finished \u{b7} 1 step"
    );
    click(vcx, "activity-chip-reviewer");
    draw(vcx);
    let want = Surface::SideChat(run_id(&reno_client::activity::agent_run_suffix("reviewer")));
    assert_eq!(
        run.read_with(vcx, |r, _| r.panel_state().active().clone()),
        want
    );
    assert!(painted(vcx, "activity-agent-body"));
}

#[gpui::test]
fn output_opens_the_session_tab_and_leaves_the_composer_focus_alone(cx: &mut TestAppContext) {
    let store = following(cx);
    let (root, vcx) = open_run(cx, &store, "plan1");
    let run = screen(&root, vcx);
    run.update(vcx, |r, cx| r.open_activity_demo("tasks", cx));
    // The panel is hidden, the Review tab (or none) is in front, and the owner is in the composer.
    vcx.update(|window, cx| {
        run.update(cx, |r, cx| r.toggle_panel(window, cx));
    });
    draw(vcx);
    let steer = run.read_with(vcx, |r, _| r.steer_input().clone());
    vcx.update(|window, cx| window.focus(&steer.read(cx).focus_handle_ref().clone(), cx));
    draw(vcx);
    click(vcx, "activity-open-output-dev");
    draw(vcx);
    assert_eq!(
        run.read_with(vcx, |r, _| r.panel_state().active().clone()),
        Surface::Session
    );
    assert!(painted(vcx, "activity-sections"), "the panel shows Session");
    let focused = vcx.update(|window, cx| steer.read(cx).focus_handle_ref().is_focused(window));
    assert!(focused, "the composer kept the keyboard");
}

#[gpui::test]
fn the_session_tab_lists_plan_running_and_agents_but_not_the_finding(cx: &mut TestAppContext) {
    let store = following(cx);
    let (_root, vcx) = open_run(cx, &store, "plan1");
    show_session(vcx);
    for section in [
        "activity-session-plan",
        "activity-session-running",
        "activity-session-agents",
    ] {
        assert!(painted(vcx, section), "{section}");
    }
    let a = activity_of(&store, vcx, "plan1").unwrap();
    let shown = words::session_sections(&a);
    assert!(shown.plan && shown.running && shown.agents);
    assert!(painted(vcx, "activity-finding-tray"), "the tray says it");
    assert!(
        !painted(vcx, "activity-finding-session"),
        "the Session tab does not repeat the finding"
    );
}

#[gpui::test]
fn with_the_real_capabilities_none_of_it_shows(cx: &mut TestAppContext) {
    let store = following(cx);
    store.update(cx, |s, cx| s.set_capabilities(Capabilities::REAL, cx));
    let (_root, vcx) = open_run(cx, &store, "plan1");
    show_session(vcx);
    assert_eq!(activity_of(&store, vcx, "plan1"), None);
    for gone in [
        "activity-plan-tray",
        "activity-bg-chip",
        "activity-sections",
        "activity-chip-researcher",
        "activity-finding-tray",
    ] {
        assert!(!painted(vcx, gone), "{gone} would pretend");
    }
    assert!(painted(vcx, "composer"));
    let stopped = store.update(vcx, |s, cx| s.stop_task(&run_id("plan1"), "dev", cx));
    assert!(stopped.is_err(), "nothing to stop without the contract");
}
