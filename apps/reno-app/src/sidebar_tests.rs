//! ROAD-32: the sidebar's thread controls against the fake daemon's mocked thread desk. Each test
//! locks one decided thing of the locked brief; they assert state and words, never pixels. "Real"
//! is the fake with the real daemon's capabilities.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_client::fake::FakeConfig;
use reno_proto::ThreadId;

use crate::app::RootView;
use crate::store::Store;
use crate::tests::{
    click, draw, one_window_test_at_a_time, painted, run_id, wait_drawn, wait_until, window,
};
use crate::ui::sidebar::Act;
use crate::ui::theme::{Appearance, Theme};
use crate::vm::capabilities::Capabilities;
use crate::vm::projects::FolderKey;
use crate::vm::threads::{self, Entry, Item};

fn thread(suffix: &str) -> ThreadId {
    ThreadId::from_suffix(suffix).unwrap()
}

/// The demo world of `--fake`: the activity runs and the sidebar's threads.
fn demo_store(cx: &mut TestAppContext) -> Entity<Store> {
    one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(
            FakeConfig {
                step: Duration::ZERO,
                question_run: true,
                question_demo_runs: true,
                activity_runs: true,
                sidebar_threads: true,
                ..FakeConfig::default()
            },
            cx,
        );
        store
    });
    // Every thread is named (`thread.list` was read) and the runs are in.
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| {
            s.thread_of(&run_id("plan1")).is_some() && s.thread_of(&run_id("atlas11")).is_some()
        })
    });
    store
}

fn open(
    cx: &mut TestAppContext,
    caps: Capabilities,
) -> (Entity<Store>, Entity<RootView>, &mut VisualTestContext) {
    let store = demo_store(cx);
    store.update(cx, |s, cx| s.set_capabilities(caps, cx));
    // The app's key bindings (⌘⇧P, ⌘⇧A, Escape) are not bound by a test window.
    cx.update(crate::actions::bind_keys);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    wait_drawn(vcx, |vcx| {
        painted(vcx, "recent-run_atlas01") || painted(vcx, "recent-run_plan1")
    });
    // As in the app: the composer has the focus.
    root.update_in(vcx, |root, window, cx| root.focus_composer(window, cx));
    draw(vcx);
    (store, root, vcx)
}

fn act(vcx: &mut VisualTestContext, root: &Entity<RootView>, act: Act) {
    root.update_in(vcx, |root, window, cx| root.sidebar_act(act, window, cx));
    draw(vcx);
}

fn failure(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> Option<String> {
    root.read_with(vcx, |r, _| {
        r.sidebar_state()
            .failure
            .as_ref()
            .map(|f| f.message.clone())
    })
}

fn marks(
    vcx: &mut VisualTestContext,
    store: &Entity<Store>,
) -> reno_client::thread_desk::ThreadMarks {
    store.read_with(vcx, |s, _| s.thread_marks().unwrap())
}

fn inside(vcx: &mut VisualTestContext, outer: &'static str, inner: &str) -> bool {
    let o = vcx.debug_bounds(outer).expect("outer painted");
    let key: &'static str = Box::leak(inner.to_owned().into_boxed_str());
    vcx.debug_bounds(key)
        .is_some_and(|i| o.contains(&i.center()))
}

fn clipboard(vcx: &mut VisualTestContext) -> Option<String> {
    vcx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
}

#[gpui::test]
fn the_row_menu_lists_rename_pin_archive_copy_a_divider_and_close_in_the_danger_tone(
    cx: &mut TestAppContext,
) {
    let (store, root, vcx) = open(cx, Capabilities::FAKE);
    let facts = store.read_with(vcx, |s, _| {
        crate::ui::sidebar::State::facts(s, &thread("atlas01"), false)
    });
    let entries = threads::menu(facts);
    let shape: Vec<_> = entries
        .iter()
        .map(|e| match e {
            Entry::Item { label, danger, .. } => (*label, *danger),
            Entry::Divider => ("-", false),
        })
        .collect();
    assert_eq!(
        shape,
        [
            ("Rename", false),
            ("Pin", false),
            ("Archive", false),
            ("Copy", false),
            ("-", false),
            ("Close thread\u{2026}", true),
        ]
    );
    // Drawn in that order, under the row.
    act(vcx, &root, Act::Menu(thread("atlas01"), None));
    let y = |vcx: &mut VisualTestContext, id: &'static str| {
        f32::from(
            vcx.debug_bounds(id)
                .unwrap_or_else(|| panic!("{id}"))
                .origin
                .y,
        )
    };
    let order = [
        "sidebar-menu-rename",
        "sidebar-menu-pin",
        "sidebar-menu-archive",
        "sidebar-menu-copy",
        "sidebar-menu-close-thread\u{2026}",
    ];
    let ys: Vec<f32> = order.iter().map(|id| y(vcx, id)).collect();
    assert!(ys.windows(2).all(|w| w[0] < w[1]), "{ys:?}");
    // A pinned thread offers Unpin in the same place.
    act(vcx, &root, Act::Dismiss);
    act(vcx, &root, Act::Do(thread("atlas01"), Item::Pin));
    act(vcx, &root, Act::Menu(thread("atlas01"), None));
    assert!(painted(vcx, "sidebar-menu-unpin") && !painted(vcx, "sidebar-menu-pin"));
}

