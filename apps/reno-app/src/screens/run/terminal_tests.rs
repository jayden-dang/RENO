//! The Terminal tab against the fake daemon's echo terminals: opening from Home, typing, the
//! debounced resize, reconnecting, paste, copy, the banners of D7, the errors, closing a shell
//! with a program in it, and what is saved and restored. No real shell, no PTY, no `claude`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    ClipboardItem, Entity, Modifiers, MouseButton, ScrollDelta, TestAppContext, TouchPhase,
    VisualTestContext, point, px, size,
};
use reno_client::fake::FakeDaemon;
use reno_proto::{ErrorCode, ErrorObject, TermId};

use super::RunScreen;
use super::terminal::Terms;
use crate::app::RootView;
use crate::panel::Descriptors;
use crate::panel::surface::{Descriptor, Kind, Surface};
use crate::panel::view::Prefs;
use crate::tests::{click, draw, fake_store, painted, run_id, wait_drawn, wait_until, window};
use crate::ui::terminal_view::{Phase, TerminalView};
use crate::ui::theme::Appearance;

type Fixture<'a> = (
    Entity<RootView>,
    &'a mut VisualTestContext,
    Entity<RunScreen>,
    Arc<FakeDaemon>,
);

/// A wide window with the run `name` open and its panel drawn.
fn open_run<'a>(cx: &'a mut TestAppContext, name: &str) -> Fixture<'a> {
    open_run_in(cx, name, Appearance::Dark)
}

fn open_run_in<'a>(cx: &'a mut TestAppContext, name: &str, appearance: Appearance) -> Fixture<'a> {
    let store = fake_store(cx);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.state().approvals().len() == 2)
    });
    let fake = store.read_with(cx, |s, _| s.fake().cloned()).expect("fake");
    let (root, vcx) = window(cx, &store, appearance);
    vcx.simulate_resize(size(px(1700.0), px(960.0)));
    vcx.update(|_, cx| {
        Prefs {
            open: Some(true),
            width: Some(800.0),
            ..Prefs::default()
        }
        .set_without_saving(cx);
    });
    draw(vcx);
    root.update_in(vcx, |r, window, cx| r.open_run(run_id(name), window, cx));
    draw(vcx);
    let screen = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("a run screen");
    (root, vcx, screen, fake)
}

/// Home, then the letter T.
fn press_t_from_home(vcx: &mut VisualTestContext) {
    click(vcx, "panel-new-tab");
    vcx.simulate_keystrokes("t");
    draw(vcx);
}

fn front(vcx: &VisualTestContext, screen: &Entity<RunScreen>) -> Entity<TerminalView> {
    screen.read_with(vcx, |s, _| {
        s.terminal_in_front().expect("a terminal in front")
    })
}

fn phase(vcx: &VisualTestContext, view: &Entity<TerminalView>) -> Phase {
    view.read_with(vcx, |v, _| v.phase().clone())
}

fn wait_phase(vcx: &mut VisualTestContext, view: &Entity<TerminalView>, want: Phase) {
    let view = view.clone();
    wait_drawn(vcx, move |vcx| phase(vcx, &view) == want);
}

fn row(vcx: &VisualTestContext, view: &Entity<TerminalView>, n: usize) -> String {
    view.read_with(vcx, |v, _| v.emulator().row_text(n))
}

fn screen_text(vcx: &VisualTestContext, view: &Entity<TerminalView>) -> String {
    view.read_with(vcx, |v, _| v.emulator().text(true))
}

fn wait_text(vcx: &mut VisualTestContext, view: &Entity<TerminalView>, needle: &str) {
    let (view, needle) = (view.clone(), needle.to_owned());
    wait_drawn(vcx, move |vcx| screen_text(vcx, &view).contains(&needle));
}

fn writes(vcx: &VisualTestContext, view: &Entity<TerminalView>) -> Vec<Vec<u8>> {
    view.read_with(vcx, |v, _| v.taps.writes.clone())
}

/// Everything the view has sent to the shell so far, in order.
fn sent(vcx: &VisualTestContext, view: &Entity<TerminalView>) -> Vec<u8> {
    writes(vcx, view).concat()
}

fn term_id(vcx: &VisualTestContext, view: &Entity<TerminalView>) -> TermId {
    view.read_with(vcx, |v, _| v.id().cloned().expect("an id"))
}

/// Opens a terminal on `name` from Home and waits until its shell is live and drawn.
fn live_terminal(vcx: &mut VisualTestContext, screen: &Entity<RunScreen>) -> Entity<TerminalView> {
    press_t_from_home(vcx);
    wait_drawn(vcx, |vcx| painted(vcx, "terminal-view"));
    let view = front(vcx, screen);
    wait_phase(vcx, &view, Phase::Live);
    view
}

fn focused(vcx: &mut VisualTestContext, view: &Entity<TerminalView>) -> bool {
    vcx.update(|window, cx| view.read(cx).is_focused(window))
}

fn advance(vcx: &mut VisualTestContext, by: Duration) {
    vcx.executor().advance_clock(by);
    vcx.run_until_parked();
}

