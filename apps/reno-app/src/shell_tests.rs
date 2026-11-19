//! The app shell against the fake daemon: the status bar (one 40 pt band with the title bar), the
//! gap under it, back and forward, the "…" menu, "N live", the agents-updates island, the one
//! panel toggle, and the sidebar's "+" and badges.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext, px};
use reno_client::{Feed, FeedState};

use crate::app::RootView;
use crate::nav::Nav;
use crate::panel::layout::GUTTER;
use crate::tests::{approvals, click, draw, fake_store, painted, wait_drawn, wait_until, window};
use crate::ui::theme::Appearance;

fn current_run(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> String {
    root.read_with(vcx, |r, cx| {
        r.run_screen()
            .map(|s| s.read(cx).run_id().to_string())
            .unwrap_or_default()
    })
}

fn bounds(vcx: &mut VisualTestContext, selector: &'static str) -> gpui::Bounds<gpui::Pixels> {
    vcx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("`{selector}` was not painted"))
}

fn near(a: gpui::Pixels, b: f32) -> bool {
    (a - px(b)).abs() < px(0.6)
}

#[gpui::test]
fn the_bar_is_one_forty_point_band_with_twenty_eight_point_controls(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let bar = bounds(vcx, "status-bar");
    assert!(near(bar.size.height, 40.0));
    assert_eq!(bar.origin.y, px(0.0));
    assert!(near(bar.size.width, 1280.0), "full width, over the sidebar");
    for id in [
        "toggle-sidebar",
        "nav-back",
        "bar-more",
        "bar-live",
        "bar-palette",
    ] {
        let control = bounds(vcx, id);
        assert!(near(control.size.height, 28.0), "{id} is 28 pt");
        assert!(
            (control.center().y - bar.center().y).abs() < px(0.6),
            "{id} is centred"
        );
    }
}

#[gpui::test]
fn canvases_start_one_gutter_below_the_controls_on_home_and_on_a_run(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    for appearance in [Appearance::Dark, Appearance::Light] {
        let (_root, vcx) = window(cx, &store, appearance);
        draw(vcx);
        let controls = bounds(vcx, "toggle-sidebar");
        let canvas = bounds(vcx, "canvas-panel");
        assert!(
            near(canvas.top() - controls.bottom(), GUTTER),
            "Home: {:?} under the controls",
            canvas.top() - controls.bottom()
        );
        // The gap under the canvas and to its right is the same gutter.
        assert!(near(px(960.0) - canvas.bottom(), GUTTER));
        assert!(near(px(1280.0) - canvas.right(), GUTTER));

        click(vcx, "recent-run_keeper1");
        wait_drawn(vcx, |vcx| painted(vcx, "chat-canvas"));
        let chat = bounds(vcx, "chat-canvas");
        assert!(
            near(chat.top() - controls.bottom(), GUTTER),
            "a run: {:?} under the controls",
            chat.top() - controls.bottom()
        );
    }
}

#[gpui::test]
fn the_sidebar_hidden_keeps_the_same_gap_under_the_bar(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "toggle-sidebar");
    draw(vcx);
    assert!(root.read_with(vcx, |r, _| r.sidebar_hidden()));
    assert!(!painted(vcx, "sidebar"));
    let controls = bounds(vcx, "toggle-sidebar");
    let canvas = bounds(vcx, "canvas-panel");
    assert!(near(canvas.top() - controls.bottom(), GUTTER));
    assert!(near(canvas.left(), GUTTER));
}

#[gpui::test]
fn live_runs_open_all_runs_and_the_pills_left_home(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    // The three pills moved from Home into the bar.
    assert!(
        !painted(vcx, "open-palette"),
        "Home has no ⌘K pill any more"
    );
    assert!(painted(vcx, "bar-live") && painted(vcx, "bar-palette") && painted(vcx, "bar-daemon"));
    click(vcx, "bar-live");
    draw(vcx);
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Runs);
    wait_drawn(vcx, |vcx| painted(vcx, "lane-run_keeper1"));
}

