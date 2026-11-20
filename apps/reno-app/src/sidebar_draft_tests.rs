//! ROAD-32: the sidebar draft against the fake daemon's mocked thread desk. Each test locks one
//! decided thing in each variant (what is out of sight is said in words, Esc backs out, the real
//! daemon hides the mocked controls); they assert state and words, never pixels.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use gpui::{Entity, TestAppContext, VisualTestContext};
use reno_client::fake::FakeConfig;
use reno_proto::ThreadId;

use crate::app::RootView;
use crate::store::Store;
use crate::tests::{
    click, draw, one_window_test_at_a_time, painted, wait_drawn, wait_until, window,
};
use crate::ui::draft32::{self, Act};
use crate::ui::theme::Appearance;
use crate::vm::capabilities::Capabilities;
use crate::vm::draft32::{Item, Variant};

fn thread(suffix: &str) -> ThreadId {
    ThreadId::from_suffix(suffix).unwrap()
}

/// The demo world of `--fake --draft32`: the activity runs and the sidebar's threads.
fn draft_store(cx: &mut TestAppContext) -> Entity<Store> {
    use gpui::AppContext as _;
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
            s.thread_of(&crate::tests::run_id("plan1")).is_some()
                && s.thread_of(&crate::tests::run_id("atlas11")).is_some()
        })
    });
    store
}

fn open(
    cx: &mut TestAppContext,
    variant: Variant,
    caps: Capabilities,
) -> (Entity<Store>, Entity<RootView>, &mut VisualTestContext) {
    let store = draft_store(cx);
    store.update(cx, |s, cx| s.set_capabilities(caps, cx));
    cx.update(|cx| draft32::install(variant, cx));
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    wait_drawn(vcx, |vcx| {
        painted(vcx, "recent-run_plan1") || painted(vcx, "chip-run_plan1")
    });
    // As in the app: the composer has the focus.
    root.update_in(vcx, |root, window, cx| root.focus_composer(window, cx));
    draw(vcx);
    (store, root, vcx)
}

fn act(vcx: &mut VisualTestContext, root: &Entity<RootView>, act: Act) {
    root.update_in(vcx, |root, window, cx| root.draft32_act(act, window, cx));
    draw(vcx);
}