#[gpui::test]
fn t_from_home_opens_a_terminal_and_typing_echoes_into_the_grid(cx: &mut TestAppContext) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let view = live_terminal(vcx, &screen);
    assert_eq!(fake.terminals(), 1);
    assert!(painted(vcx, "panel-tab-terminal-1"));
    assert!(
        !painted(vcx, "panel-home"),
        "Home was replaced by what it opened"
    );
    // The first grid is the measured one, not the 80 by 24 a view starts with.
    let geometry = view.read_with(vcx, |v, _| v.geometry().expect("measured"));
    assert_eq!(
        view.read_with(vcx, |v, _| (v.emulator().cols(), v.emulator().rows())),
        (usize::from(geometry.cols), usize::from(geometry.rows))
    );
    assert_ne!((geometry.cols, geometry.rows), (80, 24));

    // The terminal has the keyboard: typed text goes to the shell and the echo comes back.
    assert!(focused(vcx, &view), "the new terminal has the keyboard");
    vcx.simulate_input("ls");
    wait_text(vcx, &view, "ls");
    assert_eq!(row(vcx, &view, 0), "ls");
    // One write at a time: the first key goes at once and later ones ride the next write.
    assert_eq!(sent(vcx, &view), b"ls".to_vec());
    // Named keys are encoded: Enter is a carriage return.
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    assert_eq!(sent(vcx, &view), b"ls\r".to_vec());
}

#[gpui::test]
fn a_second_t_from_home_opens_another_terminal_and_letters_never_reach_home(
    cx: &mut TestAppContext,
) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let first = live_terminal(vcx, &screen);
    // Letters of the Home rows, typed in the shell: they are input, not commands.
    vcx.simulate_input("tfhcsk");
    wait_text(vcx, &first, "tfhcsk");
    assert_eq!(fake.terminals(), 1, "no second terminal from a typed t");
    assert!(matches!(
        screen.read_with(vcx, |s, _| s.panel.active().clone()),
        Surface::Terminal(1)
    ));
    assert!(!painted(vcx, "panel-tab-files"));

    // From Home, a second T is another shell.
    press_t_from_home(vcx);
    wait_drawn(vcx, |vcx| painted(vcx, "panel-tab-terminal-2"));
    assert_eq!(fake.terminals(), 2);
    let second = front(vcx, &screen);
    wait_phase(vcx, &second, Phase::Live);
    assert_ne!(term_id(vcx, &first), term_id(vcx, &second));
    // The new one has the keyboard, and the first one kept what it had.
    vcx.simulate_input("x");
    wait_text(vcx, &second, "x");
    assert!(!screen_text(vcx, &first).ends_with('x'));
    assert!(painted(vcx, "panel-tab-terminal-1"));
}

#[gpui::test]
fn the_panel_shortcuts_still_work_while_a_terminal_has_the_keyboard(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_keys);
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let view = live_terminal(vcx, &screen);
    let active = |vcx: &VisualTestContext| screen.read_with(vcx, |s, _| s.panel.active().clone());
    // Cmd-T is a new panel tab (Home), not a character for the shell.
    vcx.simulate_keystrokes("cmd-t");
    draw(vcx);
    assert_eq!(active(vcx), Surface::Home);
    assert_eq!(fake.terminals(), 1);
    assert!(sent(vcx, &view).is_empty(), "nothing reached the shell");
    // Ctrl-Tab cycles back to the terminal.
    vcx.simulate_keystrokes("ctrl-tab");
    draw(vcx);
    vcx.simulate_keystrokes("ctrl-shift-tab");
    draw(vcx);
    assert!(sent(vcx, &view).is_empty(), "nor did the tab keys");
    // Cmd-W closes the terminal tab and ends its shell.
    wait_drawn(vcx, |vcx| {
        matches!(active(vcx), Surface::Terminal(_)) || {
            vcx.simulate_keystrokes("ctrl-tab");
            false
        }
    });
    vcx.simulate_keystrokes("cmd-w");
    wait_drawn(vcx, |_| fake.terminals() == 0);
    assert!(!painted(vcx, "panel-tab-terminal-1"));
}

#[gpui::test]
fn a_resize_reaches_the_shell_once_after_the_debounce(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1");
    let view = live_terminal(vcx, &screen);
    let before = view.read_with(vcx, |v, _| (v.emulator().cols(), v.emulator().rows()));
    assert!(
        view.read_with(vcx, |v, _| v.taps.resizes.is_empty()),
        "the open used the measured size"
    );

    // A window dragged through three heights inside the debounce is one resize.
    for height in [900.0, 840.0, 780.0] {
        vcx.simulate_resize(size(px(1700.0), px(height)));
        draw(vcx);
    }
    let after = view.read_with(vcx, |v, _| (v.emulator().cols(), v.emulator().rows()));
    assert_ne!(before, after, "the grid follows the window at once");
    assert!(
        view.read_with(vcx, |v, _| v.taps.resizes.is_empty()),
        "but the shell is not told yet"
    );
    advance(vcx, Duration::from_millis(79));
    assert!(view.read_with(vcx, |v, _| v.taps.resizes.is_empty()));
    advance(vcx, Duration::from_millis(2));
    wait_drawn(vcx, |vcx| {
        view.read_with(vcx, |v, _| !v.taps.resizes.is_empty())
    });
    let sent_sizes = view.read_with(vcx, |v, _| v.taps.resizes.clone());
    assert_eq!(sent_sizes.len(), 1);
    assert_eq!(
        (usize::from(sent_sizes[0].0), usize::from(sent_sizes[0].1)),
        after
    );
}

