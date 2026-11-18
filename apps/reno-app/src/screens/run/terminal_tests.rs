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
use reno_proto::{ErrorCode, ErrorObject, RunId, TermId};

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
    screen.read_with(vcx, |s, _| s.terminal_in_front().expect("a terminal in front"))
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
fn live_terminal(
    vcx: &mut VisualTestContext,
    screen: &Entity<RunScreen>,
) -> Entity<TerminalView> {
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
    assert!(!painted(vcx, "panel-home"), "Home was replaced by what it opened");
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
    wait_drawn(vcx, |vcx| matches!(active(vcx), Surface::Terminal(_)) || {
        vcx.simulate_keystrokes("ctrl-tab");
        false
    });
    vcx.simulate_keystrokes("cmd-w");
    wait_drawn(vcx, |_| fake.terminals() == 0);
    assert!(!painted(vcx, "panel-tab-terminal-1"));
}
