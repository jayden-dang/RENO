//! Slice 4 of the app shell: the question panel above the composer, against the fake daemon's
//! mocked answer contract; "real" is the fake with the real daemon's capabilities.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_proto::{ExecEvent, RunStatus};

use crate::app::RootView;
use crate::store::Store;
use crate::tests::{click, draw, painted, run_id, wait_drawn, wait_until, window};
use crate::ui::theme::Appearance;
use crate::vm::capabilities::Capabilities;

/// A connected fake store with the demo's question runs (asker1, grok1, pager1).
fn asking(cx: &mut TestAppContext) -> Entity<Store> {
    crate::tests::one_window_test_at_a_time();
    cx.update(|cx| {
        crate::actions::bind_keys(cx);
        crate::ui::composer::bind_keys(cx);
        crate::screens::run::bind_keys(cx);
    });
    cx.update(gpui_tokio::init);
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(
            reno_client::fake::FakeConfig {
                step: std::time::Duration::ZERO,
                question_run: true,
                ..reno_client::fake::FakeConfig::default()
            },
            cx,
        );
        store
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| {
            s.state().run(&run_id("pager1")).is_some() && !s.state().executors().is_empty()
        })
    });
    store
}

fn open_run<'a>(
    cx: &'a mut TestAppContext,
    store: &Entity<Store>,
    run: &str,
) -> (Entity<RootView>, &'a mut VisualTestContext) {
    let (root, vcx) = window(cx, store, Appearance::Dark);
    draw(vcx);
    click(vcx, &format!("recent-run_{run}"));
    wait_drawn(vcx, |vcx| painted(vcx, "question-panel"));
    (root, vcx)
}

fn status(store: &Entity<Store>, cx: &impl gpui::AppContext, run: &str) -> RunStatus {
    store.read_with(cx, |s, _| s.state().run(&run_id(run)).unwrap().status)
}

fn has_answer(store: &Entity<Store>, cx: &impl gpui::AppContext, run: &str) -> bool {
    store.read_with(cx, |s, _| {
        s.state().transcript(&run_id(run)).is_some_and(|t| {
            t.events
                .iter()
                .any(|(_, e)| matches!(e, ExecEvent::ToolResult { summary: Some(s), .. } if s.starts_with("User answered")))
        })
    })
}

fn screen(
    root: &Entity<RootView>,
    vcx: &VisualTestContext,
) -> Entity<crate::screens::run::RunScreen> {
    root.read_with(vcx, |r, _| r.run_screen().unwrap().clone())
}

#[gpui::test]
fn a_single_choice_sends_on_click_and_the_transcript_shows_the_answered_bubble(
    cx: &mut TestAppContext,
) {
    let store = asking(cx);
    let (_root, vcx) = open_run(cx, &store, "asker1");
    assert!(painted(vcx, "composer"), "the panel sits above the composer");
    assert!(painted(vcx, "question-option-2") && painted(vcx, "question-skip"));
    assert!(!painted(vcx, "question-send"), "a single choice has no Send of its own to press first");
    click(vcx, "question-option-2");
    wait_drawn(vcx, |vcx| painted(vcx, "question-answered"));
    assert!(has_answer(&store, vcx, "asker1"));
    assert_eq!(status(&store, vcx, "asker1"), RunStatus::Running);
    assert!(!painted(vcx, "question-panel") && !painted(vcx, "question-chip"));
}

#[gpui::test]
fn typed_text_to_the_grok_question_is_refused_inline_and_nothing_is_sent(cx: &mut TestAppContext) {
    let store = asking(cx);
    let (root, vcx) = open_run(cx, &store, "grok1");
    let run = screen(&root, vcx);
    click(vcx, "question-write-own");
    let input = run.read_with(vcx, |r, _| r.question_input().unwrap().clone());
    input.update_in(vcx, |c, window, cx| c.fill("yaml please", window, cx));
    draw(vcx);
    click(vcx, "question-send");
    wait_drawn(vcx, |vcx| painted(vcx, "question-refusal"));
    assert_eq!(
        run.read_with(vcx, |r, _| r.question_refusal()),
        Some("Grok takes only one of: JSON lines, Plain text".to_owned())
    );
    assert!(!has_answer(&store, vcx, "grok1"));
    assert_eq!(status(&store, vcx, "grok1"), RunStatus::WaitingInput);
    // Picking an option clears the refusal and sends.
    click(vcx, "question-option-1");
    wait_drawn(vcx, |vcx| painted(vcx, "question-answered"));
    assert!(has_answer(&store, vcx, "grok1"));
}