#[gpui::test]
fn a_hidden_terminal_detaches_and_comes_back_from_its_last_seq(cx: &mut TestAppContext) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let view = live_terminal(vcx, &screen);
    let id = term_id(vcx, &view);
    fake.term_output(&id, "one\r\n");
    wait_text(vcx, &view, "one");
    assert!(view.read_with(vcx, |v, _| v.is_attached()));
    let seen = view.read_with(vcx, |v, _| v.last_seq());

    // Another tab in front: the stream is dropped, the emulator keeps what it has.
    click(vcx, "panel-tab-session");
    draw(vcx);
    assert!(!view.read_with(vcx, |v, _| v.is_attached()));
    fake.term_output(&id, "two\r\n");
    advance(vcx, Duration::from_millis(50));
    assert!(
        !screen_text(vcx, &view).contains("two"),
        "nothing arrives while it is away"
    );

    click(vcx, "panel-tab-terminal-1");
    wait_text(vcx, &view, "two");
    let text = screen_text(vcx, &view);
    assert_eq!(
        text.matches("one").count(),
        1,
        "replayed from last_seq, not from the start"
    );
    assert_eq!(text.matches("two").count(), 1);
    assert_eq!(
        view.read_with(vcx, |v, _| v.taps.attaches.clone()),
        vec![None, Some(seen)]
    );
}

#[gpui::test]
fn a_dropped_connection_reattaches_from_the_last_seq_with_a_backoff(cx: &mut TestAppContext) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let view = live_terminal(vcx, &screen);
    let id = term_id(vcx, &view);
    fake.term_output(&id, "before\r\n");
    wait_text(vcx, &view, "before");
    let seen = view.read_with(vcx, |v, _| v.last_seq());

    // The daemon's connection is cut and the shell prints while nobody listens.
    fake.sever();
    fake.term_output(&id, "during\r\n");
    // The first retry waits 500 ms; then it attaches from where it left off.
    for _ in 0..40 {
        advance(vcx, Duration::from_millis(250));
        draw(vcx);
        if screen_text(vcx, &view).contains("during") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let text = screen_text(vcx, &view);
    assert!(text.contains("during"), "{text}");
    assert_eq!(
        text.matches("before").count(),
        1,
        "no replay from the start: {text}"
    );
    let attaches = view.read_with(vcx, |v, _| v.taps.attaches.clone());
    assert_eq!(attaches[0], None);
    assert!(attaches.len() >= 2, "{attaches:?}");
    assert!(
        attaches[1..].iter().all(|a| *a == Some(seen)),
        "{attaches:?}"
    );
}

#[gpui::test]
fn paste_is_bracketed_only_when_the_program_asked_and_ctrl_c_is_the_shells(
    cx: &mut TestAppContext,
) {
    cx.update(crate::actions::bind_keys);
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let view = live_terminal(vcx, &screen);
    let id = term_id(vcx, &view);
    vcx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("ls\nls".to_owned())));

    vcx.simulate_keystrokes("cmd-v");
    wait_drawn(vcx, |vcx| !sent(vcx, &view).is_empty());
    assert_eq!(sent(vcx, &view), b"ls\rls".to_vec(), "line ends are Enter");

    // The program turns bracketed paste on: the next paste is framed.
    fake.term_output(&id, "\x1b[?2004h");
    wait_drawn(vcx, |vcx| {
        view.read_with(vcx, |v, _| v.emulator().bracketed_paste_mode())
    });
    vcx.simulate_keystrokes("cmd-v");
    wait_drawn(vcx, |vcx| sent(vcx, &view).ends_with(b"\x1b[201~"));
    assert!(sent(vcx, &view).ends_with(b"\x1b[200~ls\nls\x1b[201~"));

    // Ctrl-C is the shell's interrupt, whatever is selected.
    vcx.simulate_keystrokes("ctrl-c");
    wait_drawn(vcx, |vcx| sent(vcx, &view).ends_with(&[0x03]));
}

/// The window position of a point in the grid, in cells.
fn cell_at(
    vcx: &VisualTestContext,
    view: &Entity<TerminalView>,
    col: f32,
    row: f32,
) -> gpui::Point<gpui::Pixels> {
    let g = view.read_with(vcx, |v, _| v.geometry().expect("measured"));
    point(
        g.origin.x + px(g.cell_w * col),
        g.origin.y + px(g.line_h * (row + 0.5)),
    )
}

fn clipboard(vcx: &mut VisualTestContext) -> Option<String> {
    vcx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
}

#[gpui::test]
fn cmd_c_copies_the_selection_and_a_plain_click_selects_nothing(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_keys);
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let view = live_terminal(vcx, &screen);
    fake.term_output(&term_id(vcx, &view), "hello world\r\n");
    wait_text(vcx, &view, "hello world");
    vcx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("kept".to_owned())));

    // A press and release without travel selects nothing, and copy has nothing to do.
    let at = cell_at(vcx, &view, 1.0, 0.0);
    vcx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    assert!(!view.read_with(vcx, |v, _| v.emulator().has_selection()));
    vcx.simulate_keystrokes("cmd-c");
    assert_eq!(clipboard(vcx), Some("kept".to_owned()));

    // A drag over the first five columns selects "hello".
    let (from, to) = (cell_at(vcx, &view, 0.1, 0.0), cell_at(vcx, &view, 5.0, 0.0));
    vcx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    assert_eq!(
        view.read_with(vcx, |v, _| v.emulator().selection_text()),
        Some("hello".to_owned())
    );
    vcx.simulate_keystrokes("cmd-c");
    assert_eq!(clipboard(vcx), Some("hello".to_owned()));
    assert!(
        sent(vcx, &view).is_empty(),
        "copying typed nothing into the shell"
    );
    draw(vcx);
}

