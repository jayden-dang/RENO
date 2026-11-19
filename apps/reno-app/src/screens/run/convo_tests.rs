//! The conversation on the Run screen against the fake daemon: no header, the send / stop
//! morph, a finished turn's fold, a closed question, the tick rail and the long-message toggle.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::{Entity, TestAppContext, VisualTestContext};

use super::RunScreen;
use crate::app::RootView;
use crate::tests::{
    click, draw, fake_store, kinds, painted, run_id, wait_drawn, wait_until, window,
};
use crate::transcript::Row;
use crate::ui::theme::Appearance;

fn open<'a>(
    cx: &'a mut TestAppContext,
    run: &str,
) -> (
    Entity<RootView>,
    &'a mut VisualTestContext,
    Entity<RunScreen>,
) {
    let store = fake_store(cx);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.state().approvals().len() == 2)
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let id = run_id(run);
    root.update_in(vcx, |r, window, cx| r.open_run(id, window, cx));
    draw(vcx);
    let screen = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("a run screen");
    (root, vcx, screen)
}

fn rows(vcx: &VisualTestContext, screen: &Entity<RunScreen>) -> Vec<Row> {
    screen.read_with(vcx, |s, _| s.rows().to_vec())
}

#[gpui::test]
fn the_canvas_has_no_run_header(cx: &mut TestAppContext) {
    let (_root, vcx, _screen) = open(cx, "builder1");
    wait_drawn(vcx, |vcx| painted(vcx, "chat-canvas"));
    for gone in [
        "crumb-project",
        "status-run_builder1",
        "header-stop",
        "usage-chip",
    ] {
        assert!(!painted(vcx, gone), "`{gone}` is still drawn");
    }
}

#[gpui::test]
fn stop_turns_into_send_as_soon_as_there_is_text(cx: &mut TestAppContext) {
    let (_root, vcx, screen) = open(cx, "builder1");
    wait_drawn(vcx, |vcx| painted(vcx, "composer"));
    let steer = screen.read_with(vcx, |s, _| s.steer_input().clone());
    assert!(painted(vcx, "composer-stop"), "live and empty: stop");
    assert!(!painted(vcx, "send"));
    steer.update_in(vcx, |c, window, cx| c.fill("use the mirror", window, cx));
    draw(vcx);
    assert!(painted(vcx, "send"), "text: send");
    assert!(!painted(vcx, "composer-stop"));
    steer.update_in(vcx, |c, window, cx| c.fill("", window, cx));
    draw(vcx);
    assert!(painted(vcx, "composer-stop"), "empty again: stop");
}

#[gpui::test]
fn a_finished_run_has_neither_stop_nor_a_live_send(cx: &mut TestAppContext) {
    let (_root, vcx, _screen) = open(cx, "reviewer1");
    wait_drawn(vcx, |vcx| painted(vcx, "composer"));
    assert!(!painted(vcx, "composer-stop"));
}

#[gpui::test]
fn a_finished_turn_folds_and_the_fold_expands_in_place(cx: &mut TestAppContext) {
    let (_root, vcx, screen) = open(cx, "reviewer1");
    wait_drawn(vcx, |vcx| {
        kinds(&rows(vcx, &screen)).contains(&"worked") && painted(vcx, "chat-canvas")
    });
    let rows = rows(vcx, &screen);
    let fold = rows
        .iter()
        .find_map(|r| match r {
            Row::Worked { key, secs, .. } => Some((*key, *secs)),
            _ => None,
        })
        .expect("a fold");
    // The run's start and end are 3 minutes 12 seconds apart and it was never steered.
    assert_eq!(fold.1, Some(192));
    let head = format!("worked-{}", fold.0.0);
    wait_drawn(vcx, |vcx| painted(vcx, &head));
    assert!(!painted(vcx, "step-r2"), "the steps are folded away");
    click(vcx, &head);
    wait_drawn(vcx, |vcx| painted(vcx, "step-r2"));
    assert!(painted(vcx, "step-r1"));
}

#[gpui::test]
fn an_answered_question_is_a_bubble_above_its_answer(cx: &mut TestAppContext) {
    let (_root, vcx, _screen) = open(cx, "builder0");
    wait_drawn(vcx, |vcx| painted(vcx, "question-answered"));
    assert!(!painted(vcx, "question-pending-note"));
}

#[gpui::test]
fn the_final_message_of_a_finished_turn_has_a_quiet_copy(cx: &mut TestAppContext) {
    let (_root, vcx, screen) = open(cx, "reviewer1");
    wait_drawn(vcx, |vcx| kinds(&rows(vcx, &screen)).contains(&"worked"));
    // The prose just before the Done row is the final message.
    let all = rows(vcx, &screen);
    let Some(Row::Prose { key, .. }) = all.get(all.len() - 2) else {
        panic!("the final message sits before Done: {all:?}");
    };
    let copy = format!("message-copy-{}", key.0);
    wait_drawn(vcx, |vcx| painted(vcx, &copy));
    click(vcx, &copy);
    let copied = vcx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default();
    assert!(
        copied.starts_with("The cache-control change is correct"),
        "{copied}"
    );
}

#[gpui::test]
fn a_question_nobody_can_answer_any_more_is_one_muted_line(cx: &mut TestAppContext) {
    let (_root, vcx, screen) = open(cx, "builder0");
    wait_drawn(vcx, |vcx| painted(vcx, "question-closed"));
    assert!(kinds(&rows(vcx, &screen)).contains(&"question"));
    for card in [
        "question-tag-declined",
        "question-declined-line",
        "question-dimmed-chip",
    ] {
        assert!(!painted(vcx, card), "`{card}`: the card is gone");
    }
}

