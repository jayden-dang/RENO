//! The notifier and the app's lifecycle through the fake daemon, a `Recorder` desktop and GPUI's
//! test windows. Nothing here reaches `reno_notify`: the real desktop is only built by
//! `lifecycle::install`, which tests never call.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::rc::Rc;
use std::time::Duration;

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_client::fake::FakeConfig;
use reno_client::{ConnectionStatus, Feed, FeedState};
use reno_notify::Delivery;
use reno_proto::{ApprovalDecision, RunId, RunStatus};

use crate::actions::CloseWindow;
use crate::app::RootView;
use crate::banners::Banners;
use crate::desktop::Recorder;
use crate::lifecycle;
use crate::nav::Nav;
use crate::prefs::{Pref, PrefStore};
use crate::screens::settings::delivery_line;
use crate::store::Store;
use crate::tests::{click, draw, painted, wait_drawn, wait_until, window};
use crate::ui::theme::Appearance;
use crate::ui::toast;

/// A store on an empty fake daemon (no runs, no approvals), connected and loaded.
fn empty_store(cx: &mut TestAppContext) -> Entity<Store> {
    crate::tests::one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(
            FakeConfig {
                empty: true,
                step: Duration::ZERO,
                ..FakeConfig::default()
            },
            cx,
        );
        store
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| {
            s.connection() == ConnectionStatus::Connected
                && s.state().feed(Feed::Runs) == FeedState::Live
                && s.state().feed(Feed::Approvals) == FeedState::Live
        })
    });
    store
}

/// The app-level state over a recorder, as `app::run` builds it over the real desktop.
fn shell(
    cx: &mut TestAppContext,
    store: &Entity<Store>,
    prefs: PrefStore,
) -> (Recorder, Entity<Banners>) {
    let recorder = Recorder::default();
    let desktop = recorder.clone();
    let store = store.clone();
    let banners = cx.update(move |cx| {
        let toasts = toast::install(cx);
        lifecycle::install_with(
            cx,
            store,
            toasts,
            Some(Appearance::Dark),
            Rc::new(desktop),
            prefs,
        );
        lifecycle::banners(cx).expect("the shell installs the notifier")
    });
    cx.run_until_parked();
    (recorder, banners)
}

fn hold_a_push(cx: &mut TestAppContext, store: &Entity<Store>) {
    store.update(cx, |s, cx| {
        s.run_task("push the branch".into(), "claude-code", cx)
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.state().approvals().len() == 1)
    });
}

fn held_run(cx: &TestAppContext, store: &Entity<Store>) -> RunId {
    store.read_with(cx, |s, _| s.state().approvals()[0].run_id.clone())
}

#[gpui::test]
fn an_approval_posts_one_banner_and_the_badge_and_deciding_it_withdraws_both(
    cx: &mut TestAppContext,
) {
    let store = empty_store(cx);
    let (recorder, _) = shell(cx, &store, PrefStore::in_memory());
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    vcx.deactivate_window();

    hold_a_push(vcx, &store);
    vcx.run_until_parked();
    let approval = store.read_with(vcx, |s, _| s.state().approvals()[0].clone());
    let posts = recorder.posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(posts[0].id, format!("approval:{}", approval.id));
    assert_eq!(posts[0].title, "push the branch");
    assert_eq!(posts[0].body, "Wants to git push -u origin HEAD");
    assert_eq!(posts[0].target, Some(format!("run:{}", approval.run_id)));
    assert_eq!(recorder.badge(), Some(Some("1".to_owned())));

    store.update(vcx, |s, cx| {
        s.decide(approval.id.clone(), ApprovalDecision::ApproveOnce, cx)
    });
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| s.state().approvals().is_empty())
    });
    assert!(
        recorder
            .withdrawn()
            .contains(&format!("approval:{}", approval.id)),
        "{:?}",
        recorder.calls()
    );
    assert_eq!(recorder.badge(), Some(None), "the badge clears at zero");
    assert_eq!(recorder.posts().len(), 1, "deciding posts nothing");
}

#[gpui::test]
fn the_badge_is_the_sidebar_count(cx: &mut TestAppContext) {
    let store = empty_store(cx);
    let (recorder, _) = shell(cx, &store, PrefStore::in_memory());
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    hold_a_push(vcx, &store);
    vcx.run_until_parked();
    let need = store.read_with(vcx, |s, _| crate::summary::attention(s.state()).needs_you());
    assert_eq!(recorder.badge(), Some(Some(need.to_string())));
}