#[gpui::test]
fn the_shortcuts_pin_and_archive_the_thread_of_the_open_menu(cx: &mut TestAppContext) {
    let (store, root, vcx) = open(cx, Capabilities::FAKE);
    let facts = store.read_with(vcx, |s, _| {
        crate::ui::sidebar::State::facts(s, &thread("atlas01"), false)
    });
    let hints: Vec<_> = threads::menu(facts)
        .iter()
        .filter_map(|e| match e {
            Entry::Item { hint, .. } => *hint,
            Entry::Divider => None,
        })
        .collect();
    assert_eq!(hints, ["\u{2318}\u{21e7}P", "\u{2318}\u{21e7}A"]);

    act(vcx, &root, Act::Menu(thread("atlas01"), None));
    vcx.simulate_keystrokes("cmd-shift-p");
    draw(vcx);
    assert!(
        marks(vcx, &store).pinned.contains(&thread("atlas01")),
        "\u{2318}\u{21e7}P pins"
    );
    act(vcx, &root, Act::Menu(thread("atlas01"), None));
    vcx.simulate_keystrokes("cmd-shift-p");
    draw(vcx);
    assert!(
        !marks(vcx, &store).pinned.contains(&thread("atlas01")),
        "and unpins"
    );

    act(vcx, &root, Act::Menu(thread("atlas02"), None));
    vcx.simulate_keystrokes("cmd-shift-a");
    draw(vcx);
    assert_eq!(
        marks(vcx, &store).archived[0],
        thread("atlas02"),
        "\u{2318}\u{21e7}A archives"
    );
}

