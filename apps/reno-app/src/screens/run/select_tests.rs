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
use crate::ui::theme::Appearance;
use crate::vm::capabilities::Capabilities;
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

/// "Render check" opened in a window that is drawn.
fn open(cx: &mut TestAppContext) -> Opened<'_> {
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
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    root.update_in(vcx, |r, window, cx| {
        r.open_run(run_id(RENDER_RUN), window, cx);
    });
    let screen = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("a run screen");
    // The thread follows its tail: take the view to the prompt.
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
    let (_store, _root, vcx, screen) = open(cx);
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
    let (_store, _root, vcx, screen) = open(cx);
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
    let (_store, _root, vcx, screen) = open(cx);
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
    let (_store, _root, vcx, screen) = open(cx);
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
    let (_store, _root, vcx, screen) = open(cx);
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
    let (store, _root, vcx, screen) = open(cx);
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
    let (_store, _root, vcx, screen) = open(cx);
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
        .read_with(vcx, |s, _| s.find_marks(key))
        .expect("the reply holds the word");
    assert_eq!(marks.matcher.matches("a negotiation"), vec![2..13]);
    assert!(marks.current.is_some(), "the bar is on a hit in this reply");
    // And a row with no hit gets none.
    let user = screen.read_with(vcx, |s, _| s.rows()[0].key());
    assert!(screen.read_with(vcx, |s, _| s.find_marks(user)).is_none());
}

