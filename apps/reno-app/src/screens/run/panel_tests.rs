//! The side panel on the run screen, against the fake daemon: layout, the tab strip and its keys,
//! the Home page and its letters, the tokens chip and the header's Stop.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::{Entity, Modifiers, MouseButton, TestAppContext, VisualTestContext, point, px, size};

use super::RunScreen;
use crate::actions::{
    ClosePanelTab, NewPanelTab, NextPanelTab, PrevPanelTab, ReopenPanelTab, TogglePanel,
};
use crate::app::RootView;
use crate::panel::surface::Surface;
use crate::tests::{click, draw, fake_store, painted, run_id, wait_drawn, wait_until, window};
use crate::ui::theme::Appearance;
use crate::vm::panel_home::HomeKind;

fn open(vcx: &mut VisualTestContext, root: &Entity<RootView>, run: &str) -> Entity<RunScreen> {
    let id = run_id(run);
    root.update_in(vcx, |r, window, cx| r.open_run(id, window, cx));
    draw(vcx);
    root.read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("a run screen")
}

fn active(vcx: &VisualTestContext, screen: &Entity<RunScreen>) -> Surface {
    screen.read_with(vcx, |s, _| s.panel.active().clone())
}

fn tabs(vcx: &VisualTestContext, screen: &Entity<RunScreen>) -> Vec<Surface> {
    screen.read_with(vcx, |s, _| s.panel.tabs().to_vec())
}

/// builder1 changed files: Review is in front, with the transcript beside it.
fn ready(cx: &mut TestAppContext) -> (Entity<RootView>, &mut VisualTestContext, Entity<RunScreen>) {
    let store = fake_store(cx);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.state().approvals().len() == 2)
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let screen = open(vcx, &root, "builder1");
    wait_drawn(vcx, |vcx| painted(vcx, "panel-tab-review"));
    (root, vcx, screen)
}

#[gpui::test]
fn the_run_is_two_canvases_with_a_gutter_and_the_panel_hides_and_returns(cx: &mut TestAppContext) {
    let (_root, vcx, screen) = ready(cx);
    assert!(painted(vcx, "chat-canvas"));
    assert!(painted(vcx, "panel-canvas"));
    assert!(painted(vcx, "panel-gutter"));
    let chat = vcx.debug_bounds("chat-canvas").unwrap();
    let panel = vcx.debug_bounds("panel-canvas").unwrap();
    let gutter = vcx.debug_bounds("panel-gutter").unwrap();
    assert!(
        chat.right() <= gutter.left() + px(0.5),
        "the chat ends at the gutter"
    );
    assert!(
        (gutter.size.width - px(8.0)).abs() < px(0.5),
        "an 8 px gutter"
    );
    assert!(gutter.right() <= panel.left() + px(0.5));
    assert!(panel.size.width >= px(440.0) && chat.size.width >= px(420.0));
    // The tab strip is 42 px.
    assert!((vcx.debug_bounds("panel-tabs").unwrap().size.height - px(42.0)).abs() < px(0.5));

    click(vcx, "panel-hide");
    draw(vcx);
    assert!(!painted(vcx, "panel-canvas"));
    assert!(!painted(vcx, "panel-gutter"));
    // The toggle brings it back, to where it was.
    click(vcx, "details-toggle");
    draw(vcx);
    assert!(painted(vcx, "panel-canvas"));
    assert_eq!(active(vcx, &screen), Surface::Review);
    vcx.dispatch_action(TogglePanel);
    draw(vcx);
    assert!(!painted(vcx, "panel-canvas"));
    vcx.dispatch_action(TogglePanel);
    draw(vcx);
    assert!(painted(vcx, "panel-canvas"));
}

#[gpui::test]
fn a_narrow_window_overlays_the_chat_instead_of_splitting_it(cx: &mut TestAppContext) {
    let (_root, vcx, _screen) = ready(cx);
    vcx.simulate_resize(size(px(1000.0), px(800.0)));
    draw(vcx);
    // Below 1100 px the panel starts closed, and opens over the whole chat.
    assert!(!painted(vcx, "panel-canvas"));
    click(vcx, "details-toggle");
    draw(vcx);
    let chat = vcx.debug_bounds("chat-canvas").unwrap();
    let panel = vcx.debug_bounds("panel-canvas").unwrap();
    assert!(panel.left() < chat.right(), "the panel is over the chat");
    assert!(!painted(vcx, "panel-gutter"), "no gutter when it overlays");
    assert!(chat.size.width > px(600.0), "the chat keeps its width");
}