#[gpui::test]
fn rename_saves_on_enter_cancels_on_escape_and_refuses_an_empty_title(cx: &mut TestAppContext) {
    let (store, root, vcx) = open(cx, Capabilities::FAKE);
    let field = |vcx: &mut VisualTestContext| {
        root.read_with(vcx, |r, _| {
            r.sidebar_state().rename_of(&thread("atlas01")).cloned()
        })
    };
    // Enter saves.
    act(vcx, &root, Act::Do(thread("atlas01"), Item::Rename));
    let f = field(vcx).expect("the title is a field now");
    f.update(vcx, |f, cx| f.set_text("  Cursor paging, done  ", cx));
    vcx.simulate_keystrokes("enter");
    draw(vcx);
    assert!(field(vcx).is_none(), "the field is gone");
    assert_eq!(
        marks(vcx, &store).titles[&thread("atlas01")],
        "Cursor paging, done"
    );

    // Escape cancels and keeps the old title.
    act(vcx, &root, Act::Do(thread("atlas01"), Item::Rename));
    let f = field(vcx).unwrap();
    f.update(vcx, |f, cx| f.set_text("Something else", cx));
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(field(vcx).is_none(), "Esc leaves the rename");
    assert_eq!(
        marks(vcx, &store).titles[&thread("atlas01")],
        "Cursor paging, done"
    );

    // An empty title is refused in words and the field stays.
    act(vcx, &root, Act::Do(thread("atlas01"), Item::Rename));
    let f = field(vcx).unwrap();
    f.update(vcx, |f, cx| f.set_text("   ", cx));
    vcx.simulate_keystrokes("enter");
    draw(vcx);
    assert_eq!(
        failure(vcx, &root).as_deref(),
        Some("A thread needs a title")
    );
    assert!(painted(vcx, "failed-run_atlas01"), "said under the row");
    assert!(field(vcx).is_some(), "the field stays");
    assert_eq!(
        marks(vcx, &store).titles[&thread("atlas01")],
        "Cursor paging, done"
    );
}

#[gpui::test]
fn pinning_moves_a_thread_into_the_pinned_section_and_unpinning_moves_it_back(
    cx: &mut TestAppContext,
) {
    let (_store, root, vcx) = open(cx, Capabilities::FAKE);
    assert!(painted(vcx, "pinned-section"), "two are pinned in the demo");
    assert!(inside(vcx, "pinned-section", "recent-run_plan1"));
    assert!(!inside(vcx, "pinned-section", "recent-run_atlas01"));
    act(vcx, &root, Act::Do(thread("atlas01"), Item::Pin));
    assert!(
        inside(vcx, "pinned-section", "recent-run_atlas01"),
        "pinned: in the section"
    );
    act(vcx, &root, Act::Do(thread("atlas01"), Item::Unpin));
    assert!(
        !inside(vcx, "pinned-section", "recent-run_atlas01"),
        "unpinned: back in atlas-api"
    );
    assert!(painted(vcx, "recent-run_atlas01"));
}

#[gpui::test]
fn archiving_moves_a_thread_into_archived_n_and_unarchive_brings_it_back(cx: &mut TestAppContext) {
    let (store, root, vcx) = open(cx, Capabilities::FAKE);
    assert!(
        painted(vcx, "archived-row"),
        "\"Archived \u{b7} 4\" closes the list"
    );
    assert_eq!(
        threads::archived_label(marks(vcx, &store).archived.len()),
        "Archived \u{b7} 4"
    );
    act(vcx, &root, Act::Do(thread("atlas01"), Item::Archive));
    assert!(!painted(vcx, "recent-run_atlas01"), "out of the list");
    assert_eq!(
        threads::archived_label(marks(vcx, &store).archived.len()),
        "Archived \u{b7} 5"
    );
    click(vcx, "archived-row");
    wait_drawn(vcx, |vcx| painted(vcx, "unarchive-run_atlas01"));
    click(vcx, "unarchive-run_atlas01");
    assert!(!marks(vcx, &store).archived.contains(&thread("atlas01")));
    assert!(painted(vcx, "recent-run_atlas01"), "back in its project");
}

#[gpui::test]
fn archiving_the_live_run_says_stop_the_run_before_archiving(cx: &mut TestAppContext) {
    let (store, root, vcx) = open(cx, Capabilities::FAKE);
    act(vcx, &root, Act::Do(thread("plan1"), Item::Archive));
    assert_eq!(
        failure(vcx, &root).as_deref(),
        Some("Stop the run before archiving")
    );
    assert!(painted(vcx, "failed-run_plan1"), "said under the row");
    let m = marks(vcx, &store);
    assert_eq!(m.archived.len(), 4, "nothing was archived");
    assert!(m.pinned.contains(&thread("plan1")), "and it keeps its pin");
}

