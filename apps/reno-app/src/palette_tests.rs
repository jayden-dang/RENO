//! The ⌘K palette against the fake daemon (brief: docs/design/2026-11-19-palette-ui-brief.md):
//! it opens on what needs you, ↵ opens a thread, typing filters, tabs scope, the focused
//! thread's files are searched, Esc closes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};

use crate::app::RootView;
use crate::store::Store;
use crate::tests::{approvals, click, draw, painted, wait_drawn, wait_until, window};
use crate::ui::theme::Appearance;
use crate::vm::palette::{Found, Item, Kind, Sheet, Tab};

/// The fake world `--fake` shows: two held approvals and the asker's question, connected, with
/// the app's keys bound.
fn held(cx: &mut TestAppContext) -> Entity<Store> {
    crate::tests::one_window_test_at_a_time();
    cx.update(|cx| {
        gpui_tokio::init(cx);
        crate::actions::bind_keys(cx);
    });
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(
            reno_client::fake::FakeConfig {
                question_run: true,
                ..crate::tests::instant_script()
            },
            cx,
        );
        store
    });
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| {
            s.state()
                .runs()
                .iter()
                .any(|r| r.status == reno_proto::RunStatus::WaitingInput)
        })
    });
    store
}

/// The window as the app opens it: the composer has the keyboard.
fn opened<'a>(
    cx: &'a mut TestAppContext,
    store: &Entity<Store>,
) -> (Entity<RootView>, &'a mut VisualTestContext) {
    let (root, vcx) = window(cx, store, Appearance::Dark);
    root.update_in(vcx, |r, window, cx| r.focus_composer(window, cx));
    draw(vcx);
    (root, vcx)
}

