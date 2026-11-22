//! ROAD-33: selecting across messages and copying it, agent-posted visuals as list items, prose
//! matches painted, and a file link opening the Files tab, on the seeded "Render check" thread
//! of the fake daemon.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use gpui::{
    AppContext as _, Entity, Modifiers, MouseButton, Point, TestAppContext, VisualTestContext,
    point, px,
};
use reno_client::fake::FakeConfig;
use reno_client::fake::transcript::{RENDER_REPLY, RENDER_RUN};

use super::RunScreen;
use crate::app::RootView;
use crate::store::Store;
use crate::tests::{draw, one_window_test_at_a_time, painted, run_id, wait_until, window};
use crate::transcript::Row;
use crate::ui::draft33;
use crate::ui::theme::Appearance;
use crate::vm::capabilities::Capabilities;
use crate::vm::draft33::Variant;
use crate::vm::selection::Slot;

const PROMPT: &str = "Show every kind of content the transcript has to draw.";
const FIRST_PARAGRAPH: &str = "Here is every kind of content the transcript draws.";

fn wait_for(
    vcx: &mut VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    vcx.executor().allow_parking();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        draw(vcx);
        if done(vcx) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );

        std::thread::sleep(Duration::from_millis(10));
    }
}

type Opened<'a> = (
    Entity<Store>,
    Entity<RootView>,
    &'a mut VisualTestContext,
    Entity<RunScreen>,
);

/// "Render check" opened in a window that is drawn, with the draft's variant on when asked.
fn open(cx: &mut TestAppContext, on: Option<Variant>) -> Opened<'_> {
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
        store.read_with(cx, |s, _| {
            s.state().run(&run_id(RENDER_RUN)).is_some() && s.client().is_some()
        })
    });
    cx.update(|cx| {
        crate::actions::bind_keys(cx);
        crate::ui::composer::bind_keys(cx);
        super::bind_keys(cx);
        if let Some(variant) = on {
            draft33::install(variant, cx);
        }
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    root.update_in(vcx, |r, window, cx| {
        r.open_run(run_id(RENDER_RUN), window, cx);
    });
    let screen = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("a run screen");
    // The thread follows its tail: take the view to the prompt, where the walk starts.
    wait_for(vcx, "the rows are in", |vcx| {
        screen.read_with(vcx, |s, _| s.items().len() >= 5)
    });
    screen.update(vcx, |s, cx| {
        s.list.pause_following_tail();
        s.scroll_to_row(0, cx);
    });
    wait_for(vcx, "the prompt and the reply are drawn", |vcx| {
        screen.read_with(vcx, |s, _| {
            let pieces = s.painted_pieces();
            let has = |want: fn(&Row) -> bool| {
                pieces
                    .iter()
                    .any(|(key, _, _)| row_of(s, *key).is_some_and(want))
            };
            has(|r| matches!(r, Row::User { .. })) && has(|r| matches!(r, Row::Prose { .. }))
        })
    });
    (store, root, vcx, screen)
}

fn row_of(screen: &RunScreen, key: crate::transcript::Key) -> Option<&Row> {
    screen.rows().iter().find(|r| r.key() == key)
}

/// The centre of the first painted text of the prompt and of the reply.
fn centres(
    vcx: &VisualTestContext,
    screen: &Entity<RunScreen>,
) -> (Point<gpui::Pixels>, Point<gpui::Pixels>) {
    screen.read_with(vcx, |s, _| {
        let mut user = None;
        let mut prose: Option<gpui::Bounds<gpui::Pixels>> = None;
        for (key, slot, bounds) in s.painted_pieces() {
            match row_of(s, key) {
                Some(Row::User { .. }) if slot == Slot::WHOLE => user = Some(bounds),
                Some(Row::Prose { .. }) if prose.is_none_or(|p| bounds.top() < p.top()) => {
                    prose = Some(bounds);
                }
                _ => {}
            }
        }
        // A third of the way along the first line of each.
        let on_first_line = |b: gpui::Bounds<gpui::Pixels>| {
            point(b.left() + b.size.width / 3.0, b.top() + px(11.0))
        };
        (
            on_first_line(user.expect("the prompt is painted")),
            on_first_line(prose.expect("the reply is painted")),
        )
    })
}

fn drag(vcx: &mut VisualTestContext, from: Point<gpui::Pixels>, to: Point<gpui::Pixels>) {
    vcx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_move(
        point(from.x + px(8.0), from.y + px(8.0)),
        MouseButton::Left,
        Modifiers::none(),
    );
    draw(vcx);
    vcx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    draw(vcx);
}

fn clipboard(vcx: &VisualTestContext) -> Option<String> {
    vcx.read_from_clipboard().and_then(|c| c.text())
}

#[gpui::test]
fn a_drag_from_the_prompt_to_the_reply_and_cmd_c_copies_both_in_order(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx, Some(Variant::A));
    let (from, to) = centres(vcx, &screen);
    drag(vcx, from, to);
    assert!(screen.read_with(vcx, |s, _| s.has_selection()));
    vcx.write_to_clipboard(gpui::ClipboardItem::new_string("untouched".into()));
    vcx.simulate_keystrokes("cmd-c");
    let copied = clipboard(vcx).expect("something was copied");
    // From the middle of the prompt to the middle of the first paragraph, a blank line between.
    let (head, tail) = copied.split_once("\n\n").expect("two entries");
    assert!(
        PROMPT.ends_with(head) && head.len() > 8 && head.len() < PROMPT.len(),
        "{copied:?}"
    );
    assert!(
        FIRST_PARAGRAPH.starts_with(tail) && tail.len() > 8,
        "{copied:?}"
    );
    // No control text: the quiet Copy, the code block's buttons and "Rendering" never go along.
    assert!(
        !copied.contains("Copy") && !copied.contains("Thinking"),
        "{copied:?}"
    );
}