#[gpui::test]
fn a_long_group_ends_in_show_n_more_and_expands(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open(cx, Capabilities::FAKE);
    assert!(painted(vcx, "more-atlas-api"), "\"Show 6 more\"");
    assert!(!painted(vcx, "recent-run_atlas11"), "six are out of sight");
    act(
        vcx,
        &root,
        Act::ToggleMore(FolderKey::Repo("/demo/atlas-api".into())),
    );
    assert!(
        painted(vcx, "recent-run_atlas11"),
        "all fourteen are listed"
    );
    assert!(painted(vcx, "more-atlas-api"), "\"Show less\" stays");
    act(
        vcx,
        &root,
        Act::ToggleMore(FolderKey::Repo("/demo/atlas-api".into())),
    );
    assert!(!painted(vcx, "recent-run_atlas11"));
}

#[gpui::test]
fn close_thread_always_asks_with_the_worktree_copy_and_a_live_run_says_why_not(
    cx: &mut TestAppContext,
) {
    let (store, root, vcx) = open(cx, Capabilities::FAKE);
    act(vcx, &root, Act::Menu(thread("atlas01"), None));
    click(vcx, "sidebar-menu-close-thread\u{2026}");
    assert!(painted(vcx, "sidebar-confirm"), "a question, not a close");
    assert!(!store.read_with(vcx, |s, _| s.is_closing(&thread("atlas01"))));
    // Said from what the worktree holds, once it is read.
    let said = |vcx: &mut VisualTestContext| {
        store.read_with(vcx, |s, _| {
            let info = s.state().thread(&thread("atlas01")).unwrap();
            let held = s
                .changes(&run_id("atlas01"))
                .and_then(|c| match &c.workspace {
                    crate::vm::changes::Loaded::Ready(Some(w)) => w.changes,
                    _ => None,
                });
            threads::close_question(info, held.as_ref())
        })
    };
    wait_drawn(vcx, |vcx| said(vcx).contains("not merged"));
    assert_eq!(
        said(vcx),
        "reno/atlas01 with 3 commits not merged and uncommitted changes stays in the worktree: closing removes nothing."
    );
    act(vcx, &root, Act::Dismiss);
    assert!(!painted(vcx, "sidebar-confirm"), "Esc or Cancel backs out");

    // The live run: the busy line, and the button does nothing.
    act(vcx, &root, Act::Do(thread("plan1"), Item::Close));
    assert!(painted(vcx, "sidebar-close-confirm"));
    store.read_with(vcx, |s, _| {
        let info = s.state().thread(&thread("plan1")).unwrap();
        assert_eq!(threads::close_question(info, None), threads::BUSY);
    });
    click(vcx, "sidebar-close-confirm");
    act(vcx, &root, Act::ConfirmClose);
    assert!(
        !store.read_with(vcx, |s, _| s.is_closing(&thread("plan1"))),
        "a live run is not closed"
    );

    // A confirmed close on an idle thread goes to the daemon.
    act(vcx, &root, Act::Dismiss);
    act(vcx, &root, Act::Do(thread("atlas01"), Item::Close));
    click(vcx, "sidebar-close-confirm");
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| {
            s.is_closing(&thread("atlas01")) || s.close_result(&thread("atlas01")).is_some()
        })
    });
}