#[gpui::test]
fn a_file_link_in_a_reply_opens_the_files_tab_at_its_line(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
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

/// A selection is painted: the entries it covers carry a highlight over the selected words (that
/// it is there, not what colour it is).
#[gpui::test]
fn a_selection_is_highlighted_over_the_words_it_covers(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    let theme = crate::ui::theme::Theme::for_appearance(Appearance::Dark);
    let prompt = screen.read_with(vcx, |s, _| {
        s.rows()
            .iter()
            .find(|r| matches!(r, Row::User { .. }))
            .map(Row::key)
            .unwrap()
    });
    let highlighted = |vcx: &VisualTestContext| {
        screen.read_with(vcx, |s, _| {
            let spot = |key| super::select::Spot {
                key,
                slot: Slot::WHOLE,
                base: 0,
                len: PROMPT.len(),
                total: Some(PROMPT.len()),
            };
            s.piece_layers(&theme, spot(prompt), PROMPT, &[])
                .iter()
                .any(|(range, style)| !range.is_empty() && style.background_color.is_some())
        })
    };
    assert!(!highlighted(vcx), "nothing selected, nothing painted");
    let (from, to) = centres(vcx, &screen);
    drag(vcx, from, to);
    assert!(highlighted(vcx), "the selected words carry a highlight");
}

/// `n` presses in a row at `at`, each a click on its own (down, then up), as the platform counts them.
fn press(vcx: &mut VisualTestContext, at: Point<gpui::Pixels>, n: usize, modifiers: Modifiers) {
    for count in 1..=n {
        vcx.simulate_event(gpui::MouseDownEvent {
            position: at,
            modifiers,
            button: MouseButton::Left,
            click_count: count,
            first_mouse: false,
        });
        vcx.simulate_event(gpui::MouseUpEvent {
            position: at,
            modifiers,
            button: MouseButton::Left,
            click_count: count,
        });
    }
    draw(vcx);
}

/// The place in the middle of the word `needle` starts (four characters in), as painted.
fn on_text(
    vcx: &VisualTestContext,
    screen: &Entity<RunScreen>,
    needle: &str,
) -> Point<gpui::Pixels> {
    screen
        .read_with(vcx, |s, _| s.painted_point(needle, 4))
        .unwrap_or_else(|| panic!("{needle:?} is painted"))
}

fn copy_with_the_keyboard(vcx: &mut VisualTestContext) -> String {
    vcx.write_to_clipboard(gpui::ClipboardItem::new_string("untouched".into()));
    vcx.simulate_keystrokes("cmd-c");
    clipboard(vcx).expect("a clipboard")
}

#[gpui::test]
fn double_triple_and_quadruple_click_select_a_word_a_line_and_a_block(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    let in_prompt = on_text(vcx, &screen, "transcript has");
    let in_reply = on_text(vcx, &screen, "transcript draws");

    press(vcx, in_prompt, 2, Modifiers::none());
    assert_eq!(
        copy_with_the_keyboard(vcx),
        "transcript",
        "a word, not its neighbours"
    );

    // The prompt wraps in the test window: a line is the row the click is on, not the message.
    press(vcx, in_prompt, 3, Modifiers::none());
    assert_eq!(
        copy_with_the_keyboard(vcx),
        "Show every kind of content the transcript has to",
        "the row the click is on"
    );
    press(vcx, on_text(vcx, &screen, "draw."), 3, Modifiers::none());
    assert_eq!(
        copy_with_the_keyboard(vcx),
        "draw.",
        "the row it wrapped onto"
    );

    press(vcx, in_prompt, 4, Modifiers::none());
    assert_eq!(copy_with_the_keyboard(vcx), PROMPT, "the whole message");

    // In a reply a block is the paragraph, not every paragraph of the reply.
    press(vcx, in_reply, 2, Modifiers::none());
    assert_eq!(copy_with_the_keyboard(vcx), "transcript");
    press(vcx, in_reply, 4, Modifiers::none());
    assert_eq!(copy_with_the_keyboard(vcx), FIRST_PARAGRAPH);
}

#[gpui::test]
fn shift_click_extends_the_selection(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    let in_prompt = on_text(vcx, &screen, "transcript has");
    let in_reply = on_text(vcx, &screen, "transcript draws");

    // With nothing selected a shift-click selects nothing.
    press(vcx, in_reply, 1, Modifiers::shift());
    assert!(!screen.read_with(vcx, |s, _| s.has_selection()));

    press(vcx, in_prompt, 2, Modifiers::none());
    press(vcx, in_reply, 1, Modifiers::shift());
    let copied = copy_with_the_keyboard(vcx);
    let (head, tail) = copied.split_once("\n\n").expect("two entries");
    assert_eq!(head, "transcript has to draw.", "the word stays selected");
    assert_eq!(
        tail, "Here is every kind of content the tran",
        "up to the click: {copied:?}"
    );

    // A shift-click before the selection extends it backwards, to the click.
    let before = on_text(vcx, &screen, "every kind");
    press(vcx, before, 1, Modifiers::shift());
    let copied = copy_with_the_keyboard(vcx);
    let head = copied.split_once("\n\n").expect("two entries").0;
    let from = PROMPT.find("every kind").unwrap() + 4;
    assert_eq!(head, &PROMPT[from..], "{copied:?}");
}

#[gpui::test]
fn copy_as_markdown_gives_the_source_with_its_math_delimiters(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    let reply = screen.read_with(vcx, |s, _| {
        s.rows()
            .iter()
            .find(|r| matches!(r, Row::Prose { .. }))
            .map(Row::key)
            .unwrap()
    });
    screen.update(vcx, |s, _| s.select_source(reply, 0..RENDER_REPLY.len()));
    let seen = copy_with_the_keyboard(vcx);
    assert!(
        !seen.contains("[supervisor]("),
        "what is seen has no link markup"
    );
    vcx.write_to_clipboard(gpui::ClipboardItem::new_string("untouched".into()));
    screen.update(vcx, |s, cx| s.copy_markdown(cx));
    assert_eq!(clipboard(vcx).as_deref(), Some(RENDER_REPLY));
    let source = clipboard(vcx).unwrap();
    assert!(
        source.contains("$$\\int_0^1 x^2\\,dx = \\tfrac13$$"),
        "{source}"
    );
    assert!(source.contains("area under $x^2$ on"), "{source}");
    assert!(
        source.contains("[supervisor](src/supervisor.rs:42)"),
        "{source}"
    );
}

#[gpui::test]
fn triple_click_in_a_reply_copies_the_closing_marker_of_the_paragraph_as_markdown(
    cx: &mut TestAppContext,
) {
    let (_store, _root, vcx, screen) = open(cx);
    // The last paragraph before the note ends in inline code.
    for _ in 0..4 {
        let y = screen
            .read_with(vcx, |s, _| s.painted_point("supervisor.rs:42", 4))
            .map_or(px(0.0), |p| p.y);
        screen.update(vcx, |s, cx| {
            s.list.scroll_by(y - px(400.0));
            cx.notify();
        });
        draw(vcx);
    }
    let at = on_text(vcx, &screen, "supervisor.rs:42");
    press(vcx, at, 3, Modifiers::none());
    vcx.write_to_clipboard(gpui::ClipboardItem::new_string("untouched".into()));
    screen.update(vcx, |s, cx| s.copy_markdown(cx));
    let source = clipboard(vcx).unwrap();
    assert!(
        source.ends_with("at `src/supervisor.rs:42`"),
        "the closing backtick comes with the row: {source:?}"
    );
}

fn right_click(vcx: &mut VisualTestContext, at: Point<gpui::Pixels>) {
    vcx.simulate_mouse_down(at, MouseButton::Right, Modifiers::none());
    vcx.simulate_mouse_up(at, MouseButton::Right, Modifiers::none());
    draw(vcx);
}

fn choose_menu_row(vcx: &mut VisualTestContext, row: &str) {
    crate::tests::click(vcx, row);
    draw(vcx);
    assert!(!painted(vcx, "transcript-menu"));
}

#[gpui::test]
fn right_click_offers_copy_message_on_a_reply_and_copy_on_a_selection(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = open(cx);
    let (prompt, reply) = centres(vcx, &screen);

    right_click(vcx, prompt);
    assert!(painted(vcx, "transcript-menu"));
    for row in [
        "transcript-menu-open",
        "transcript-menu-copy-link",
        "transcript-menu-copy",
        "transcript-menu-copy-as-markdown",
        "transcript-menu-copy-message",
    ] {
        assert!(!painted(vcx, row), "{row}");
    }
    assert!(painted(vcx, "transcript-menu-scroll-to-bottom"));

    right_click(vcx, reply);
    assert!(painted(vcx, "transcript-menu-copy-message"));
    choose_menu_row(vcx, "transcript-menu-copy-message");
    assert_eq!(clipboard(vcx).as_deref(), Some(RENDER_REPLY));

    let at = on_text(vcx, &screen, FIRST_PARAGRAPH);
    press(vcx, at, 3, Modifiers::none());
    right_click(vcx, reply);
    choose_menu_row(vcx, "transcript-menu-copy");
    assert_eq!(clipboard(vcx).as_deref(), Some(FIRST_PARAGRAPH));

    right_click(vcx, reply);
    choose_menu_row(vcx, "transcript-menu-copy-as-markdown");
    assert_eq!(clipboard(vcx).as_deref(), Some(FIRST_PARAGRAPH));

    vcx.write_to_clipboard(gpui::ClipboardItem::new_string("untouched".into()));
    vcx.simulate_keystrokes("shift-f10");
    draw(vcx);
    assert!(painted(vcx, "transcript-menu-copy-as-markdown"));
    vcx.simulate_keystrokes("down enter");
    draw(vcx);
    assert!(!painted(vcx, "transcript-menu"));
    assert_eq!(clipboard(vcx).as_deref(), Some(FIRST_PARAGRAPH));
}

fn append_reply(
    vcx: &mut VisualTestContext,
    store: &Entity<Store>,
    screen: &Entity<RunScreen>,
    reply: &str,
) {
    use reno_proto::{ExecEvent, Seq, SessionRef, StopReason};

    let run = run_id(RENDER_RUN);
    wait_for(vcx, "the seeded reply is complete", |vcx| {
        screen.read_with(vcx, |s, _| {
            s.rows().iter().any(|row| matches!(row, Row::Done { .. }))
        })
    });
    for event in [
        ExecEvent::Resumed {
            from_attempt: 0,
            session: SessionRef("menu".into()),
        },
        ExecEvent::Text { text: reply.into() },
        ExecEvent::Done {
            stop: StopReason::Completed,
            result: None,
            diagnostic: None,
            reason: None,
        },
    ] {
        store.update(vcx, |s, cx| {
            let seq = s
                .state()
                .transcript(&run)
                .and_then(|t| t.highest)
                .and_then(Seq::next)
                .unwrap_or(Seq(1));
            s.apply(
                reno_client::Update::RunEvent {
                    run: run.clone(),
                    seq,
                    event,
                },
                cx,
            );
        });
    }
    screen.update(vcx, |s, cx| s.jump_to_latest(cx));
    draw(vcx);
}

#[gpui::test]
fn the_transcript_menu_copies_link_destinations_and_opens_them_through_their_handlers(
    cx: &mut TestAppContext,
) {
    const WEB: &str = "https://example.com/docs?q=1";
    const FILE: &str = "./src/supervisor.rs#L42";
    const INLINE: &str = "src/supervisor.rs:42";
    const IMAGE: &str = "https://example.com/badge.png";
    const REPLY: &str = "See [the guide](https://example.com/docs?q=1), [source file](./src/supervisor.rs#L42), and `src/supervisor.rs:42`.

Write to [the mailbox](mailto:team@example.com).

![remote badge](https://example.com/badge.png)";
    let recording = std::rc::Rc::new(crate::links::fake::Recording::default());
    cx.update(|cx| crate::links::install(cx, recording.clone()));
    let (store, _root, vcx, screen) = open(cx);
    append_reply(vcx, &store, &screen, REPLY);
    wait_for(vcx, "the worktree file resolved", |vcx| {
        screen.read_with(vcx, |s, cx| s.file_resolves("src/supervisor.rs", cx))
    });

    let web = on_text(vcx, &screen, "the guide");
    let file = on_text(vcx, &screen, "source file");
    let inline = on_text(vcx, &screen, INLINE);
    let image = vcx.debug_bounds("md-image-card").unwrap().center();
    for (at, url) in [(web, WEB), (file, FILE), (inline, INLINE), (image, IMAGE)] {
        right_click(vcx, at);
        assert!(painted(vcx, "transcript-menu-open"), "{url}");
        assert!(painted(vcx, "transcript-menu-copy-message"), "{url}");
        choose_menu_row(vcx, "transcript-menu-copy-link");
        assert_eq!(clipboard(vcx).as_deref(), Some(url));
    }
    for text in ["the mailbox", "Write to"] {
        let at = on_text(vcx, &screen, text);
        right_click(vcx, at);
        assert!(painted(vcx, "transcript-menu-copy-message"));
        assert!(!painted(vcx, "transcript-menu-open"));
        assert!(!painted(vcx, "transcript-menu-copy-link"));
        choose_menu_row(vcx, "transcript-menu-copy-message");
    }

    assert!(
        recording.opened.borrow().is_empty(),
        "right-clicking and copying do not activate a link"
    );
    right_click(vcx, web);
    choose_menu_row(vcx, "transcript-menu-open");
    assert_eq!(recording.opened.borrow().as_slice(), [WEB]);

    right_click(vcx, file);
    choose_menu_row(vcx, "transcript-menu-open");
    let (active, opened) = screen.read_with(vcx, |s, _| {
        (
            s.panel_state().active().clone(),
            s.files_model()
                .and_then(|model| model.doc().map(|doc| (doc.path.clone(), doc.line))),
        )
    });
    assert_eq!(active, crate::panel::surface::Surface::Files);
    assert_eq!(opened, Some(("src/supervisor.rs".to_owned(), Some(42))));
}

#[gpui::test]
fn the_transcript_menu_copies_a_selection_across_a_hoisted_footnote_in_reading_order(
    cx: &mut TestAppContext,
) {
    const REPLY: &str = "Read this[^note].

[^note]: Café from the real note.

Skip this line.

**Only the painted tail.**";
    let (store, _root, vcx, screen) = open(cx);
    append_reply(vcx, &store, &screen, REPLY);
    let (tail, note) = screen.read_with(vcx, |s, _| {
        (
            s.painted_point("the painted tail.", 0).unwrap(),
            s.painted_point("Café from", "Café".len()).unwrap(),
        )
    });
    vcx.simulate_mouse_down(tail, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_move(note, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_up(note, MouseButton::Left, Modifiers::none());
    draw(vcx);

    for (row, expected) in [
        (
            "transcript-menu-copy",
            "the painted tail.

Café",
        ),
        (
            "transcript-menu-copy-as-markdown",
            "the painted tail.**

[^note]: Café",
        ),
        ("transcript-menu-copy-message", REPLY),
    ] {
        right_click(vcx, note);
        choose_menu_row(vcx, row);
        assert_eq!(clipboard(vcx).as_deref(), Some(expected));
    }
}

#[gpui::test]
fn shift_f10_opens_the_transcript_menu_and_arrows_and_enter_pick(cx: &mut TestAppContext) {
    use crate::panel::persist::{PanelStore, path_in};
    use crate::panel::saved::Saved;

    let tmp = tempfile::tempdir().unwrap();
    let path = path_in(tmp.path());
    cx.update(|cx| Saved::install(path.clone(), cx));
    let (_store, _root, vcx, screen) = open(cx);
    let key = screen.read_with(vcx, |s, _| s.panel_key().clone());
    let persisted = |vcx: &mut VisualTestContext| {
        vcx.update(|_, cx| Saved::flush(cx));
        PanelStore::load(&path)
            .0
            .get(&key)
            .and_then(|panel| panel.place)
    };

    vcx.simulate_keystrokes("shift-f10");
    draw(vcx);
    assert!(
        !painted(vcx, "transcript-menu"),
        "the composer owns the keyboard"
    );
    for _ in 0..12 {
        vcx.simulate_keystrokes("shift-tab");
        draw(vcx);
        vcx.simulate_keystrokes("shift-f10");
        draw(vcx);
        if painted(vcx, "transcript-menu") {
            break;
        }
    }
    assert!(
        painted(vcx, "transcript-menu"),
        "reachable without a pointer or selection"
    );

    let before = screen.read_with(vcx, |s, _| s.scroll_offset());
    vcx.simulate_keystrokes("down up escape");
    draw(vcx);
    assert!(!painted(vcx, "transcript-menu"));
    assert_eq!(screen.read_with(vcx, |s, _| s.scroll_offset()), before);

    // Escape restores transcript focus, so Down scrolls again after the menu closes.
    vcx.simulate_keystrokes("down");
    draw(vcx);
    assert_ne!(screen.read_with(vcx, |s, _| s.scroll_offset()), before);
    vcx.simulate_keystrokes("shift-f10");
    draw(vcx);
    assert!(painted(vcx, "transcript-menu-scroll-to-top"));
    assert!(painted(vcx, "transcript-menu-scroll-to-bottom"));
    vcx.simulate_keystrokes("down enter");
    draw(vcx);
    assert!(!painted(vcx, "transcript-menu"));
    assert!(screen.read_with(vcx, |s, _| s.is_following_tail()));
    assert_eq!(persisted(vcx), None);

    vcx.simulate_keystrokes("shift-f10");
    draw(vcx);
    assert!(painted(vcx, "transcript-menu-scroll-to-top"));
    assert!(!painted(vcx, "transcript-menu-scroll-to-bottom"));
    vcx.simulate_keystrokes("up enter");
    draw(vcx);
    assert!(!painted(vcx, "transcript-menu"));
    assert_eq!(screen.read_with(vcx, |s, _| s.scroll_offset()), before);
    let place = screen.read_with(vcx, |s, _| s.saved_place());
    assert!(place.is_some());
    assert_eq!(persisted(vcx), place);
}

#[gpui::test]
fn tab_navigation_between_the_composer_and_transcript_stays_within_the_run(
    cx: &mut TestAppContext,
) {
    let (_store, _root, vcx, screen) = open(cx);
    let transcript_focused = |vcx: &mut VisualTestContext| {
        vcx.update(|window, cx| screen.read(cx).transcript_focus.is_focused(window))
    };
    let composer_focused = |vcx: &mut VisualTestContext| {
        vcx.update(|window, cx| screen.read(cx).focus_handle(cx).is_focused(window))
    };
    assert!(composer_focused(vcx));
    for _ in 0..2 {
        vcx.simulate_keystrokes("shift-tab");
        draw(vcx);
        assert!(transcript_focused(vcx));
        for _ in 0..12 {
            vcx.simulate_keystrokes("shift-tab");
            draw(vcx);
        }
        assert!(transcript_focused(vcx));
        vcx.simulate_keystrokes("tab");
        draw(vcx);
        assert!(composer_focused(vcx));
        for _ in 0..12 {
            vcx.simulate_keystrokes("tab");
            draw(vcx);
        }
        assert!(composer_focused(vcx));
    }

    let composer = screen.read_with(vcx, |s, _| s.steer_input().clone());
    vcx.simulate_keystrokes("cmd-a");
    vcx.simulate_input("@src/superv");
    wait_for(vcx, "the composer suggests the worktree file", |vcx| {
        composer.read_with(vcx, |c, _| {
            c.completion_labels()
                .is_some_and(|labels| labels == ["src/supervisor.rs"])
        })
    });
    vcx.simulate_keystrokes("tab");
    draw(vcx);
    assert!(composer_focused(vcx));
    assert_eq!(
        composer.read_with(vcx, |c, _| c.text().to_owned()),
        "@src/supervisor.rs "
    );
}