#[gpui::test]
fn back_and_forward_return_to_where_the_person_has_been(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    // Nothing to go back to yet: the button does nothing.
    click(vcx, "nav-back");
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Home);

    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "chat-canvas"));
    click(vcx, "bar-live");
    draw(vcx);
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Runs);
    assert!(root.read_with(vcx, |r, _| r.run_screen().is_none()));

    click(vcx, "nav-back");
    wait_drawn(vcx, |vcx| painted(vcx, "chat-canvas"));
    assert_eq!(current_run(vcx, &root), "run_keeper1");
    click(vcx, "nav-back");
    draw(vcx);
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Home);
    click(vcx, "nav-forward");
    wait_drawn(vcx, |vcx| painted(vcx, "chat-canvas"));
    assert_eq!(current_run(vcx, &root), "run_keeper1");
    click(vcx, "nav-forward");
    draw(vcx);
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Runs);
    // At the end: forward does nothing.
    click(vcx, "nav-forward");
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Runs);
}

#[gpui::test]
fn the_more_menu_offers_the_palette_the_theme_triggers_and_the_panel_only_on_a_run(
    cx: &mut TestAppContext,
) {
    cx.update(crate::actions::bind_keys);
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "bar-more");
    wait_drawn(vcx, |vcx| painted(vcx, "more-menu"));
    for id in ["more-palette", "more-theme", "more-triggers"] {
        assert!(painted(vcx, id), "{id}");
    }
    assert!(!painted(vcx, "more-panel"), "no panel on Home");

    let before = root.read_with(vcx, |r, _| r.appearance_override_for_test());
    click(vcx, "more-theme");
    draw(vcx);
    assert!(!painted(vcx, "more-menu"), "choosing closes the menu");
    assert_ne!(
        root.read_with(vcx, |r, _| r.appearance_override_for_test()),
        before
    );

    click(vcx, "bar-more");
    wait_drawn(vcx, |vcx| painted(vcx, "more-triggers"));
    click(vcx, "more-triggers");
    draw(vcx);
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Triggers);

    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "chat-canvas"));
    click(vcx, "bar-more");
    wait_drawn(vcx, |vcx| painted(vcx, "more-panel"));
    click(vcx, "more-palette");
    draw(vcx);
    assert!(root.read_with(vcx, |r, _| r.palette_open()));
}

#[gpui::test]
fn the_panel_has_one_toggle_and_it_is_in_the_bar_on_a_run(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    assert!(!painted(vcx, "panel-toggle"), "nothing to toggle on Home");
    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "panel-canvas"));
    assert!(painted(vcx, "panel-toggle"));
    assert!(
        !painted(vcx, "details-toggle"),
        "no toggle in the run header"
    );
    assert!(!painted(vcx, "panel-hide"), "no hide button in the panel");
    click(vcx, "panel-toggle");
    draw(vcx);
    assert!(!painted(vcx, "panel-canvas"));
    click(vcx, "panel-toggle");
    draw(vcx);
    assert!(painted(vcx, "panel-canvas"));
}

#[gpui::test]
fn the_updates_island_shows_only_agents_with_an_update_and_update_runs_the_job(
    cx: &mut TestAppContext,
) {
    let store = fake_store(cx);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| {
            s.state().feed(Feed::Executors) == FeedState::Live
        })
    });
    let listed = |cx: &gpui::TestAppContext| {
        store.read_with(cx, |s, _| {
            crate::vm::executors::agent_updates(s.state(), s.now_ms(), None)
                .into_iter()
                .map(|u| u.id)
                .collect::<Vec<_>>()
        })
    };
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let offered = listed(vcx);
    assert!(
        offered.iter().any(|id| id == "acp.grok"),
        "the demo has an agent with an update: {offered:?}"
    );
    assert!(
        !offered.iter().any(|id| id == "codex"),
        "codex updates by itself"
    );
    wait_drawn(vcx, |vcx| painted(vcx, "bar-updates"));
    click(vcx, "bar-updates");
    wait_drawn(vcx, |vcx| painted(vcx, "updates-menu"));
    assert!(painted(vcx, "update-all"));
    assert!(painted(vcx, "update-acp.grok"));
    assert!(!painted(vcx, "skip-acp.grok"), "Skip has nothing behind it");
    click(vcx, "update-acp.grok");
    // The job runs on the daemon; the agent leaves the list and the row says it is up to date.
    wait_drawn(vcx, |vcx| !listed(vcx).iter().any(|id| id == "acp.grok"));
    assert!(painted(vcx, "update-row-acp.grok"), "kept as Up to date");
    // Closing the list forgets it.
    click(vcx, "bar-updates");
    draw(vcx);
    assert!(!painted(vcx, "updates-menu"));
}