fn sheet(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> Sheet {
    root.read_with(vcx, |r, cx| {
        r.palette().expect("the palette is open").read(cx).sheet()
    })
}

fn selected(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> Option<Item> {
    root.read_with(vcx, |r, cx| {
        r.palette()
            .expect("the palette is open")
            .read(cx)
            .selected()
    })
}

fn current_run(vcx: &mut VisualTestContext, root: &Entity<RootView>) -> Option<String> {
    root.read_with(vcx, |r, cx| {
        r.run_screen().map(|s| s.read(cx).run_id().to_string())
    })
}

fn thread_title(item: &Item) -> &str {
    match item {
        Item::Thread(t) => &t.title,
        other => panic!("not a thread: {other:?}"),
    }
}

#[gpui::test]
fn command_k_opens_on_the_threads_that_need_you(cx: &mut TestAppContext) {
    let store = held(cx);
    let (root, vcx) = opened(cx, &store);
    vcx.simulate_keystrokes("cmd-k");
    draw(vcx);
    let s = sheet(vcx, &root);
    assert_eq!(s.sections[0].kind, Kind::NeedsYou);
    assert_eq!(s.sections[0].finding.as_deref(), Some("3 threads need you"));
    assert!(painted(vcx, "palette-finding"));
    assert!(painted(vcx, "palette-tab-commands"));
    assert!(painted(vcx, "palette-row-0"));
}

#[gpui::test]
fn enter_opens_the_first_waiting_thread(cx: &mut TestAppContext) {
    let store = held(cx);
    let (root, vcx) = opened(cx, &store);
    vcx.simulate_keystrokes("cmd-k");
    draw(vcx);
    let first = match selected(vcx, &root) {
        Some(Item::Thread(t)) => t.run.to_string(),
        other => panic!("expected a thread, got {other:?}"),
    };
    vcx.simulate_keystrokes("enter");
    draw(vcx);
    assert!(!root.read_with(vcx, |r, _| r.palette_open()));
    assert_eq!(current_run(vcx, &root), Some(first));
}

#[gpui::test]
fn typing_filters_the_threads(cx: &mut TestAppContext) {
    let store = held(cx);
    let (root, vcx) = opened(cx, &store);
    vcx.simulate_keystrokes("cmd-k a d v i s");
    draw(vcx);
    let s = sheet(vcx, &root);
    let threads = s.sections.iter().find(|x| x.kind == Kind::Threads).unwrap();
    assert_eq!(thread_title(&threads.items[0]), "check upstream advisories");
}

#[gpui::test]
fn a_waiting_thread_is_one_line_whatever_it_waits_for(cx: &mut TestAppContext) {
    let store = held(cx);
    let (root, vcx) = opened(cx, &store);
    vcx.simulate_keystrokes("cmd-k");
    draw(vcx);
    assert_eq!(
        thread_title(&selected(vcx, &root).unwrap()),
        "pick the retry policy"
    );
    assert!(
        !painted(vcx, "palette-held"),
        "a question: the badge says it"
    );
    vcx.simulate_keystrokes("down");
    draw(vcx);
    assert_eq!(
        thread_title(&selected(vcx, &root).unwrap()),
        "check upstream advisories"
    );
    assert!(
        !painted(vcx, "palette-held"),
        "a held action: the badge says it too"
    );
}

#[gpui::test]
fn tabs_scope_by_click_by_command_digit_and_by_sign(cx: &mut TestAppContext) {
    let store = held(cx);
    let (root, vcx) = opened(cx, &store);
    vcx.simulate_keystrokes("cmd-k");
    draw(vcx);
    click(vcx, "palette-tab-commands");
    assert_eq!(sheet(vcx, &root).tab, Tab::Commands);
    vcx.simulate_keystrokes("cmd-2");
    draw(vcx);
    assert_eq!(sheet(vcx, &root).tab, Tab::Threads);
    vcx.simulate_keystrokes("tab");
    draw(vcx);
    assert_eq!(sheet(vcx, &root).tab, Tab::Files);
    vcx.simulate_keystrokes("cmd-1 > s e t t");
    draw(vcx);
    let s = sheet(vcx, &root);
    assert_eq!(s.tab, Tab::Commands);
    match &s.sections[0].items[0] {
        Item::Command(e) => assert_eq!(e.label, "Settings"),
        other => panic!("{other:?}"),
    }
}

#[gpui::test]
fn on_home_the_file_sections_ask_for_a_thread(cx: &mut TestAppContext) {
    let store = held(cx);
    let (root, vcx) = opened(cx, &store);
    vcx.simulate_keystrokes("cmd-k m a i n");
    draw(vcx);
    let s = sheet(vcx, &root);
    let files = s.sections.iter().find(|x| x.kind == Kind::Files).unwrap();
    assert_eq!(
        files.note.as_ref().map(|n| n.text.as_str()),
        Some("Open a thread to search its project's files")
    );
    assert!(painted(vcx, "palette-note-files"));
}

#[gpui::test]
fn in_a_thread_its_files_and_lines_are_searched(cx: &mut TestAppContext) {
    let store = held(cx);
    let (root, vcx) = opened(cx, &store);
    click(vcx, "recent-run_reviewer1");
    draw(vcx);
    vcx.simulate_keystrokes("cmd-k m a i n");
    wait_drawn(vcx, |vcx| {
        let s = sheet(vcx, &root);
        s.sections
            .iter()
            .any(|x| x.kind == Kind::Text && !x.items.is_empty())
    });
    let s = sheet(vcx, &root);
    let files = s.sections.iter().find(|x| x.kind == Kind::Files).unwrap();
    assert!(
        files
            .items
            .iter()
            .any(|i| *i == Item::File("src/main.rs".to_owned())),
        "{files:?}"
    );
    let lines = s.sections.iter().find(|x| x.kind == Kind::Text).unwrap();
    assert!(
        lines.items.iter().any(
            |i| matches!(i, Item::Line(l) if l.path == "src/main.rs" && l.text.contains("fn main"))
        ),
        "{lines:?}"
    );
    // The search answered for this query, not a stale one.
    root.read_with(vcx, |r, cx| {
        let p = r.palette().unwrap().read(cx);
        assert!(matches!(p.files(), Found::Done(_)));
    });
}

#[gpui::test]
fn escape_closes_and_returns_to_the_composer(cx: &mut TestAppContext) {
    let store = held(cx);
    let (root, vcx) = opened(cx, &store);
    vcx.simulate_keystrokes("cmd-k");
    draw(vcx);
    assert!(root.read_with(vcx, |r, _| r.palette_open()));
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(!root.read_with(vcx, |r, _| r.palette_open()));
}

#[gpui::test]
fn arrows_move_the_selection_and_wrap(cx: &mut TestAppContext) {
    let store = held(cx);
    let (root, vcx) = opened(cx, &store);
    vcx.simulate_keystrokes("cmd-k down");
    draw(vcx);
    assert_eq!(
        thread_title(&selected(vcx, &root).unwrap()),
        "check upstream advisories"
    );
    vcx.simulate_keystrokes("up up");
    draw(vcx);
    match selected(vcx, &root).unwrap() {
        Item::Command(_) => {}
        other => panic!("wrapped to the last row, a command: {other:?}"),
    }
}

#[gpui::test]
fn control_j_and_k_move_down_and_up_like_the_arrows(cx: &mut TestAppContext) {
    let store = held(cx);
    let (root, vcx) = opened(cx, &store);
    vcx.simulate_keystrokes("cmd-k ctrl-j");
    draw(vcx);
    assert_eq!(
        thread_title(&selected(vcx, &root).unwrap()),
        "check upstream advisories"
    );
    vcx.simulate_keystrokes("ctrl-k");
    draw(vcx);
    assert_eq!(
        thread_title(&selected(vcx, &root).unwrap()),
        "pick the retry policy"
    );
    vcx.simulate_keystrokes("ctrl-k");
    draw(vcx);
    assert!(
        matches!(selected(vcx, &root), Some(Item::Command(_))),
        "wraps to the last row"
    );
    let query = root.read_with(vcx, |r, cx| r.palette().unwrap().read(cx).sheet());
    assert_eq!(query.tab, Tab::All, "nothing was typed");
}