#[gpui::test]
fn dragging_the_gutter_resizes_the_panel_within_the_limits(cx: &mut TestAppContext) {
    let (_root, vcx, _screen) = ready(cx);
    let before = vcx.debug_bounds("panel-canvas").unwrap().size.width;
    let gutter = vcx.debug_bounds("panel-gutter").unwrap().center();
    vcx.simulate_mouse_down(gutter, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_move(
        point(gutter.x - px(40.0), gutter.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    vcx.simulate_mouse_move(
        point(gutter.x - px(80.0), gutter.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    vcx.simulate_mouse_up(
        point(gutter.x - px(80.0), gutter.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    draw(vcx);
    let after = vcx.debug_bounds("panel-canvas").unwrap().size.width;
    assert!(after > before + px(40.0), "{before:?} -> {after:?}");
    // Far past the chat's minimum, the panel stops where the chat keeps 420 px.
    let gutter = vcx.debug_bounds("panel-gutter").unwrap().center();
    vcx.simulate_mouse_down(gutter, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_move(
        point(px(300.0), gutter.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    vcx.simulate_mouse_move(
        point(px(290.0), gutter.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    vcx.simulate_mouse_up(
        point(px(290.0), gutter.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    draw(vcx);
    let chat = vcx.debug_bounds("chat-canvas").unwrap().size.width;
    assert!(chat >= px(419.0), "{chat:?}");
}

#[gpui::test]
fn the_strip_opens_closes_cycles_reopens_and_reorders_tabs(cx: &mut TestAppContext) {
    let (_root, vcx, screen) = ready(cx);
    // Session is pinned: it has no close button. Review has one.
    assert!(!painted(vcx, "panel-tab-close-session"));
    assert!(painted(vcx, "panel-tab-close-review"));

    // + opens a New tab showing the home.
    click(vcx, "panel-new-tab");
    assert_eq!(active(vcx, &screen), Surface::Home);
    draw(vcx);
    assert!(painted(vcx, "panel-home"));
    assert!(painted(vcx, "panel-tab-home"));

    // Closing the New tab falls back to the tab used before it.
    click(vcx, "panel-tab-close-home");
    assert_eq!(active(vcx, &screen), Surface::Review);

    // Cycling wraps both ways.
    vcx.dispatch_action(NextPanelTab);
    assert_eq!(active(vcx, &screen), Surface::Session);
    vcx.dispatch_action(PrevPanelTab);
    assert_eq!(active(vcx, &screen), Surface::Review);

    // Cmd+W closes the shown tab, and Cmd+Shift+T brings it back.
    vcx.dispatch_action(ClosePanelTab);
    assert_eq!(tabs(vcx, &screen), [Surface::Session]);
    assert_eq!(active(vcx, &screen), Surface::Session);
    vcx.dispatch_action(ClosePanelTab);
    assert_eq!(
        tabs(vcx, &screen),
        [Surface::Session],
        "Session cannot close"
    );
    vcx.dispatch_action(ReopenPanelTab);
    assert_eq!(active(vcx, &screen), Surface::Review);
    vcx.dispatch_action(NewPanelTab);
    assert_eq!(active(vcx, &screen), Surface::Home);

    // A drag lands as a reorder; Session stays first whatever is asked.
    screen.update_in(vcx, |s, window, cx| {
        s.open_surface(Surface::Planned(HomeKind::History), window, cx);
        s.panel_cmd(
            crate::panel::view::Cmd::Reorder { from: 3, to: 1 },
            window,
            cx,
        );
        s.panel_cmd(
            crate::panel::view::Cmd::Reorder { from: 1, to: 0 },
            window,
            cx,
        );
    });
    assert_eq!(
        tabs(vcx, &screen),
        [
            Surface::Session,
            Surface::Planned(HomeKind::History),
            Surface::Review,
            Surface::Home
        ]
    );
}

#[gpui::test]
fn the_home_lists_its_rows_with_letters_and_opens_one_with_its_key(cx: &mut TestAppContext) {
    let (_root, vcx, screen) = ready(cx);
    click(vcx, "panel-new-tab");
    draw(vcx);
    for row in [
        "changes",
        "files",
        "history",
        "terminal",
        "side-chat",
        "fork-chat",
    ] {
        assert!(painted(vcx, &format!("home-row-{row}")), "{row}");
    }
    assert!(painted(vcx, "home-add-action"));
    assert!(painted(vcx, "home-now"));

    // A built row opens its tab: C goes to Review (the New tab is replaced).
    vcx.simulate_keystrokes("c");
    draw(vcx);
    assert_eq!(active(vcx, &screen), Surface::Review);
    assert!(!tabs(vcx, &screen).contains(&Surface::Home));

    // F opens the file browser.
    click(vcx, "panel-new-tab");
    vcx.simulate_keystrokes("f");
    draw(vcx);
    assert_eq!(active(vcx, &screen), Surface::Files);
    assert!(painted(vcx, "panel-tab-files"));

    // A row that is not built opens a placeholder that says so.
    click(vcx, "panel-new-tab");
    vcx.simulate_keystrokes("h");
    draw(vcx);
    assert_eq!(active(vcx, &screen), Surface::Planned(HomeKind::History));
    assert!(painted(vcx, "panel-planned-history"));
    assert!(painted(vcx, "panel-tab-planned-history"));

    // A letter with no row is passed on, so the home stays.
    click(vcx, "panel-new-tab");
    vcx.simulate_keystrokes("x");
    assert_eq!(active(vcx, &screen), Surface::Home);

    // Clicking a row works too, and + Add says it is not built.
    click(vcx, "home-row-terminal");
    draw(vcx);
    assert!(matches!(active(vcx, &screen), Surface::Terminal(_)));
    click(vcx, "panel-new-tab");
    click(vcx, "home-add-action");
    let toasts = vcx.update(|_, cx| crate::ui::toast::shown(cx));
    assert!(
        toasts
            .iter()
            .any(|(_, message)| message.contains("Actions")),
        "a not-yet toast"
    );
}

#[gpui::test]
fn a_letter_typed_in_the_steer_field_types_it_and_never_opens_a_row(cx: &mut TestAppContext) {
    let (_root, vcx, screen) = ready(cx);
    click(vcx, "panel-new-tab");
    draw(vcx);
    // The steer field takes the keyboard back from the panel.
    let steer = screen.read_with(vcx, |s, _| s.steer_input().clone());
    steer.update_in(vcx, |c, window, cx| {
        use gpui::Focusable as _;
        window.focus(&c.focus_handle(cx), cx);
    });
    vcx.simulate_input("c");
    assert_eq!(active(vcx, &screen), Surface::Home);
    assert_eq!(steer.read_with(vcx, |c, _| c.text().to_owned()), "c");
}

#[gpui::test]
fn dimmed_rows_stay_with_their_reason_and_do_not_open(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.state().approvals().len() == 2)
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    // builder0's workspace was removed.
    let screen = open(vcx, &root, "builder0");
    wait_drawn(vcx, |vcx| painted(vcx, "panel-tab-review"));
    click(vcx, "panel-new-tab");
    draw(vcx);
    let home = screen.read_with(vcx, |s, cx| s.home(cx).unwrap());
    for kind in [
        HomeKind::Changes,
        HomeKind::Files,
        HomeKind::History,
        HomeKind::Terminal,
    ] {
        let row = home.row(kind).unwrap();
        assert!(!row.enabled, "{kind:?}");
        assert_eq!(row.detail, "workspace removed");
        assert!(painted(
            vcx,
            &format!("home-row-{}", row.label.to_lowercase())
        ));
    }
    vcx.simulate_keystrokes("f");
    assert_eq!(
        active(vcx, &screen),
        Surface::Home,
        "a dimmed row does nothing"
    );
}

#[gpui::test]
fn closed_tabs_are_listed_on_the_home_and_open_again_from_it(cx: &mut TestAppContext) {
    let (_root, vcx, screen) = ready(cx);
    click(vcx, "panel-tab-close-review");
    click(vcx, "panel-new-tab");
    draw(vcx);
    assert!(painted(vcx, "home-closed-0"));
    click(vcx, "home-closed-0");
    draw(vcx);
    assert_eq!(active(vcx, &screen), Surface::Review);
    assert!(painted(vcx, "review"));
}

#[gpui::test]
fn the_tokens_chip_hides_at_zero_and_opens_session(cx: &mut TestAppContext) {
    let (_root, vcx, screen) = ready(cx);
    // builder1 has reported no tokens yet in the fake script's first frames: no chip.
    let has_usage = screen.read_with(vcx, |s, _| s.facts().usage.is_some_and(|u| u.total() > 0));
    assert_eq!(painted(vcx, "usage-chip"), has_usage);
    if !has_usage {
        screen.update(vcx, |s, cx| {
            s.facts.usage = Some(crate::transcript::Usage {
                input: 1_000,
                output: 400,
            });
            cx.notify();
        });
        draw(vcx);
    }
    assert!(painted(vcx, "usage-chip"));
    click(vcx, "usage-chip");
    assert_eq!(active(vcx, &screen), Surface::Session);
    assert!(painted(vcx, "details"));
}

#[gpui::test]
fn a_live_run_has_stop_in_the_header_and_an_ended_one_does_not(cx: &mut TestAppContext) {
    let (_root, vcx, _screen) = ready(cx);
    assert!(painted(vcx, "header-stop"), "builder1 is running");
    click(vcx, "header-stop");
    wait_drawn(vcx, |vcx| !painted(vcx, "header-stop"));
}