#[gpui::test]
fn background_only_holds_a_banner_while_the_window_is_active_and_never_replays_it(
    cx: &mut TestAppContext,
) {
    let store = empty_store(cx);
    let (recorder, _) = shell(cx, &store, PrefStore::in_memory());
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    vcx.update(|window, _| window.activate_window());
    vcx.run_until_parked();

    hold_a_push(vcx, &store);
    vcx.run_until_parked();
    assert!(recorder.posts().is_empty(), "held while Reno is active");
    assert_eq!(
        recorder.badge(),
        Some(Some("1".to_owned())),
        "the badge still counts it"
    );

    vcx.deactivate_window();
    vcx.executor().advance_clock(Duration::from_secs(30));
    vcx.run_until_parked();
    assert!(
        recorder.posts().is_empty(),
        "a held banner is not shown later"
    );
}

#[gpui::test]
fn background_only_off_rings_with_the_window_active(cx: &mut TestAppContext) {
    let store = empty_store(cx);
    let (recorder, banners) = shell(cx, &store, PrefStore::in_memory());
    banners.update(cx, |b, cx| b.set_pref(Pref::BackgroundOnly, false, cx));
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    vcx.update(|window, _| window.activate_window());
    hold_a_push(vcx, &store);
    vcx.run_until_parked();
    assert_eq!(recorder.posts().len(), 1);
}

#[gpui::test]
fn a_finished_run_rings_once_it_has_stood_two_seconds(cx: &mut TestAppContext) {
    let store = empty_store(cx);
    let (recorder, _) = shell(cx, &store, PrefStore::in_memory());
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    vcx.deactivate_window();
    store.update(vcx, |s, cx| {
        s.run_task("just do it".into(), "claude-code", cx)
    });
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| {
            s.state()
                .runs()
                .iter()
                .any(|r| r.status == RunStatus::Succeeded)
        })
    });
    vcx.run_until_parked();
    assert!(recorder.posts().is_empty(), "not before it has stood");
    vcx.executor().advance_clock(Duration::from_millis(2100));
    vcx.run_until_parked();
    let posts = recorder.posts();
    assert_eq!(posts.len(), 1, "{:?}", recorder.calls());
    assert_eq!(posts[0].body, "Finished");
    assert!(posts[0].id.starts_with("done:"));
}

#[gpui::test]
fn closing_the_window_does_not_quit_and_the_store_keeps_running(cx: &mut TestAppContext) {
    let store = empty_store(cx);
    let (recorder, _) = shell(cx, &store, PrefStore::in_memory());
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    vcx.update(|window, _| window.activate_window());
    vcx.dispatch_action(CloseWindow);
    vcx.run_until_parked();
    assert!(cx.update(|cx| cx.windows().is_empty()), "the window closed");

    // The store goes on, and the banner still comes with no window.
    hold_a_push(cx, &store);
    cx.run_until_parked();
    assert_eq!(recorder.posts().len(), 1);
    assert_eq!(recorder.badge(), Some(Some("1".to_owned())));

    // A Dock click builds a window around the same store.
    cx.update(lifecycle::reopen);
    assert_eq!(cx.update(|cx| cx.windows().len()), 1);
    cx.update(lifecycle::reopen);
    assert_eq!(
        cx.update(|cx| cx.windows().len()),
        1,
        "a window open is left alone"
    );
}