#[gpui::test]
fn a_selection_is_anchored_to_entry_keys_with_source_byte_ranges(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx, Some(Variant::B));
    let (from, to) = centres(vcx, &screen);
    drag(vcx, from, to);
    let anchors = screen.read_with(vcx, |s, _| s.selection_anchors());
    let keys: Vec<_> = screen.read_with(vcx, |s, _| s.rows().iter().map(Row::key).collect());
    assert_eq!(anchors.len(), 2, "{anchors:?}");
    assert_eq!(anchors[0].key, keys[0]);
    let prompt = anchors[0].range.clone().expect("the prompt has a source");
    assert!(prompt.start > 0 && prompt.end == PROMPT.len(), "{prompt:?}");
    let reply = anchors[1].range.clone().expect("the reply has a source");
    assert_eq!(reply.start, 0);
    assert!(RENDER_REPLY.is_char_boundary(reply.end) && reply.end <= FIRST_PARAGRAPH.len());
}

#[gpui::test]
fn a_click_that_does_not_drag_clears_the_selection_and_one_on_it_keeps_it(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx, Some(Variant::A));
    let (from, to) = centres(vcx, &screen);
    drag(vcx, from, to);
    assert!(screen.read_with(vcx, |s, _| s.has_selection()));
    // A click on the selected text keeps it.
    vcx.simulate_click(to, Modifiers::none());
    draw(vcx);
    assert!(screen.read_with(vcx, |s, _| s.has_selection()));
    // A click elsewhere in the transcript, past the selection's end, clears it.
    let below = screen.read_with(vcx, |s, _| {
        s.painted_pieces()
            .into_iter()
            .filter(|(k, _, b)| {
                matches!(row_of(s, *k), Some(Row::Prose { .. })) && b.bottom() < px(800.0)
            })
            .map(|(_, _, b)| b)
            .max_by(|a, b| a.top().partial_cmp(&b.top()).unwrap())
            .expect("later text in view")
            .center()
    });
    vcx.simulate_click(below, Modifiers::none());
    draw(vcx);
    assert!(!screen.read_with(vcx, |s, _| s.has_selection()));
}

#[gpui::test]
fn cmd_c_in_the_composer_still_copies_the_composers_text(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx, Some(Variant::A));
    let (from, to) = centres(vcx, &screen);
    drag(vcx, from, to);
    // The owner goes back to the composer, types and selects it: ⌘C is the composer's.
    let steer = screen.read_with(vcx, |s, _| s.steer_input().clone());
    vcx.update(|window, cx| {
        steer.update(cx, |c, cx| c.fill("a draft reply", window, cx));
        window.focus(&steer.read(cx).focus_handle_ref().clone(), cx);
    });
    draw(vcx);
    vcx.simulate_keystrokes("cmd-a");
    vcx.write_to_clipboard(gpui::ClipboardItem::new_string("untouched".into()));
    vcx.simulate_keystrokes("cmd-c");
    assert_eq!(clipboard(vcx).as_deref(), Some("a draft reply"));
}

#[gpui::test]
fn a_selection_holds_while_its_start_scrolls_out_of_view_and_the_drag_scrolls_the_list(
    cx: &mut TestAppContext,
) {
    let (_store, _root, vcx, screen) = open(cx, Some(Variant::A));
    let (from, _) = centres(vcx, &screen);
    // Hold the pointer below the list: it scrolls on each tick, and the selection stays.
    vcx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_move(
        point(from.x, from.y + px(40.0)),
        MouseButton::Left,
        Modifiers::none(),
    );
    draw(vcx);
    let below = point(from.x, px(2000.0));
    vcx.simulate_mouse_move(below, MouseButton::Left, Modifiers::none());
    let before = screen.read_with(vcx, |s, _| s.scroll_offset());
    for _ in 0..6 {
        vcx.update(|window, cx| {
            screen.update(cx, |s, cx| s.scroll_tick(window, cx));
        });
        draw(vcx);
    }
    let after = screen.read_with(vcx, |s, _| s.scroll_offset());
    assert!(
        after != before,
        "the list scrolled while the pointer was past its edge"
    );
    vcx.simulate_mouse_up(below, MouseButton::Left, Modifiers::none());
    assert!(screen.read_with(vcx, |s, _| s.has_selection()));
    let copied = screen
        .read_with(vcx, |s, _| s.selected_text())
        .expect("text");
    assert!(copied.starts_with("draw") || PROMPT.contains(copied.split("\n\n").next().unwrap()));
}