#[gpui::test]
fn a_program_that_tracks_the_mouse_gets_reports_and_shift_still_selects(cx: &mut TestAppContext) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let view = live_terminal(vcx, &screen);
    fake.term_output(&term_id(vcx, &view), "abcdef\r\n\x1b[?1000h\x1b[?1006h");
    wait_drawn(vcx, |vcx| {
        view.read_with(vcx, |v, _| v.emulator().sgr_mouse())
    });
    let at = cell_at(vcx, &view, 2.5, 0.0);
    vcx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    wait_drawn(vcx, |vcx| sent(vcx, &view).ends_with(b"\x1b[<0;3;1m"));
    assert!(sent(vcx, &view).starts_with(b"\x1b[<0;3;1M"));
    assert!(!view.read_with(vcx, |v, _| v.emulator().has_selection()));

    // Shift takes the pointer back for selecting.
    let shift = Modifiers {
        shift: true,
        ..Modifiers::none()
    };
    let (from, to) = (cell_at(vcx, &view, 0.1, 0.0), cell_at(vcx, &view, 3.0, 0.0));
    let n = sent(vcx, &view).len();
    vcx.simulate_mouse_down(from, MouseButton::Left, shift);
    vcx.simulate_mouse_move(to, MouseButton::Left, shift);
    vcx.simulate_mouse_up(to, MouseButton::Left, shift);
    assert_eq!(
        view.read_with(vcx, |v, _| v.emulator().selection_text()),
        Some("abc".to_owned())
    );
    assert_eq!(sent(vcx, &view).len(), n, "nothing was reported");
}

#[gpui::test]
fn the_wheel_scrolls_history_and_an_alt_screen_gets_arrows(cx: &mut TestAppContext) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let view = live_terminal(vcx, &screen);
    let id = term_id(vcx, &view);
    let mut lines = String::new();
    for n in 0..200 {
        lines.push_str(&format!("line {n}\r\n"));
    }
    fake.term_output(&id, &lines);
    wait_text(vcx, &view, "line 199");
    let g = view.read_with(vcx, |v, _| v.geometry().expect("measured"));
    let wheel = |vcx: &mut VisualTestContext, delta: ScrollDelta, phase: TouchPhase| {
        vcx.simulate_event(gpui::ScrollEvent {
            position: g.bounds.center(),
            delta,
            modifiers: Modifiers::none(),
            touch_phase: phase,
            source: gpui::ScrollSource::Pointer,
        });
    };
    // Quarter-row pixel events: nothing moves for three, then a row.
    for _ in 0..40 {
        wheel(
            vcx,
            ScrollDelta::Pixels(point(px(0.0), px(g.line_h / 4.0))),
            TouchPhase::Moved,
        );
    }
    assert_eq!(
        view.read_with(vcx, |v, _| v.emulator().display_offset()),
        10
    );
    // Back down past the live bottom rests flush with it.
    for _ in 0..60 {
        wheel(vcx, ScrollDelta::Lines(point(0.0, -0.7)), TouchPhase::Moved);
    }
    assert_eq!(view.read_with(vcx, |v, _| v.emulator().display_offset()), 0);
    // Typing while scrolled back goes to the live bottom first.
    wheel(vcx, ScrollDelta::Lines(point(0.0, 5.0)), TouchPhase::Moved);
    assert_eq!(view.read_with(vcx, |v, _| v.emulator().display_offset()), 5);
    vcx.simulate_input("x");
    assert_eq!(view.read_with(vcx, |v, _| v.emulator().display_offset()), 0);

    // A full-screen program that does not track the mouse gets the wheel as arrow keys.
    fake.term_output(&id, "\x1b[?1049h");
    wait_drawn(vcx, |vcx| {
        view.read_with(vcx, |v, _| v.emulator().alt_screen())
    });
    let n = sent(vcx, &view).len();
    wheel(vcx, ScrollDelta::Lines(point(0.0, 2.0)), TouchPhase::Moved);
    wait_drawn(vcx, |vcx| sent(vcx, &view).len() > n);
    assert_eq!(&sent(vcx, &view)[n..], b"\x1b[A\x1b[A");
}

#[gpui::test]
fn a_terminal_on_a_live_run_says_protected_files_are_read_only_and_after_it_ends_offers_a_reopen(
    cx: &mut TestAppContext,
) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let store = screen.read_with(vcx, |s, _| s.store.clone());
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| {
            s.state()
                .run(&run_id("builder1"))
                .is_some_and(|r| crate::vm::panel_home::holds_workspace(r.status))
        })
    });
    let view = live_terminal(vcx, &screen);
    assert!(view.read_with(vcx, |v, _| v.info().unwrap().sandboxed));
    wait_drawn(vcx, |vcx| painted(vcx, "terminal-banner"));
    assert!(!painted(vcx, "terminal-reopen-unlimited"));
    let text = screen.read_with(vcx, |s, cx| s.terminal_banner_text(cx));
    assert_eq!(
        text.as_deref(),
        Some(
            "Protected files (.claude, .codex, .reno, .mcp.json, git) are read-only here while the run is live"
        )
    );

    // The run ends: the shell keeps its limits, and a new one would have none.
    store.update(vcx, |s, cx| s.cancel_run(run_id("builder1"), cx));
    wait_drawn(vcx, |vcx| painted(vcx, "terminal-reopen-unlimited"));
    let text = screen
        .read_with(vcx, |s, cx| s.terminal_banner_text(cx))
        .unwrap();
    assert!(text.contains("started while the run was live"), "{text}");

    let old = term_id(vcx, &view);
    click(vcx, "terminal-reopen-unlimited");
    wait_drawn(vcx, |vcx| painted(vcx, "panel-tab-terminal-2"));
    let fresh = front(vcx, &screen);
    wait_phase(vcx, &fresh, Phase::Live);
    assert!(!fresh.read_with(vcx, |v, _| v.info().unwrap().sandboxed));
    assert_ne!(term_id(vcx, &fresh), old);
    assert!(!painted(vcx, "terminal-banner"), "no limits, no line");
    // The old tab is closed and its shell is gone from the daemon.
    assert!(!painted(vcx, "panel-tab-terminal-1"));
    assert_eq!(fake.terminals(), 1);
}