#[gpui::test]
fn a_banner_click_opens_the_window_if_needed_and_then_the_run(cx: &mut TestAppContext) {
    let store = empty_store(cx);
    let (recorder, _) = shell(cx, &store, PrefStore::in_memory());
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    vcx.update(|window, _| window.activate_window());
    vcx.dispatch_action(CloseWindow);
    vcx.run_until_parked();
    assert!(cx.update(|cx| cx.windows().is_empty()));
    hold_a_push(cx, &store);
    let run = held_run(cx, &store);
    cx.run_until_parked();
    let target = recorder.posts()[0].target.clone().unwrap();

    cx.update(|cx| cx.global::<lifecycle::Shell>().click(&target));
    cx.run_until_parked();
    let window = cx.update(|cx| {
        cx.windows()
            .into_iter()
            .find_map(|w| w.downcast::<RootView>())
    });
    let window = window.expect("the click opened the window");
    let vcx = VisualTestContext::from_window(window.into(), cx).into_mut();
    let root = window.root(vcx).unwrap();
    let shown = root.read_with(vcx, |r, cx| {
        r.run_screen().map(|s| s.read(cx).run_id().clone())
    });
    assert_eq!(
        shown,
        Some(run),
        "the run the banner announced is on screen"
    );
    assert!(
        recorder
            .withdrawn()
            .iter()
            .any(|id| id.starts_with("approval:")),
        "opening the run takes its banners away: {:?}",
        recorder.calls()
    );

    // The daemon banner opens Home, a burst the Runs screen.
    cx.update(|cx| cx.global::<lifecycle::Shell>().click("burst:finished"));
    cx.run_until_parked();
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Runs);
    cx.update(|cx| cx.global::<lifecycle::Shell>().click("daemon"));
    cx.run_until_parked();
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Home);
}

#[gpui::test]
fn opening_a_run_in_the_app_withdraws_its_banners(cx: &mut TestAppContext) {
    let store = empty_store(cx);
    let (recorder, _) = shell(cx, &store, PrefStore::in_memory());
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    vcx.deactivate_window();
    hold_a_push(vcx, &store);
    vcx.run_until_parked();
    let approval = store.read_with(vcx, |s, _| s.state().approvals()[0].clone());
    // Starting the run opened it; go back Home, so opening it is news again.
    root.update_in(vcx, |r, window, cx| r.navigate(Nav::Home, window, cx));
    vcx.run_until_parked();
    recorder.clear();

    root.update_in(vcx, |r, window, cx| {
        r.open_run(approval.run_id.clone(), window, cx)
    });
    vcx.run_until_parked();
    assert!(
        recorder
            .withdrawn()
            .contains(&format!("approval:{}", approval.id)),
        "{:?}",
        recorder.calls()
    );
}

#[gpui::test]
fn the_notifications_tab_toggles_persist_beside_the_socket(cx: &mut TestAppContext) {
    let store = empty_store(cx);
    let tmp = tempfile::tempdir().unwrap();
    let path = crate::prefs::settings_path(&tmp.path().join("reno.sock")).unwrap();
    let (recorder, banners) = shell(cx, &store, PrefStore::at(path.clone()));
    assert_eq!(banners.read_with(cx, |b, _| b.delivery()), Delivery::Native);
    recorder.set_delivery(Delivery::Script);
    assert_eq!(banners.read_with(cx, |b, _| b.delivery()), Delivery::Script);
    assert!(delivery_line(Delivery::Script).contains("cargo xtask bundle"));
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    root.update_in(vcx, |r, window, cx| r.navigate(Nav::Settings, window, cx));
    draw(vcx);
    click(vcx, "tab-Notifications");
    wait_drawn(vcx, |vcx| painted(vcx, "notify-finished"));
    for key in ["notifications", "backgroundOnly", "finished", "dockBadge"] {
        assert!(painted(vcx, &format!("notify-{key}")), "{key}");
    }
    assert!(painted(vcx, "notification-delivery"));

    click(vcx, "notify-finished");
    click(vcx, "notify-dockBadge");
    assert!(!banners.read_with(vcx, |b, _| b.prefs().finished));
    assert!(!banners.read_with(vcx, |b, _| b.prefs().dock_badge));
    assert!(banners.read_with(vcx, |b, _| b.prefs().notifications));
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(written["finished"], false);
    assert_eq!(written["dockBadge"], false);
    assert_eq!(written["notifications"], true);

    // Back on: written again.
    click(vcx, "notify-finished");
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(written["finished"], true);
}

#[gpui::test]
fn turning_the_badge_off_clears_it_and_banners_off_posts_nothing(cx: &mut TestAppContext) {
    let store = empty_store(cx);
    let (recorder, banners) = shell(cx, &store, PrefStore::in_memory());
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    vcx.deactivate_window();
    banners.update(vcx, |b, cx| b.set_pref(Pref::Notifications, false, cx));
    hold_a_push(vcx, &store);
    vcx.run_until_parked();
    assert!(recorder.posts().is_empty());
    assert_eq!(recorder.badge(), Some(Some("1".to_owned())));
    banners.update(vcx, |b, cx| b.set_pref(Pref::DockBadge, false, cx));
    assert_eq!(recorder.badge(), Some(None));
}