#[gpui::test]
fn copy_copies_the_path_the_thread_id_and_the_harness_session_id(cx: &mut TestAppContext) {
    let (store, root, vcx) = open(cx, Capabilities::FAKE);
    let (path, session) = store.read_with(vcx, |s, _| {
        let info = s.state().thread(&thread("atlas01")).unwrap();
        (
            info.worktree.as_ref().unwrap().display().to_string(),
            s.state()
                .run(&run_id("atlas01"))
                .unwrap()
                .session
                .clone()
                .unwrap()
                .0,
        )
    });
    act(vcx, &root, Act::Menu(thread("atlas01"), None));
    click(vcx, "sidebar-menu-copy");
    assert!(
        painted(vcx, "sidebar-menu-path"),
        "the sub-list is in place"
    );
    click(vcx, "sidebar-menu-path");
    assert_eq!(clipboard(vcx).as_deref(), Some(path.as_str()));
    for (open_id, id, want) in [
        (
            "sidebar-menu-copy",
            "sidebar-menu-thread-id",
            "thr_atlas01".to_owned(),
        ),
        (
            "sidebar-menu-copy",
            "sidebar-menu-harness-session-id",
            session,
        ),
    ] {
        act(vcx, &root, Act::Menu(thread("atlas01"), None));
        click(vcx, open_id);
        click(vcx, id);
        assert_eq!(clipboard(vcx), Some(want), "{id}");
    }
}

#[gpui::test]
fn the_edge_drags_clamps_to_200_and_380_and_is_remembered(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open(cx, Capabilities::FAKE);
    let width = |vcx: &mut VisualTestContext| root.read_with(vcx, |r, _| r.sidebar_state().width);
    act(vcx, &root, Act::Resize(300.0));
    assert!((width(vcx) - 300.0).abs() < 0.01);
    act(vcx, &root, Act::Resize(9000.0));
    assert!((width(vcx) - 380.0).abs() < 0.01, "at most 380");
    act(vcx, &root, Act::Resize(10.0));
    assert!((width(vcx) - 200.0).abs() < 0.01, "at least 200");
    act(vcx, &root, Act::Resize(260.0));
    act(vcx, &root, Act::ResizeEnd);
    let saved = vcx.update(|_, cx| crate::prefs::current(cx).sidebar_width);
    assert!((saved - 260.0).abs() < 0.01, "kept as a pref");
    let painted_width = vcx.debug_bounds("sidebar").expect("sidebar").size.width;
    assert!((f32::from(painted_width) - 260.0).abs() < 0.6);
}

#[gpui::test]
fn the_real_daemon_hides_rename_pin_archive_pinned_and_archived_and_keeps_copy_and_close(
    cx: &mut TestAppContext,
) {
    let (store, root, vcx) = open(cx, Capabilities::REAL);
    assert!(!painted(vcx, "pinned-section") && !painted(vcx, "archived-row"));
    let facts = store.read_with(vcx, |s, _| {
        crate::ui::sidebar::State::facts(s, &thread("atlas01"), false)
    });
    assert!(!facts.edit && facts.known);
    let labels: Vec<_> = threads::menu(facts)
        .iter()
        .filter_map(|e| match e {
            Entry::Item { label, .. } => Some(*label),
            Entry::Divider => None,
        })
        .collect();
    assert_eq!(labels, ["Copy", "Close thread\u{2026}"]);
    act(vcx, &root, Act::Menu(thread("atlas01"), None));
    assert!(painted(vcx, "sidebar-menu-copy") && painted(vcx, "sidebar-menu-close-thread\u{2026}"));
    assert!(
        !painted(vcx, "sidebar-menu-rename")
            && !painted(vcx, "sidebar-menu-pin")
            && !painted(vcx, "sidebar-menu-archive")
    );
    // Hover details, "Show 6 more" and the edge are real.
    assert!(painted(vcx, "more-atlas-api") && painted(vcx, "sidebar-resize"));
    let _ = Theme::for_appearance(Appearance::Dark);
}

#[gpui::test]
fn escape_closes_a_menu_one_step_at_a_time_while_the_composer_has_the_focus(
    cx: &mut TestAppContext,
) {
    let (_store, root, vcx) = open(cx, Capabilities::FAKE);
    act(vcx, &root, Act::Menu(thread("plan1"), None));
    act(vcx, &root, Act::CopyList(true));
    assert!(painted(vcx, "sidebar-menu-back"));
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(
        painted(vcx, "sidebar-menu") && !painted(vcx, "sidebar-menu-back"),
        "back one step"
    );
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(!painted(vcx, "sidebar-menu"));
}