#[gpui::test]
fn a_terminal_in_place_says_so(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "reviewer1");
    let view = live_terminal(vcx, &screen);
    assert!(view.read_with(vcx, |v, _| v.info().unwrap().in_place));
    wait_drawn(vcx, |vcx| painted(vcx, "terminal-banner"));
    let text = screen
        .read_with(vcx, |s, cx| s.terminal_banner_text(cx))
        .unwrap();
    assert_eq!(text, "in /fake/worktrees/run_reviewer1, not a worktree");
    assert!(!painted(vcx, "terminal-reopen-unlimited"));
}

fn refuse(fake: &FakeDaemon, code: ErrorCode, message: &str) {
    fake.fail("term.open", ErrorObject::new(code, message));
}

#[gpui::test]
fn a_refused_open_says_why_in_the_tab(cx: &mut TestAppContext) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    refuse(&fake, ErrorCode::SandboxUnavailable, "no sandbox");
    press_t_from_home(vcx);
    wait_drawn(vcx, |vcx| painted(vcx, "terminal-failed"));
    let words = screen
        .read_with(vcx, |s, cx| s.terminal_failure(1, cx))
        .unwrap();
    assert_eq!(words.0, "No terminal on a live run");
    assert!(
        words
            .1
            .contains("A terminal cannot be opened on a live run on this Mac")
    );
    assert_eq!(fake.terminals(), 0);
    assert!(painted(vcx, "terminal-reopen"), "Try again");

    // The sandbox starts after all: Try again replaces the tab with a live shell.
    fake.clear_failures();
    click(vcx, "terminal-reopen");
    wait_drawn(vcx, |vcx| painted(vcx, "panel-tab-terminal-2"));
    wait_phase(vcx, &front(vcx, &screen), Phase::Live);
    assert!(!painted(vcx, "panel-tab-terminal-1"));
    assert_eq!(fake.terminals(), 1);
}

#[gpui::test]
fn a_removed_workspace_has_no_terminal(cx: &mut TestAppContext) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder0");
    // The Home row is dimmed for a removed workspace; the daemon says the same when asked.
    refuse(&fake, ErrorCode::WorkspaceUnavailable, "worktree removed");
    let surface = screen.update(vcx, |s, _| s.new_terminal_surface());
    screen.update_in(vcx, |s, window, cx| s.open_surface(surface, window, cx));
    wait_drawn(vcx, |vcx| painted(vcx, "terminal-failed"));
    let words = screen
        .read_with(vcx, |s, cx| s.terminal_failure(1, cx))
        .unwrap();
    assert_eq!(words.0, "Workspace removed");
    assert!(!painted(vcx, "terminal-reopen"), "nothing to try again");
}

#[gpui::test]
fn at_the_limit_an_ended_terminal_is_offered_for_closing(cx: &mut TestAppContext) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let old = live_terminal(vcx, &screen);
    fake.term_exit(&term_id(vcx, &old), 0);
    wait_drawn(vcx, |vcx| matches!(phase(vcx, &old), Phase::Ended { .. }));
    assert!(
        painted(vcx, "terminal-ended"),
        "an ended shell says so, with a Reopen"
    );

    refuse(&fake, ErrorCode::TerminalLimit, "16 terminals");
    press_t_from_home(vcx);
    wait_drawn(vcx, |vcx| painted(vcx, "terminal-close-ended"));
    let words = screen
        .read_with(vcx, |s, cx| s.terminal_failure(2, cx))
        .unwrap();
    assert_eq!(words.0, "Too many terminals");

    fake.clear_failures();
    click(vcx, "terminal-close-ended");
    wait_drawn(vcx, |vcx| painted(vcx, "panel-tab-terminal-3"));
    wait_phase(vcx, &front(vcx, &screen), Phase::Live);
    assert_eq!(fake.terminals(), 1, "the ended one was closed to make room");
}

#[gpui::test]
fn closing_a_terminal_with_a_program_in_it_asks_first(cx: &mut TestAppContext) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let view = live_terminal(vcx, &screen);
    let id = term_id(vcx, &view);
    fake.set_term_busy(&id, true);

    click(vcx, "panel-tab-close-terminal-1");
    wait_drawn(vcx, |vcx| painted(vcx, "terminal-confirm"));
    assert_eq!(fake.terminals(), 1, "nothing is closed yet");
    assert!(painted(vcx, "panel-tab-terminal-1"));

    // Keep it open.
    click(vcx, "terminal-confirm-keep");
    draw(vcx);
    assert!(!painted(vcx, "terminal-confirm"));
    assert_eq!(fake.terminals(), 1);

    // Close it anyway.
    click(vcx, "panel-tab-close-terminal-1");
    wait_drawn(vcx, |vcx| painted(vcx, "terminal-confirm-yes"));
    click(vcx, "terminal-confirm-yes");
    wait_drawn(vcx, |_| fake.terminals() == 0);
    assert!(!painted(vcx, "panel-tab-terminal-1"));
}