#[gpui::test]
fn agent_posted_visuals_are_items_after_their_entry_with_the_fake_and_none_with_the_real_daemon(
    cx: &mut TestAppContext,
) {
    let (store, _root, vcx, screen) = open(cx, Some(Variant::A));
    let visuals = |vcx: &VisualTestContext| {
        screen.read_with(vcx, |s, _| {
            let items = s.items();
            let at: Vec<usize> = (0..items.len())
                .filter(|&i| items.items[i].visual.is_some())
                .collect();
            let reply = s
                .rows()
                .iter()
                .position(|r| matches!(r, Row::Prose { .. }))
                .map(|row| items.item_of_top(row));
            (at, reply)
        })
    };
    let (at, reply) = visuals(vcx);
    assert_eq!(at.len(), 2, "both posted visuals are items");
    assert_eq!(
        at[0],
        reply.unwrap() + 1,
        "right after the reply they were posted with"
    );
    // They sit under the reply: bring them into view.
    screen.update(vcx, |s, cx| {
        let first = s
            .items()
            .items
            .iter()
            .position(|i| i.visual.is_some())
            .unwrap();
        s.list.scroll_to(gpui::ListOffset {
            item_ix: first,
            offset_in_item: px(0.0),
        });
        cx.notify();
    });
    wait_for(vcx, "the visuals draw", |vcx| painted(vcx, "visual"));
    store.update(vcx, |s, cx| s.set_capabilities(Capabilities::REAL, cx));
    draw(vcx);
    assert!(
        visuals(vcx).0.is_empty(),
        "no visual items and no placeholder"
    );
    assert!(!painted(vcx, "visual"));
}

#[gpui::test]
fn the_find_bar_hands_a_reply_its_needle_and_which_of_its_hits_is_current(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx, Some(Variant::C));
    vcx.simulate_keystrokes("cmd-f");
    draw(vcx);
    vcx.simulate_keystrokes("n e g o t i a t i o n");
    draw(vcx);
    vcx.simulate_keystrokes("enter");
    draw(vcx);
    let key = screen.read_with(vcx, |s, _| {
        s.rows()
            .iter()
            .find(|r| matches!(r, Row::Prose { .. }))
            .map(Row::key)
            .unwrap()
    });
    let marks = screen
        .read_with(vcx, |s, _| s.find_marks(key, Variant::C))
        .expect("the reply holds the word");
    assert_eq!(marks.needle, "negotiation");
    assert!(marks.current.is_some(), "the bar is on a hit in this reply");
    // And a row with no hit gets none.
    let user = screen.read_with(vcx, |s, _| s.rows()[0].key());
    assert!(
        screen
            .read_with(vcx, |s, _| s.find_marks(user, Variant::C))
            .is_none()
    );
}

#[gpui::test]
fn a_file_link_in_a_reply_opens_the_files_tab_at_its_line(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx, Some(Variant::A));
    // The reply names `src/supervisor.rs:42`: once the worktree answers, it is a link.
    wait_for(vcx, "the worktree answered for src", |vcx| {
        screen.read_with(vcx, |s, cx| s.file_resolves("src/supervisor.rs", cx))
    });
    assert!(!screen.read_with(vcx, |s, cx| s.file_resolves("src/missing.rs", cx)));
    let links = screen.update(vcx, |s, cx| s.file_links(cx));
    vcx.update(|window, cx| {
        links.open(
            crate::ui::markdown_view::link::FileTarget {
                path: "src/supervisor.rs".to_owned(),
                line: Some(42),
            },
            window,
            cx,
        );
    });
    draw(vcx);
    let (active, doc) = screen.read_with(vcx, |s, _| {
        (
            s.panel_state().active().clone(),
            s.files_model()
                .and_then(|m| m.doc().map(|d| (d.path.clone(), d.line))),
        )
    });
    assert_eq!(active, crate::panel::surface::Surface::Files);
    assert_eq!(doc, Some(("src/supervisor.rs".to_owned(), Some(42))));
}

#[gpui::test]
fn a_selection_survives_switching_the_draft_variant(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx, Some(Variant::A));
    let (from, to) = centres(vcx, &screen);
    drag(vcx, from, to);
    let copied = screen
        .read_with(vcx, |s, _| s.selected_text())
        .expect("selected");
    for _ in 0..3 {
        vcx.simulate_keystrokes("cmd-alt-right");
        draw(vcx);
        assert!(screen.read_with(vcx, |s, _| s.has_selection()));
        assert_eq!(
            screen.read_with(vcx, |s, _| s.selected_text()).as_deref(),
            Some(copied.as_str())
        );
    }
}
