//! `#[gpui::test]`s over the real store, the real views and a fake client.
//!
//! The fake daemon runs on a real tokio runtime (`gpui_tokio`), so these tests wait on real
//! time, in 10 ms steps, until the state they expect arrives or a deadline passes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_client::{ConnectionStatus, Update};
use reno_proto::{ApprovalDecision, RunId};

use crate::app::RootView;
use crate::nav::Nav;
use crate::screens::home::Headline;
use crate::store::Store;
use crate::summary::{self, Attention, Finding};
use crate::ui::theme::Appearance;
use crate::ui::toast;

fn wait_until(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
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

fn fake_store(cx: &mut TestAppContext) -> Entity<Store> {
    cx.update(gpui_tokio::init);
    cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake(cx);
        store
    })
}

fn approvals(cx: &TestAppContext, store: &Entity<Store>) -> usize {
    store.read_with(cx, |s, _| s.state().approvals().len())
}

fn window<'a>(
    cx: &'a mut TestAppContext,
    store: &Entity<Store>,
    appearance: Appearance,
) -> (Entity<RootView>, &'a mut VisualTestContext) {
    let toasts = cx.update(toast::install);
    let store = store.clone();
    cx.add_window_view(move |window, cx| RootView::new(store, toasts, Some(appearance), window, cx))
}

fn draw(cx: &mut VisualTestContext) {
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
    // Other destinations do not navigate.
    root.update_in(vcx, |r, window, cx| r.navigate(Nav::Inbox, window, cx));
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Home);
}