#[gpui::test]
fn a_shell_with_nothing_running_closes_at_once_and_a_closed_tab_reopens_a_fresh_shell(
    cx: &mut TestAppContext,
) {
    cx.update(crate::actions::bind_keys);
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let _view = live_terminal(vcx, &screen);
    click(vcx, "panel-tab-close-terminal-1");
    wait_drawn(vcx, |_| fake.terminals() == 0);
    assert!(!painted(vcx, "terminal-confirm"));

    // Cmd-Shift-T brings the tab back as a new shell: the old one is gone.
    vcx.simulate_keystrokes("cmd-shift-t");
    wait_drawn(vcx, |vcx| painted(vcx, "terminal-view"));
    wait_phase(vcx, &front(vcx, &screen), Phase::Live);
    assert_eq!(fake.terminals(), 1);
}

#[gpui::test]
fn the_cursor_blinks_rests_after_a_quiet_spell_and_holds_solid_while_typing(
    cx: &mut TestAppContext,
) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let view = live_terminal(vcx, &screen);
    // A steady cursor schedules nothing; the shell asks for a blinking block (DECSCUSR 1).
    draw(vcx);
    assert!(!view.read_with(vcx, |v, _| v.blink_running()));
    fake.term_output(&term_id(vcx, &view), "\x1b[1 q");
    wait_drawn(vcx, |vcx| {
        view.read_with(vcx, |v, _| v.emulator().cursor_style().1)
    });
    assert!(focused(vcx, &view));
    vcx.update(|window, _| window.activate_window());
    draw(vcx);
    assert!(vcx.update(|window, _| window.is_window_active()), "active");
    wait_drawn(vcx, |vcx| view.read_with(vcx, |v, _| v.blink_running()));
    assert!(view.read_with(vcx, |v, _| v.blink_on()));
    advance(vcx, Duration::from_millis(500));
    assert!(!view.read_with(vcx, |v, _| v.blink_on()), "off half");
    advance(vcx, Duration::from_millis(500));
    assert!(view.read_with(vcx, |v, _| v.blink_on()), "on half");

    // Typing holds it solid for a full period.
    advance(vcx, Duration::from_millis(500));
    vcx.simulate_input("a");
    assert!(view.read_with(vcx, |v, _| v.blink_on()));
    advance(vcx, Duration::from_millis(400));
    assert!(view.read_with(vcx, |v, _| v.blink_on()), "still solid");
    advance(vcx, Duration::from_millis(100));
    assert!(!view.read_with(vcx, |v, _| v.blink_on()));

    // After 15 quiet seconds it rests solid and schedules nothing more.
    for _ in 0..32 {
        advance(vcx, Duration::from_millis(500));
    }
    assert!(view.read_with(vcx, |v, _| v.blink_on()));
    assert!(!view.read_with(vcx, |v, _| v.blink_running()));
}

#[gpui::test]
fn focus_changes_are_reported_to_a_program_that_asks(cx: &mut TestAppContext) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1");
    let view = live_terminal(vcx, &screen);
    fake.term_output(&term_id(vcx, &view), "\x1b[?1004h");
    wait_drawn(vcx, |vcx| {
        view.read_with(vcx, |v, _| v.emulator().focus_reporting())
    });
    vcx.update(|window, cx| window.blur(cx));
    draw(vcx);
    wait_drawn(vcx, |vcx| sent(vcx, &view).ends_with(b"\x1b[O"));
    let handle = view.read_with(vcx, |v, _| v.focus_handle_for_tests());
    vcx.update(|window, cx| window.focus(&handle, cx));
    draw(vcx);
    wait_drawn(vcx, |vcx| sent(vcx, &view).ends_with(b"\x1b[I"));
}

#[gpui::test]
fn colours_wide_characters_and_composition_paint_in_both_appearances(cx: &mut TestAppContext) {
    use gpui::EntityInputHandler as _;
    for appearance in [Appearance::Dark, Appearance::Light] {
        let (_root, vcx, screen, fake) = open_run_in(cx, "builder1", appearance);
        let view = live_terminal(vcx, &screen);
        let id = term_id(vcx, &view);
        fake.term_output(
            &id,
            "\x1b[31mred\x1b[0m \x1b[1;32mbold green\x1b[0m \x1b[48;5;21m blue bg \x1b[0m \
             \x1b[38;2;10;200;30mtruecolor\x1b[0m \x1b[7minverse\x1b[0m \x1b[2mdim\x1b[0m \
             \x1b[4munder\x1b[0m \x1b[3mitalic\x1b[0m\r\n\
             wide: \u{65e5}\u{672c}\u{8a9e} \u{2192} box \u{2502}\u{2500}\u{256d} done\r\n\
             \x1b[38;5;232mgrey ramp\x1b[0m \x1b[38;5;196mcube\x1b[0m\r\n",
        );
        wait_text(vcx, &view, "done");
        draw(vcx);
        assert!(painted(vcx, "terminal-view"));
        assert!(row(vcx, &view, 1).contains("\u{65e5}\u{672c}\u{8a9e}"));

        // Input-method composition is shown at the cursor and committed as typed.
        view.update_in(vcx, |v, window, cx| {
            v.replace_and_mark_text_in_range(None, "\u{304b}\u{306a}", None, window, cx);
        });
        draw(vcx);
        assert!(
            view.update_in(vcx, |v, window, cx| v.marked_text_range(window, cx))
                .is_some()
        );
        view.update_in(vcx, |v, window, cx| {
            v.replace_text_in_range(None, "\u{304b}\u{306a}", window, cx);
        });
        draw(vcx);
        assert!(
            view.update_in(vcx, |v, window, cx| v.marked_text_range(window, cx))
                .is_none()
        );
        assert_eq!(sent(vcx, &view), "\u{304b}\u{306a}".as_bytes().to_vec());
    }
}