fn failure(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> Option<String> {
    root.read_with(vcx, |r, _| {
        r.draft32_state()
            .failure
            .as_ref()
            .map(|f| f.message.clone())
    })
}

#[gpui::test]
fn a_project_with_fourteen_threads_says_show_6_more_in_every_variant(cx: &mut TestAppContext) {
    for variant in [Variant::A, Variant::B, Variant::C] {
        let (_store, root, vcx) = open(cx, variant, Capabilities::FAKE);
        assert!(
            painted(vcx, "more-atlas-api"),
            "{variant:?}: the words are on screen"
        );
        // The page holds eight; the other six are out of sight until it is asked.
        assert!(painted(vcx, "recent-run_atlas05") || variant == Variant::B);
        assert!(!painted(vcx, "recent-run_atlas11"));
        act(
            vcx,
            &root,
            Act::ToggleMore(crate::vm::projects::FolderKey::Repo(
                "/demo/atlas-api".into(),
            )),
        );
        assert!(root.read_with(vcx, |r, _| r.draft32_state().expanded.len() == 1));
    }
}

#[gpui::test]
fn what_is_archived_is_said_in_words_where_each_variant_keeps_it(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open(cx, Variant::A, Capabilities::FAKE);
    assert!(
        painted(vcx, "archived-row"),
        "A: \"Archived \u{b7} 4\" closes the list"
    );
    assert!(!painted(vcx, "unarchive-run_arch1"));
    click(vcx, "archived-row");
    assert!(root.read_with(vcx, |r, _| r.draft32_state().archived_open));
    wait_drawn(vcx, |vcx| painted(vcx, "unarchive-run_arch1"));

    vcx.update(|_, cx| draft32::cycle(true, cx));
    draw(vcx);
    assert!(
        painted(vcx, "archived-count"),
        "B: \"4 archived\" in the header"
    );
    click(vcx, "draft32-filter");
    assert!(painted(vcx, "draft32-filter-archived"));
    click(vcx, "draft32-filter-archived");
    wait_drawn(vcx, |vcx| painted(vcx, "unarchive-run_arch1"));
    assert!(
        !painted(vcx, "recent-run_plan1"),
        "the filter swaps the list"
    );
}

#[gpui::test]
fn archiving_a_thread_with_a_live_run_is_refused_in_words_and_changes_nothing(
    cx: &mut TestAppContext,
) {
    let (store, root, vcx) = open(cx, Variant::A, Capabilities::FAKE);
    act(vcx, &root, Act::Do(thread("plan1"), Item::Archive));
    assert_eq!(
        failure(vcx, &root).as_deref(),
        Some("Stop the run before archiving")
    );
    assert!(painted(vcx, "failed-run_plan1"), "said under the row");
    let marks = store.read_with(vcx, |s, _| s.thread_marks().unwrap());
    assert_eq!(marks.archived.len(), 4);
    assert!(marks.pinned.contains(&thread("plan1")));
    // An idle thread goes into the archive and out of the list.
    assert!(painted(vcx, "recent-run_atlas01"));
    act(vcx, &root, Act::Do(thread("atlas01"), Item::Archive));
    assert_eq!(failure(vcx, &root), None);
    assert!(!painted(vcx, "recent-run_atlas01"));
    let marks = store.read_with(vcx, |s, _| s.thread_marks().unwrap());
    assert_eq!(marks.archived[0], thread("atlas01"));
}

#[gpui::test]
fn an_empty_title_is_refused_and_the_field_stays(cx: &mut TestAppContext) {
    let (store, root, vcx) = open(cx, Variant::A, Capabilities::FAKE);
    act(vcx, &root, Act::Do(thread("plan1"), Item::Rename));
    let field = root
        .read_with(vcx, |r, _| {
            r.draft32_state()
                .rename_of(&thread("plan1"), false)
                .cloned()
        })
        .expect("the title is a field now");
    field.update(vcx, |f, cx| f.set_text("   ", cx));
    act(vcx, &root, Act::RenameSubmit);
    assert_eq!(
        failure(vcx, &root).as_deref(),
        Some("A thread needs a title")
    );
    assert!(root.read_with(vcx, |r, _| {
        r.draft32_state()
            .rename_of(&thread("plan1"), false)
            .is_some()
    }));
    field.update(vcx, |f, cx| f.set_text("  ACP v2, done properly ", cx));
    act(vcx, &root, Act::RenameSubmit);
    assert!(root.read_with(vcx, |r, _| {
        r.draft32_state()
            .rename_of(&thread("plan1"), false)
            .is_none()
    }));
    let titles = store.read_with(vcx, |s, _| s.thread_marks().unwrap().titles);
    assert_eq!(titles[&thread("plan1")], "ACP v2, done properly");
}

#[gpui::test]
fn escape_backs_out_of_a_menu_and_a_rename_while_the_composer_has_the_focus(
    cx: &mut TestAppContext,
) {
    let (_store, root, vcx) = open(cx, Variant::A, Capabilities::FAKE);
    act(vcx, &root, Act::Menu(thread("plan1"), None));
    assert!(painted(vcx, "draft32-menu"));
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(!painted(vcx, "draft32-menu"), "Esc closes the menu");

    act(vcx, &root, Act::Menu(thread("plan1"), None));
    act(vcx, &root, Act::Do(thread("plan1"), Item::Copy));
    assert!(painted(vcx, "draft32-menu-back"));
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(
        painted(vcx, "draft32-menu") && !painted(vcx, "draft32-menu-back"),
        "back out one step"
    );
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(!painted(vcx, "draft32-menu"));

    act(vcx, &root, Act::Do(thread("plan1"), Item::Rename));
    assert!(root.read_with(vcx, |r, _| r.draft32_state().any_open()));
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(
        !root.read_with(vcx, |r, _| r.draft32_state().any_open()),
        "Esc leaves the rename"
    );
}

#[gpui::test]
fn the_real_daemon_hides_rename_pin_and_archive_and_keeps_copy_and_close(cx: &mut TestAppContext) {
    for variant in [Variant::A, Variant::B, Variant::C] {
        let (store, root, vcx) = open(cx, variant, Capabilities::REAL);
        assert!(!painted(vcx, "archived-row") && !painted(vcx, "archived-count"));
        assert!(!painted(vcx, "pinned-section") && !painted(vcx, "pinned-strip"));
        let facts = store.read_with(vcx, |s, _| {
            crate::ui::draft32::State::facts(s, &thread("reviewer1"))
        });
        assert!(!facts.edit && facts.known);
        let menu = crate::vm::draft32::menu(variant, facts);
        assert_eq!(menu.len(), 3, "{variant:?}: Copy, a divider, Close thread");
        // Hover shows no pin or archive icons.
        act(vcx, &root, Act::Hover(thread("reviewer1"), true));
        assert!(!painted(vcx, "pin-run_reviewer1") && !painted(vcx, "archive-run_reviewer1"));
        // "Show 6 more" is real.
        assert!(painted(vcx, "more-atlas-api"), "{variant:?}");
    }
}

#[gpui::test]
fn closing_always_asks_and_a_live_thread_cannot_be_closed(cx: &mut TestAppContext) {
    let (store, root, vcx) = open(cx, Variant::A, Capabilities::FAKE);
    act(vcx, &root, Act::Menu(thread("atlas01"), None));
    click(vcx, "draft32-menu-close-thread\u{2026}");
    assert!(painted(vcx, "draft32-confirm"), "a question, not a close");
    assert!(root.read_with(vcx, |r, _| r.draft32_state().confirm.is_some()));
    assert!(!store.read_with(vcx, |s, _| s.is_closing(&thread("atlas01"))));
    act(vcx, &root, Act::Dismiss);
    assert!(!painted(vcx, "draft32-confirm"));

    // The live run: the question says so and does not close.
    act(vcx, &root, Act::Do(thread("plan1"), Item::Close));
    act(vcx, &root, Act::ConfirmClose);
    assert!(!store.read_with(vcx, |s, _| s.is_closing(&thread("plan1"))));
}

#[gpui::test]
fn c_opens_its_card_after_the_pointer_rests_and_closes_it_when_it_leaves(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open(cx, Variant::C, Capabilities::FAKE);
    act(vcx, &root, Act::Hover(thread("atlas01"), true));
    vcx.executor().advance_clock(Duration::from_millis(300));
    vcx.run_until_parked();
    assert!(
        root.read_with(vcx, |r, _| r.draft32_state().card.is_none()),
        "not before 400 ms"
    );
    vcx.executor().advance_clock(Duration::from_millis(150));
    vcx.run_until_parked();
    draw(vcx);
    assert!(root.read_with(vcx, |r, _| r.draft32_state().card.is_some()));
    assert!(painted(vcx, "draft32-card"));
    act(vcx, &root, Act::Hover(thread("atlas01"), false));
    vcx.executor().advance_clock(Duration::from_millis(350));
    vcx.run_until_parked();
    assert!(root.read_with(vcx, |r, _| r.draft32_state().card.is_none()));
}

#[gpui::test]
fn the_switch_cycles_the_variants_and_starts_each_from_a_quiet_sidebar(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open(cx, Variant::A, Capabilities::FAKE);
    assert!(painted(vcx, "draft32-label"));
    act(vcx, &root, Act::Menu(thread("plan1"), None));
    vcx.update(|_, cx| draft32::cycle(true, cx));
    draw(vcx);
    assert_eq!(vcx.update(|_, cx| draft32::current(cx)), Some(Variant::B));
    assert!(
        !root.read_with(vcx, |r, _| r.draft32_state().any_open()),
        "a new variant starts quiet"
    );
    vcx.update(|_, cx| draft32::cycle(true, cx));
    vcx.update(|_, cx| draft32::cycle(true, cx));
    assert_eq!(vcx.update(|_, cx| draft32::current(cx)), Some(Variant::A));
}

#[gpui::test]
fn the_sidebar_is_as_it_was_without_the_draft(cx: &mut TestAppContext) {
    let store = draft_store(cx);
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    wait_drawn(vcx, |vcx| painted(vcx, "recent-run_plan1"));
    for absent in [
        "draft32-label",
        "archived-row",
        "pinned-section",
        "pinned-strip",
        "sidebar-resize",
    ] {
        assert!(!painted(vcx, absent), "{absent}");
    }
    // No "Show more", no archive: only the first eight threads of a project, as ever.
    assert!(!painted(vcx, "more-atlas-api"));
}

#[gpui::test]
fn the_edge_resizes_and_remembers_the_width(cx: &mut TestAppContext) {
    let (_store, root, vcx) = open(cx, Variant::A, Capabilities::FAKE);
    act(vcx, &root, Act::Resize(300.0));
    assert!((root.read_with(vcx, |r, _| r.draft32_state().width) - 300.0).abs() < 0.01);
    act(vcx, &root, Act::Resize(9000.0));
    assert!(
        (root.read_with(vcx, |r, _| r.draft32_state().width) - 380.0).abs() < 0.01,
        "clamped"
    );
    act(vcx, &root, Act::Resize(260.0));
    act(vcx, &root, Act::ResizeEnd);
    let saved = vcx.update(|_, cx| crate::prefs::current(cx).sidebar_width);
    assert!((saved - 260.0).abs() < 0.01, "kept as a pref");
    let width = vcx.debug_bounds("sidebar").expect("sidebar").size.width;
    assert!((f32::from(width) - 260.0).abs() < 0.6);
}
