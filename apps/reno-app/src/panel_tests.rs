//! Slice 3 of the app shell: the composer's controls and the approval panel that takes its place.
//! Run against the fake daemon; "real" is the fake with the real daemon's capabilities.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_client::fake::ApprovalDisconnect;
use reno_proto::{ApprovalStatus, Effort, ErrorCode, ErrorObject};

use crate::app::RootView;
use crate::store::Store;
use crate::tests::{
    approvals, click, draw, fake_store, painted, run_id, wait_drawn, wait_until, window,
};
use crate::ui::composer::{Composer, bind_keys};
use crate::ui::theme::Appearance;
use crate::vm::capabilities::Capabilities;

/// A fake store with two held approvals (keeper's push, scout's fetch), connected.
fn held(cx: &mut TestAppContext) -> Entity<Store> {
    cx.update(|cx| {
        crate::actions::bind_keys(cx);
        bind_keys(cx);
        crate::screens::run::bind_keys(cx);
    });
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| !s.state().executors().is_empty())
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
    draw(vcx);
    (root, vcx)
}

fn steer(vcx: &VisualTestContext, root: &Entity<RootView>) -> Entity<Composer> {
    root.read_with(vcx, |r, cx| {
        r.run_screen().unwrap().read(cx).steer_input().clone()
    })
}

fn status_of(
    store: &Entity<Store>,
    vcx: &VisualTestContext,
    approval: &str,
) -> Option<ApprovalStatus> {
    store.read_with(vcx, |s, _| {
        s.state()
            .approval_history()
            .iter()
            .find(|r| r.id.as_str() == approval)
            .map(|r| r.status)
    })
}

#[gpui::test]
fn the_store_derives_its_capabilities_from_the_connection(cx: &mut TestAppContext) {
    let real = cx.update(|cx| cx.new(|_| Store::connecting()));
    assert_eq!(
        real.read_with(cx, |s, _| s.capabilities()),
        Capabilities::REAL
    );
    let store = held(cx);
    assert_eq!(
        store.read_with(cx, |s, _| s.capabilities()),
        Capabilities::FAKE
    );
}