#[gpui::test]
fn esc_with_the_composer_focused_does_nothing_and_cmd_j_then_a_digit_answers(
    cx: &mut TestAppContext,
) {
    let store = asking(cx);
    let (root, vcx) = open_run(cx, &store, "asker1");
    let steer = screen(&root, vcx).read_with(vcx, |r, _| r.steer_input().clone());
    steer.update_in(vcx, |c, window, cx| {
        window.focus(&c.focus_handle_ref().clone(), cx);
    });
    draw(vcx);
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(painted(vcx, "question-panel"), "Esc stayed with the composer");
    vcx.simulate_keystrokes("1");
    draw(vcx);
    assert!(!has_answer(&store, vcx, "asker1"), "a digit did not answer");
    vcx.simulate_keystrokes("cmd-j");
    draw(vcx);
    vcx.simulate_keystrokes("2");
    wait_until(vcx, |cx| status(&store, cx, "asker1") == RunStatus::Running);
    assert!(has_answer(&store, vcx, "asker1"));
}

#[gpui::test]
fn esc_on_a_claimed_panel_closes_it_and_the_chip_reopens_it(cx: &mut TestAppContext) {
    let store = asking(cx);
    let (_root, vcx) = open_run(cx, &store, "asker1");
    click(vcx, "question-header");
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert!(!painted(vcx, "question-panel"));
    assert!(painted(vcx, "question-chip"), "Question waiting \u{b7} Open");
    assert_eq!(status(&store, vcx, "asker1"), RunStatus::WaitingInput);
    click(vcx, "question-chip");
    draw(vcx);
    assert!(painted(vcx, "question-panel") && !painted(vcx, "question-chip"));
    click(vcx, "question-close");
    draw(vcx);
    assert!(painted(vcx, "question-chip"), "the \u{d7} closes it too");
}

#[gpui::test]
fn two_questions_page_with_next_and_send_the_whole_set(cx: &mut TestAppContext) {
    let store = asking(cx);
    let (root, vcx) = open_run(cx, &store, "pager1");
    let run = screen(&root, vcx);
    assert!(painted(vcx, "question-counter") && painted(vcx, "question-next"));
    assert!(!painted(vcx, "question-send"), "Send waits for the last page");
    click(vcx, "question-option-1");
    assert!(!has_answer(&store, vcx, "pager1"), "a paged ask does not send on click");
    click(vcx, "question-next");
    draw(vcx);
    assert_eq!(run.read_with(vcx, |r, _| r.question_page()), Some(1));
    assert!(painted(vcx, "question-send") && !painted(vcx, "question-next"));
    click(vcx, "question-option-1");
    click(vcx, "question-option-3");
    click(vcx, "question-send");
    wait_drawn(vcx, |vcx| painted(vcx, "question-answered"));
    assert!(has_answer(&store, vcx, "pager1"));
}

#[gpui::test]
fn with_the_real_capabilities_the_panel_is_read_only(cx: &mut TestAppContext) {
    let store = asking(cx);
    store.update(cx, |s, cx| s.set_capabilities(Capabilities::REAL, cx));
    let (_root, vcx) = open_run(cx, &store, "asker1");
    assert!(painted(vcx, "question-prompt"), "the question shows");
    assert!(painted(vcx, "question-readonly-note"), "Answer it in Claude Code for now");
    for gone in [
        "question-send",
        "question-skip",
        "question-option-1",
        "question-write-own",
    ] {
        assert!(!painted(vcx, gone), "{gone} would pretend to answer");
    }
    assert!(painted(vcx, "composer"));
    assert_eq!(status(&store, vcx, "asker1"), RunStatus::WaitingInput);
}