#[gpui::test]
fn files_and_terminal_are_dimmed_for_a_daemon_without_them(cx: &mut TestAppContext) {
    use crate::vm::panel_home::HomeKind;
    use gpui::AppContext as _;
    crate::tests::one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    let store = cx.new(|cx| {
        let mut store = crate::store::Store::connecting();
        store.start_fake_with(
            reno_client::fake::FakeConfig {
                without_features: vec![
                    reno_proto::FEATURE_FILES.to_owned(),
                    reno_proto::FEATURE_TERMINALS.to_owned(),
                ],
                ..crate::tests::instant_script()
            },
            cx,
        );
        store
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.state().approvals().len() == 2)
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    vcx.simulate_resize(size(px(1700.0), px(960.0)));
    vcx.update(|_, cx| {
        Prefs {
            open: Some(true),
            width: Some(800.0),
            ..Prefs::default()
        }
        .set_without_saving(cx);
    });
    draw(vcx);
    root.update_in(vcx, |r, window, cx| {
        r.open_run(run_id("builder1"), window, cx)
    });
    draw(vcx);
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    click(vcx, "panel-new-tab");
    wait_drawn(vcx, |vcx| painted(vcx, "home-row-terminal"));
    let home = screen.read_with(vcx, |s, cx| s.home(cx)).unwrap();
    for kind in [HomeKind::Files, HomeKind::Terminal] {
        let row = home.row(kind).unwrap();
        assert!(!row.enabled, "{kind:?}");
        assert_eq!(row.detail, "needs a newer daemon");
        assert_eq!(row.reason.as_deref(), Some("needs a newer daemon"));
    }
    // The letter does nothing, and a click does not open a tab.
    vcx.simulate_keystrokes("t f");
    click(vcx, "home-row-terminal");
    draw(vcx);
    assert_eq!(
        screen.read_with(vcx, |s, _| s.panel.active().clone()),
        Surface::Home
    );
    assert!(screen.read_with(vcx, |s, _| s.terminal_ids().is_empty()));
}

fn desc(kind: Kind, key: &str) -> Descriptor {
    Descriptor {
        kind,
        key: key.to_owned(),
    }
}

#[test]
fn saved_terminals_get_slots_and_slots_become_ids_again() {
    let saved = Descriptors {
        tabs: vec![
            desc(Kind::Session, ""),
            desc(Kind::Terminal, "term_a1"),
            desc(Kind::Terminal, "term_b2"),
            desc(Kind::Terminal, "not an id"),
            desc(Kind::Terminal, "term_a1"),
            desc(Kind::File, "src/a.rs"),
        ],
        active: Some(desc(Kind::Terminal, "term_b2")),
        closed: vec![desc(Kind::Terminal, "term_c3"), desc(Kind::File, "z.rs")],
        place: None,
    };
    let (terms, panel) = Terms::restore(saved);
    // One slot per id, in the order met; a bad id and a repeat add nothing.
    assert_eq!(
        panel.tabs,
        vec![
            desc(Kind::Session, ""),
            desc(Kind::Terminal, "1"),
            desc(Kind::Terminal, "2"),
            desc(Kind::Terminal, "1"),
            desc(Kind::File, "src/a.rs"),
        ]
    );
    assert_eq!(panel.active, Some(desc(Kind::Terminal, "2")));
    assert_eq!(
        panel.closed,
        vec![desc(Kind::File, "z.rs")],
        "a closed shell stays closed"
    );
    assert_eq!(terms.tabs.len(), 2);

    // Saving writes the ids back; a tab whose shell is not open yet is not saved.
    let mut terms = terms;
    terms
        .tabs
        .insert(3, super::terminal::TermTab::open_for_tests());
    let mut panel = panel;
    panel.tabs.push(desc(Kind::Terminal, "3"));
    panel.closed.push(desc(Kind::Terminal, "2"));
    let saved = terms.to_saved(panel);
    assert_eq!(
        saved.tabs,
        vec![
            desc(Kind::Session, ""),
            desc(Kind::Terminal, "term_a1"),
            desc(Kind::Terminal, "term_b2"),
            desc(Kind::Terminal, "term_a1"),
            desc(Kind::File, "src/a.rs"),
        ]
    );
    assert_eq!(saved.active, Some(desc(Kind::Terminal, "term_b2")));
    assert_eq!(saved.closed, vec![desc(Kind::File, "z.rs")]);
}

#[gpui::test]
fn a_restored_panel_reattaches_a_live_shell_and_says_a_dead_one_ended(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let at = crate::panel::persist::path_in(dir.path());
    cx.update(|cx| crate::panel::saved::Saved::install(at, cx));
    let (root, vcx, screen, fake) = open_run(cx, "builder1");
    let first = live_terminal(vcx, &screen);
    let (alive, dies) = (term_id(vcx, &first), {
        press_t_from_home(vcx);
        wait_drawn(vcx, |vcx| painted(vcx, "panel-tab-terminal-2"));
        let second = front(vcx, &screen);
        wait_phase(vcx, &second, Phase::Live);
        term_id(vcx, &second)
    });
    fake.term_output(&alive, "alpha\r\n");
    let _ = &first;

    // What is saved is the ids, under the panel's key.
    let key = screen.read_with(vcx, |s, _| s.panel_key.clone());
    let saved = vcx
        .update(|_, cx| crate::panel::saved::Saved::get(&key, cx))
        .unwrap();
    let ids: Vec<_> = saved
        .tabs
        .iter()
        .filter(|d| d.kind == Kind::Terminal)
        .map(|d| d.key.clone())
        .collect();
    assert_eq!(ids, vec![alive.to_string(), dies.to_string()]);
    assert_eq!(
        saved.active,
        Some(desc(Kind::Terminal, dies.as_str())),
        "the front tab is remembered by id"
    );

    // One shell dies (the daemon forgets it) and output piles up past what is held.
    let store = screen.read_with(vcx, |s, _| s.store.clone());
    let client = store.read_with(vcx, |s, _| s.client().unwrap());
    let gone = dies.clone();
    let task = vcx
        .update(|_, cx| gpui_tokio::Tokio::spawn(cx, async move { client.term_close(gone).await }));
    vcx.executor().allow_parking();
    wait_drawn(vcx, |_| fake.terminals() == 1);
    drop(task);
    for n in 0..(reno_client::fake::terminals::HELD + 20) {
        fake.term_output(&alive, &format!("row {n}\r\n"));
    }

    // Leave the run and come back: a new screen restores the tabs from what was saved.
    root.update_in(vcx, |r, window, cx| {
        r.open_run(run_id("reviewer1"), window, cx)
    });
    draw(vcx);
    root.update_in(vcx, |r, window, cx| {
        r.open_run(run_id("builder1"), window, cx)
    });
    draw(vcx);
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    assert_eq!(
        screen.read_with(vcx, |s, _| s.terminal_ids()),
        vec![(1, Some(alive.clone())), (2, Some(dies.clone()))]
    );

    // The dead one is in front (it was when saved): it says so and offers Reopen.
    wait_drawn(vcx, |vcx| painted(vcx, "terminal-gone"));
    assert!(painted(vcx, "terminal-reopen"));

    // The live one comes back with what the daemon held, a note where output was lost, and
    // no answers typed into the shell for what the replay asked.
    click(vcx, "panel-tab-terminal-1");
    wait_drawn(vcx, |vcx| painted(vcx, "terminal-view"));
    let back = front(vcx, &screen);
    wait_phase(vcx, &back, Phase::Live);
    wait_text(
        vcx,
        &back,
        &format!("row {}", reno_client::fake::terminals::HELD + 19),
    );
    let text = screen_text(vcx, &back);
    assert!(text.contains("some output was lost"), "a gap says so");
    assert_eq!(text.matches("some output was lost").count(), 1);
    assert_eq!(term_id(vcx, &back), alive);
    assert!(back.read_with(vcx, |v, _| v.info().unwrap().sandboxed));

    // Reopen replaces the dead tab with a fresh shell.
    click(vcx, "panel-tab-terminal-2");
    wait_drawn(vcx, |vcx| painted(vcx, "terminal-reopen"));
    click(vcx, "terminal-reopen");
    wait_drawn(vcx, |vcx| painted(vcx, "panel-tab-terminal-3"));
    wait_phase(vcx, &front(vcx, &screen), Phase::Live);
    assert!(!painted(vcx, "panel-tab-terminal-2"));
    assert_eq!(fake.terminals(), 2);
}

#[gpui::test]
fn what_the_emulator_answers_goes_back_to_the_shell_unless_it_was_a_replay(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let at = crate::panel::persist::path_in(dir.path());
    cx.update(|cx| crate::panel::saved::Saved::install(at, cx));
    let (root, vcx, screen, fake) = open_run(cx, "builder1");
    let view = live_terminal(vcx, &screen);
    let id = term_id(vcx, &view);
    // A cursor-position query printed live is answered, through the same queue as typing.
    fake.term_output(&id, "\x1b[6n");
    wait_drawn(vcx, |vcx| sent(vcx, &view).ends_with(b"R"));
    assert!(sent(vcx, &view).starts_with(b"\x1b["));

    // The same query in a replay was answered when it was live: a restored view stays quiet.
    root.update_in(vcx, |r, window, cx| {
        r.open_run(run_id("reviewer1"), window, cx)
    });
    draw(vcx);
    root.update_in(vcx, |r, window, cx| {
        r.open_run(run_id("builder1"), window, cx)
    });
    draw(vcx);
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    wait_drawn(vcx, |vcx| painted(vcx, "terminal-view"));
    let back = front(vcx, &screen);
    wait_phase(vcx, &back, Phase::Live);
    wait_drawn(vcx, |vcx| back.read_with(vcx, |v, _| v.last_seq()) >= 1);
    advance(vcx, Duration::from_millis(50));
    assert!(sent(vcx, &back).is_empty(), "{:?}", sent(vcx, &back));
}