#[gpui::test]
fn a_projects_plus_opens_a_new_thread_there_and_start_a_task_has_no_project(
    cx: &mut TestAppContext,
) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "new-thread-atlas-api"));
    click(vcx, "new-thread-atlas-api");
    draw(vcx);
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Home);
    assert!(painted(vcx, "new-thread-title"));
    assert!(root.read_with(vcx, |r, _| r.home_folder().is_some()));
    // The chip says so, and back returns to the run.
    click(vcx, "nav-back");
    wait_drawn(vcx, |vcx| painted(vcx, "chat-canvas"));
    click(vcx, "nav-forward");
    draw(vcx);
    assert!(painted(vcx, "new-thread-title"));
    click(vcx, "new-task");
    draw(vcx);
    assert!(!painted(vcx, "new-thread-title"));
    assert!(root.read_with(vcx, |r, _| r.home_folder().is_none()));
}

#[gpui::test]
fn a_thread_waiting_for_you_says_so_in_words_on_its_row(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    // The demo's keeper run waits on a held action; the running one has a spinner, no badge.
    assert!(painted(vcx, "badge-run_keeper1"), "held: Needs approval");
    let running = store.read_with(vcx, |s, _| {
        crate::vm::shell::live_runs(s.state())
            .into_iter()
            .find(|r| r.status == reno_proto::RunStatus::Running)
            .map(|r| r.run_id.to_string())
    });
    if let Some(run) = running {
        assert!(!painted(
            vcx,
            Box::leak(format!("badge-{run}").into_boxed_str())
        ));
    }
}

#[gpui::test]
fn the_sidebar_and_settings_navigation_sit_directly_under_the_bar(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let bar = bounds(vcx, "status-bar");
    assert_eq!(bounds(vcx, "sidebar").top(), bar.bottom());
    click(vcx, "nav-Settings");
    draw(vcx);
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Settings);
    assert_eq!(bounds(vcx, "settings-nav").top(), bar.bottom());
    // No band of its own above "Back to app": the first row is within one gutter of the nav.
    let nav = bounds(vcx, "settings-nav");
    let back = bounds(vcx, "settings-back");
    assert!(back.top() - nav.top() < px(16.0));
    // The bar's back and forward rest while Settings shows ("Back to app" is the way out).
    click(vcx, "nav-back");
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Settings);
}

#[gpui::test]
fn a_live_lane_shows_what_the_run_is_doing_now(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "bar-live");
    // Live runs are followed while they are live, so each lane can say its last step.
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, cx| r.runs().read(cx).lanes(cx))
            .iter()
            .any(|l| l.tail.is_some())
    });
}

#[gpui::test]
fn a_run_asking_a_question_has_a_needs_input_badge_and_a_lane_with_its_question(
    cx: &mut TestAppContext,
) {
    crate::tests::one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    let store = cx.new(|cx| {
        let mut store = crate::store::Store::connecting();
        store.start_fake_with(
            reno_client::fake::FakeConfig {
                step: std::time::Duration::ZERO,
                question_run: true,
                ..reno_client::fake::FakeConfig::default()
            },
            cx,
        );
        store
    });
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    wait_drawn(vcx, |vcx| painted(vcx, "badge-run_asker1"));
    click(vcx, "bar-live");
    wait_drawn(vcx, |vcx| {
        root.read_with(vcx, |r, cx| r.runs().read(cx).lanes(cx))
            .iter()
            .any(|l| {
                l.run.to_string() == "run_asker1"
                    && l.asks == Some(crate::vm::projects::Badge::NeedsInput)
                    && l.tail.as_deref()
                        == Some("Which retry policy should the webhook trigger use?")
            })
    });
}