#[gpui::test]
fn a_long_user_message_collapses_and_opens(cx: &mut TestAppContext) {
    let (_root, vcx, _screen) = open(cx, "builder0");
    wait_drawn(vcx, |vcx| painted(vcx, "user-more"));
    assert!(!painted(vcx, "user-less"));
    click(vcx, "user-more");
    wait_drawn(vcx, |vcx| painted(vcx, "user-less"));
}

/// A thread of two runs whose first is long enough that the transcript scrolls.
fn long_thread<'a>(
    cx: &'a mut TestAppContext,
) -> (
    Entity<RootView>,
    &'a mut VisualTestContext,
    Entity<RunScreen>,
) {
    use crate::tests::{run_status, send_from_home};
    let current_run_of = |vcx: &mut VisualTestContext, root: &Entity<RootView>| {
        let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
        screen.read_with(vcx, |s, _| s.run_id().clone())
    };
    let store = fake_store(cx);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.state().approvals().len() == 2)
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let first = send_from_home(vcx, &root, "/burst 3000");
    wait_drawn(vcx, |vcx| {
        run_status(vcx, &store, &first).is_some_and(reno_proto::RunStatus::is_terminal)
    });
    let screen = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("a run screen");
    let steer = screen.read_with(vcx, |s, _| s.steer_input().clone());
    steer.update_in(vcx, |c, window, cx| c.fill("second task", window, cx));
    draw(vcx);
    click(vcx, "send");
    wait_drawn(vcx, |vcx| current_run_of(vcx, &root) != first);
    let screen = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("a run screen");
    wait_drawn(vcx, |vcx| {
        let rows = rows(vcx, &screen);
        rows.iter()
            .filter(|r| matches!(r, Row::User { .. }))
            .count()
            == 2
            && kinds(&rows).last() == Some(&"done")
    });
    (root, vcx, screen)
}

fn user_rows(vcx: &VisualTestContext, screen: &Entity<RunScreen>) -> Vec<usize> {
    rows(vcx, screen)
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(r, Row::User { .. }))
        .map(|(ix, _)| ix)
        .collect()
}

#[gpui::test]
fn the_tick_rail_is_a_minimap_and_a_click_really_scrolls(cx: &mut TestAppContext) {
    let (_root, vcx, screen) = long_thread(cx);
    wait_drawn(vcx, |vcx| painted(vcx, "tick-rail"));
    let users = user_rows(vcx, &screen);
    assert_eq!(users.len(), 2);
    let (first, second) = (format!("tick-{}", users[0]), format!("tick-{}", users[1]));
    wait_drawn(vcx, |vcx| painted(vcx, &first) && painted(vcx, &second));
    let rail = vcx.debug_bounds("tick-rail").unwrap();
    let (a, b) = (
        vcx.debug_bounds(Box::leak(first.clone().into_boxed_str()))
            .unwrap(),
        vcx.debug_bounds(Box::leak(second.clone().into_boxed_str()))
            .unwrap(),
    );
    // The first message is at the top of the rail; the second, after a long answer, is far below.
    assert!(a.top() - rail.top() < gpui::px(14.0), "{a:?} in {rail:?}");
    assert!(
        b.top() - rail.top() > rail.size.height * 0.6,
        "{b:?} in {rail:?}"
    );
    // The transcript follows its tail: the message in view is the last one.
    assert!(screen.read_with(vcx, |s, _| s.top_row()) > 0);
    // A click scrolls the list for real, so that message is at the top.
    click(vcx, &first);
    assert_eq!(screen.read_with(vcx, |s, _| s.top_row()), users[0]);
    wait_drawn(vcx, |vcx| painted(vcx, "tick-current"));
    let current = vcx.debug_bounds("tick-current").unwrap();
    assert!(current.top() < b.top(), "the first message is the one lit");
    // The last message is near the end, so the list stops at its bottom: the top is at or below it.
    click(vcx, &second);
    assert!(screen.read_with(vcx, |s, _| s.top_row()) >= users[1]);
}

#[gpui::test]
fn a_transcript_that_fits_has_no_tick_rail(cx: &mut TestAppContext) {
    let (_root, vcx, _screen) = open(cx, "reviewer1");
    wait_drawn(vcx, |vcx| {
        painted(vcx, "message-copy-0") || painted(vcx, "composer")
    });
    draw(vcx);
    assert!(!painted(vcx, "tick-rail"));
}

#[gpui::test]
fn a_live_run_between_steps_says_thinking(cx: &mut TestAppContext) {
    let (_root, vcx, _screen) = open(cx, "builder1");
    wait_drawn(vcx, |vcx| painted(vcx, "thinking"));
}

#[gpui::test]
fn the_side_panel_does_not_repeat_the_run_status(cx: &mut TestAppContext) {
    // keeper1 is held for the owner: the composer slot says so, the panel need not.
    let (_root, vcx, _screen) = open(cx, "keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "panel-canvas"));
    crate::tests::show_session(vcx);
    assert!(painted(vcx, "details"));
    assert!(!painted(vcx, "session-finding"));
    click(vcx, "panel-new-tab");
    wait_drawn(vcx, |vcx| painted(vcx, "home-add-action"));
    assert!(!painted(vcx, "home-now"));
}