#[gpui::test]
fn the_approval_panel_replaces_the_composer_and_allow_once_decides(cx: &mut TestAppContext) {
    let store = held(cx);
    let (_root, vcx) = open_run(cx, &store, "keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "approval-panel"));
    assert!(
        !painted(vcx, "composer"),
        "the panel takes the composer's place"
    );
    assert!(painted(vcx, "approval-kind"), "the kind label shows");
    assert!(painted(vcx, "approval-detail"), "the exact command shows");
    click(vcx, "approve");
    wait_until(vcx, |cx| approvals(cx, &store) == 1);
    wait_drawn(vcx, |vcx| {
        painted(vcx, "composer") && !painted(vcx, "approval-panel")
    });
    wait_drawn(vcx, |vcx| {
        store.update(vcx, |s, cx| s.load_history(cx));
        status_of(&store, vcx, "appr_keeperpush") == Some(ApprovalStatus::Approved)
    });
}

#[gpui::test]
fn a_lost_approval_reply_stays_uncertain_until_history_can_be_read(cx: &mut TestAppContext) {
    let store = held(cx);
    let (_root, vcx) = open_run(cx, &store, "keeper1");
    let daemon = store.read_with(vcx, |s, _| s.fake_daemon().unwrap());
    let reply = daemon.hold_next_approval_decision();
    daemon.fail(
        "approval.history",
        ErrorObject::new(ErrorCode::Unavailable, "history is temporarily unavailable"),
    );

    wait_drawn(vcx, |vcx| painted(vcx, "approve"));
    click(vcx, "approve");
    draw(vcx);
    assert!(
        painted(vcx, "approval-sending"),
        "show the in-flight decision"
    );
    assert_eq!(approvals(vcx, &store), 2);
    vcx.simulate_keystrokes("escape");
    reply.send(ApprovalDisconnect::AfterDecision).unwrap();

    wait_drawn(vcx, |vcx| {
        approvals(vcx, &store) == 1
            && painted(vcx, "approval-checking")
            && painted(vcx, "approval-check-again")
    });
    assert!(
        !painted(vcx, "approve"),
        "an empty pending list does not confirm the outcome"
    );
    daemon.clear_failures();
    click(vcx, "approval-check-again");
    wait_drawn(vcx, |vcx| {
        vcx.update(|_, cx| {
            crate::ui::toast::shown(cx)
                .iter()
                .any(|(label, message)| label == "Approval recorded" && message == "Approved.")
        })
    });
    assert!(painted(vcx, "composer"));
    assert!(!painted(vcx, "approval-checking"));
}

#[gpui::test]
fn reconnecting_only_reopens_a_decision_the_daemon_still_lists_as_pending(cx: &mut TestAppContext) {
    let store = held(cx);
    let (_root, vcx) = open_run(cx, &store, "keeper1");
    let daemon = store.read_with(vcx, |s, _| s.fake_daemon().unwrap());
    let reply = daemon.hold_next_approval_decision();

    wait_drawn(vcx, |vcx| painted(vcx, "approve"));
    click(vcx, "approve");
    wait_drawn(vcx, |vcx| painted(vcx, "approval-sending"));
    reply.send(ApprovalDisconnect::BeforeDecision).unwrap();

    wait_drawn(vcx, |vcx| painted(vcx, "approval-still-pending"));
    assert_eq!(
        approvals(vcx, &store),
        2,
        "reconnect did not resend the decision"
    );
    assert!(!painted(vcx, "approval-checking"));

    click(vcx, "approve");
    wait_drawn(vcx, |vcx| {
        painted(vcx, "composer") && !painted(vcx, "approval-panel")
    });
    assert_eq!(approvals(vcx, &store), 1);
}

#[gpui::test]
fn esc_does_nothing_while_the_composer_has_focus_and_denies_after_cmd_j(cx: &mut TestAppContext) {
    let store = held(cx);
    let (root, vcx) = open_run(cx, &store, "keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "approval-panel"));
    // The owner is typing: the composer stays, with its draft, and keeps the keys.
    let composer = steer(vcx, &root);
    composer.update_in(vcx, |c, window, cx| {
        c.fill("one more thing", window, cx);
        window.focus(&c.focus_handle_ref().clone(), cx);
    });
    draw(vcx);
    assert!(painted(vcx, "approval-panel") && painted(vcx, "composer"));
    vcx.simulate_keystrokes("escape");
    draw(vcx);
    assert_eq!(approvals(vcx, &store), 2, "Esc did not deny");
    assert_eq!(
        composer.read_with(vcx, |c, _| c.text().to_owned()),
        "one more thing"
    );
    vcx.simulate_keystrokes("enter");
    draw(vcx);
    assert_eq!(approvals(vcx, &store), 2, "Enter did not allow");
    // Jumping to the panel hands it the keys.
    vcx.simulate_keystrokes("cmd-j");
    draw(vcx);
    vcx.simulate_keystrokes("escape");
    wait_until(vcx, |cx| approvals(cx, &store) == 1);
    wait_drawn(vcx, |vcx| {
        store.update(vcx, |s, cx| s.load_history(cx));
        status_of(&store, vcx, "appr_keeperpush") == Some(ApprovalStatus::Denied)
    });
}

#[gpui::test]
fn a_click_on_the_panel_also_hands_it_the_keys(cx: &mut TestAppContext) {
    let store = held(cx);
    let (_root, vcx) = open_run(cx, &store, "scout1");
    wait_drawn(vcx, |vcx| painted(vcx, "approval-panel"));
    vcx.simulate_keystrokes("enter");
    draw(vcx);
    assert_eq!(approvals(vcx, &store), 2, "unclaimed: Enter does nothing");
    click(vcx, "approval-detail");
    vcx.simulate_keystrokes("enter");
    wait_until(vcx, |cx| approvals(cx, &store) == 1);
}

#[gpui::test]
fn the_permission_chip_and_plus_rows_are_on_the_fake_only(cx: &mut TestAppContext) {
    let store = held(cx);
    let (_root, vcx) = open_run(cx, &store, "keeper1");
    // Finish the pending approvals so the composer is back.
    store.update(vcx, |s, cx| s.cancel_run(run_id("keeper1"), cx));
    wait_drawn(vcx, |vcx| painted(vcx, "composer"));
    assert!(painted(vcx, "permission-chip"));
    assert!(painted(vcx, "plus"));
    click(vcx, "plus");
    wait_drawn(vcx, |vcx| painted(vcx, "plus-plan-mode"));
    assert!(painted(vcx, "plus-goal"));
    assert!(!painted(vcx, "plus-files"), "a steer box takes no files");
    click(vcx, "permission-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "permission-menu"));
    store.update(vcx, |s, cx| s.set_capabilities(Capabilities::REAL, cx));
    wait_drawn(vcx, |vcx| !painted(vcx, "permission-chip"));
    assert!(!painted(vcx, "permission-menu"));
    assert!(!painted(vcx, "plus"), "nothing to offer, so no button");
}

#[gpui::test]
fn the_permission_chip_shrinks_to_its_icon_while_a_panel_is_open(cx: &mut TestAppContext) {
    let store = held(cx);
    let (root, vcx) = open_run(cx, &store, "keeper1");
    let composer = steer(vcx, &root);
    composer.update_in(vcx, |c, window, cx| c.fill("draft", window, cx));
    wait_drawn(vcx, |vcx| {
        painted(vcx, "approval-panel") && painted(vcx, "composer")
    });
    assert!(painted(vcx, "permission-chip"));
    assert!(
        !painted(vcx, "permission-chip-label"),
        "icon only under a panel"
    );
}

#[gpui::test]
fn the_effort_popover_offers_only_the_levels_the_model_has_and_a_step_reaches_the_client(
    cx: &mut TestAppContext,
) {
    let store = held(cx);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    // Home: pick Haiku (low to high), then open the small popover.
    click(vcx, "model-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "effort-popover"));
    assert!(painted(vcx, "effort-title") && painted(vcx, "effort-model"));
    click(vcx, "effort-model");
    wait_drawn(vcx, |vcx| painted(vcx, "picker-tab-claude-code"));
    click(vcx, "picker-tab-claude-code");
    wait_drawn(vcx, |vcx| painted(vcx, "pick-model-claude-haiku-5-5"));
    click(vcx, "pick-model-claude-haiku-5-5");
    draw(vcx);
    click(vcx, "model-chip");
    draw(vcx);
    click(vcx, "model-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "effort-step-high"));
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    click(vcx, "effort-step-xhigh");
    draw(vcx);
    assert_eq!(
        composer.read_with(vcx, |c, _| c.pick().and_then(|p| p.effort)),
        None,
        "a level Haiku lacks cannot be picked"
    );
    click(vcx, "effort-step-high");
    draw(vcx);
    assert_eq!(
        composer.read_with(vcx, |c, _| c.pick().and_then(|p| p.effort)),
        Some(Effort::High)
    );
}

#[gpui::test]
fn a_step_on_a_live_run_goes_to_the_daemon(cx: &mut TestAppContext) {
    crate::screens::run::keep_composer_under_panels();
    let store = held(cx);
    let (_root, vcx) = open_run(cx, &store, "keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "model-chip"));
    click(vcx, "model-chip");
    wait_drawn(vcx, |vcx| painted(vcx, "effort-step-high"));
    click(vcx, "effort-step-high");
    wait_until(vcx, |cx| {
        store.read_with(cx, |s, _| {
            s.state().run(&run_id("keeper1")).and_then(|r| r.effort) == Some(Effort::High)
        })
    });
}

#[gpui::test]
fn on_a_new_task_files_are_real_and_plan_and_goal_are_the_fake_s(cx: &mut TestAppContext) {
    let store = held(cx);
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "plus");
    wait_drawn(vcx, |vcx| painted(vcx, "plus-files"));
    assert!(painted(vcx, "plus-plan-mode") && painted(vcx, "plus-goal"));
    store.update(vcx, |s, cx| s.set_capabilities(Capabilities::REAL, cx));
    wait_drawn(vcx, |vcx| !painted(vcx, "plus-plan-mode"));
    assert!(
        painted(vcx, "plus-files"),
        "attachments work on a real daemon"
    );
    assert!(!painted(vcx, "plus-goal"));
}

#[gpui::test]
fn the_box_on_a_run_says_work_with_the_executor(cx: &mut TestAppContext) {
    let store = held(cx);
    let (root, vcx) = open_run(cx, &store, "reviewer1");
    wait_drawn(vcx, |vcx| painted(vcx, "composer"));
    let composer = steer(vcx, &root);
    assert_eq!(
        composer.read_with(vcx, |c, cx| c.placeholder_text(cx).to_string()),
        "Work with Codex"
    );
}

fn open_note(vcx: &mut VisualTestContext) {
    wait_drawn(vcx, |vcx| painted(vcx, "approve-more"));
    click(vcx, "approve-more");
    wait_drawn(vcx, |vcx| painted(vcx, "approval-menu-note"));
    click(vcx, "approval-menu-note");
    wait_drawn(vcx, |vcx| painted(vcx, "deny-note"));
}

fn note_field(vcx: &VisualTestContext, root: &Entity<RootView>) -> Entity<Composer> {
    root.read_with(vcx, |r, cx| {
        r.run_screen()
            .unwrap()
            .read(cx)
            .note_input()
            .unwrap()
            .clone()
    })
}

fn message_of(
    store: &Entity<Store>,
    vcx: &VisualTestContext,
    approval: &str,
) -> Option<Option<String>> {
    store.read_with(vcx, |s, _| {
        s.state()
            .approval_history()
            .iter()
            .find(|r| r.id.as_str() == approval)
            .map(|r| r.message.clone())
    })
}

#[gpui::test]
fn the_split_menu_holds_the_note_on_every_daemon_and_the_scopes_on_the_fake(
    cx: &mut TestAppContext,
) {
    let store = held(cx);
    let (_root, vcx) = open_run(cx, &store, "keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "approve-more"));
    click(vcx, "approve-more");
    wait_drawn(vcx, |vcx| painted(vcx, "approval-menu-note"));
    assert!(painted(vcx, "approve-scope-1"), "the fake has the scopes");
    store.update(vcx, |s, cx| s.set_capabilities(Capabilities::REAL, cx));
    draw(vcx);
    assert!(painted(vcx, "approve-more") && painted(vcx, "approval-menu-note"));
    assert!(!painted(vcx, "approve-scope-1"), "the real daemon has not");
}

#[gpui::test]
fn a_note_goes_to_the_daemon_as_the_denial_message_and_shows_in_the_transcript(
    cx: &mut TestAppContext,
) {
    let store = held(cx);
    let (root, vcx) = open_run(cx, &store, "scout1");
    open_note(vcx);
    assert!(
        !painted(vcx, "approval-detail"),
        "the note replaces the detail"
    );
    note_field(vcx, &root).update_in(vcx, |c, window, cx| c.fill("use the mirror", window, cx));
    vcx.simulate_keystrokes("enter");
    wait_until(vcx, |cx| approvals(cx, &store) == 1);
    wait_drawn(vcx, |vcx| {
        store.update(vcx, |s, cx| s.load_history(cx));
        message_of(&store, vcx, "appr_scoutfetch") == Some(Some("use the mirror".to_owned()))
    });
    assert_eq!(
        status_of(&store, vcx, "appr_scoutfetch"),
        Some(ApprovalStatus::Denied)
    );
}

#[gpui::test]
fn esc_in_the_note_cancels_without_deciding(cx: &mut TestAppContext) {
    let store = held(cx);
    let (root, vcx) = open_run(cx, &store, "scout1");
    open_note(vcx);
    note_field(vcx, &root).update_in(vcx, |c, window, cx| c.fill("never mind", window, cx));
    vcx.simulate_keystrokes("escape");
    wait_drawn(vcx, |vcx| {
        painted(vcx, "approval-detail") && !painted(vcx, "deny-note")
    });
    assert_eq!(approvals(vcx, &store), 2, "nothing was decided");
    assert!(painted(vcx, "approve") && painted(vcx, "deny"));
}

#[gpui::test]
fn an_empty_note_is_a_plain_deny(cx: &mut TestAppContext) {
    let store = held(cx);
    let (_root, vcx) = open_run(cx, &store, "scout1");
    open_note(vcx);
    click(vcx, "note-deny");
    wait_until(vcx, |cx| approvals(cx, &store) == 1);
    wait_drawn(vcx, |vcx| {
        store.update(vcx, |s, cx| s.load_history(cx));
        status_of(&store, vcx, "appr_scoutfetch") == Some(ApprovalStatus::Denied)
    });
    assert_eq!(message_of(&store, vcx, "appr_scoutfetch"), Some(None));
}
