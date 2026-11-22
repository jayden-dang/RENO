//! ROAD-33: the transcript draft's switch and its seeded world. The switch cycles with the keys,
//! the status bar names the variant and only while the draft is on, and the seeded thread
//! reaches the run screen as eight folded turns. They assert state and words, never pixels.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_client::fake::FakeConfig;

use crate::app::RootView;
use crate::store::Store;
use crate::tests::{
    draw, one_window_test_at_a_time, painted, run_id, wait_drawn, wait_until, window,
};
use crate::transcript::Row;
use crate::ui::draft33;
use crate::ui::theme::Appearance;
use crate::vm::draft33::{OpenHook, Variant};

fn world(cx: &mut TestAppContext) -> Entity<Store> {
    one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(
            FakeConfig {
                step: Duration::ZERO,
                transcript_threads: true,
                ..FakeConfig::default()
            },
            cx,
        );
        store
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.thread_of(&run_id("acp8")).is_some())
    });
    store
}

fn open(
    cx: &mut TestAppContext,
    on: Option<Variant>,
) -> (Entity<Store>, Entity<RootView>, &mut VisualTestContext) {
    let store = world(cx);
    cx.update(|cx| {
        crate::actions::bind_keys(cx);
        if let Some(variant) = on {
            draft33::install(variant, cx);
        }
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    (store, root, vcx)
}

fn showing(vcx: &mut VisualTestContext) -> Option<Variant> {
    vcx.update(|_, cx| draft33::current(cx))
}

#[gpui::test]
fn the_keys_cycle_the_variants_and_the_status_bar_names_the_one_showing(cx: &mut TestAppContext) {
    let (_store, _root, vcx) = open(cx, Some(Variant::A));
    assert!(painted(vcx, "draft33-label"));
    assert_eq!(showing(vcx), Some(Variant::A));
    vcx.simulate_keystrokes("cmd-alt-right");
    draw(vcx);
    assert_eq!(showing(vcx), Some(Variant::B));
    vcx.simulate_keystrokes("cmd-alt-left cmd-alt-left");
    assert_eq!(showing(vcx), Some(Variant::C));
    // It wraps.
    vcx.simulate_keystrokes("cmd-alt-right");
    assert_eq!(showing(vcx), Some(Variant::A));
    assert!(painted(vcx, "draft33-label"));
}

#[gpui::test]
fn without_the_draft_the_bar_has_no_label_and_the_keys_do_nothing(cx: &mut TestAppContext) {
    let (_store, _root, vcx) = open(cx, None);
    assert!(!painted(vcx, "draft33-label"));
    vcx.simulate_keystrokes("cmd-alt-right");
    draw(vcx);
    assert_eq!(showing(vcx), None);
    assert!(!painted(vcx, "draft33-label"));
}

#[gpui::test]
fn the_handshake_thread_shows_as_eight_folded_turns(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open(cx, Some(Variant::A));
    root.update_in(vcx, |root, window, cx| {
        root.draft33_hook(OpenHook::Thread, window, cx);
    });
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned().expect("a run is open"));
    let folds = |vcx: &mut VisualTestContext| -> Vec<usize> {
        screen.read_with(vcx, |s, _| {
            s.rows()
                .iter()
                .filter_map(|row| match row {
                    Row::Worked { rows, .. } => Some(rows.len()),
                    _ => None,
                })
                .collect()
        })
    };
    wait_drawn(vcx, |vcx| folds(vcx).len() == 8);
    let sizes = folds(vcx);
    // Each turn is one fold of about 250 events: a reasoning note, and a note and a tool group
    // for each of 82 steps.
    assert!(sizes.iter().all(|n| *n >= 160), "{sizes:?}");
}

#[gpui::test]
fn the_render_hook_opens_the_render_check_and_the_stream_hook_starts_the_long_reply(
    cx: &mut TestAppContext,
) {
    let (store, root, vcx) = open(cx, Some(Variant::B));
    root.update_in(vcx, |root, window, cx| {
        root.draft33_hook(OpenHook::Render, window, cx);
    });
    let open_run = |root: &Entity<RootView>, vcx: &mut VisualTestContext| {
        root.read_with(vcx, |r, cx| {
            r.run_screen().map(|s| s.read(cx).run_id().clone())
        })
    };
    assert_eq!(open_run(&root, vcx), Some(run_id("render1")));
    root.update_in(vcx, |root, window, cx| {
        root.draft33_hook(OpenHook::Stream, window, cx);
    });
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| {
            s.state()
                .runs()
                .iter()
                .any(|r| r.prompt.as_deref() == Some("/longreply"))
        })
    });
    wait_drawn(vcx, |vcx| open_run(&root, vcx) != Some(run_id("render1")));
}
