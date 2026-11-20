//! ROAD-31 draft: the completions never take the keyboard or the typed text, the draft is inert
//! unless it was switched on, and the real daemon keeps only what is real.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::TestAppContext;

use crate::tests::{approvals, draw, fake_store, window};
use crate::ui::composer::Composer;
use crate::ui::theme::Appearance;
use crate::vm::capabilities::Capabilities;
use crate::vm::draft31::Variant;

fn home_composer(
    cx: &mut TestAppContext,
    draft: Option<Variant>,
    caps: Capabilities,
) -> (gpui::Entity<Composer>, &mut gpui::VisualTestContext) {
    let store = fake_store(cx);
    crate::tests::wait_until(cx, |cx| approvals(cx, &store) == 2);
    store.update(cx, |s, cx| s.set_capabilities(caps, cx));
    if let Some(variant) = draft {
        cx.update(|cx| crate::ui::draft31::install(variant, cx));
    }
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    (composer, vcx)
}

fn type_in(vcx: &mut gpui::VisualTestContext, c: &gpui::Entity<Composer>, text: &str) {
    c.update_in(vcx, |c, window, cx| c.fill(text, window, cx));
    draw(vcx);
}

#[gpui::test]
fn without_the_draft_nothing_opens_and_the_text_is_plain(cx: &mut TestAppContext) {
    let (c, vcx) = home_composer(cx, None, Capabilities::FAKE);
    type_in(vcx, &c, "@src/");
    assert_eq!(c.read_with(vcx, |c, _| c.draft_labels()), None);
    c.read_with(vcx, |c, cx| {
        let theme = crate::ui::theme::current(cx);
        assert!(c.decor(&theme, cx).spans.is_empty());
    });
}

#[gpui::test]
fn at_opens_files_without_taking_focus_or_losing_the_text(cx: &mut TestAppContext) {
    let (c, vcx) = home_composer(cx, Some(Variant::A), Capabilities::FAKE);
    vcx.update(|window, cx| window.focus(&c.read(cx).focus_handle_ref().clone(), cx));
    type_in(vcx, &c, "look at @src/");
    let labels = c.read_with(vcx, |c, _| c.draft_labels()).unwrap();
    assert!(labels.iter().any(|l| l == "src/lib.rs"), "{labels:?}");
    assert_eq!(
        c.read_with(vcx, |c, _| c.text().to_owned()),
        "look at @src/"
    );
    vcx.update(|window, cx| assert!(c.read(cx).is_focused(window)));

    // Esc closes it and keeps the text; typing on in the same token does not reopen it.
    assert!(c.update(vcx, |c, cx| c.draft_dismiss(cx)));
    assert_eq!(c.read_with(vcx, |c, _| c.draft_labels()), None);
    assert_eq!(
        c.read_with(vcx, |c, _| c.text().to_owned()),
        "look at @src/"
    );
    type_in(vcx, &c, "look at @src/l");
    assert_eq!(c.read_with(vcx, |c, _| c.draft_labels()), None);
}

#[gpui::test]
fn enter_inserts_the_row_in_place_of_the_token(cx: &mut TestAppContext) {
    let (c, vcx) = home_composer(cx, Some(Variant::A), Capabilities::FAKE);
    type_in(vcx, &c, "read @src/li");
    let took = c.update_in(vcx, |c, window, cx| c.draft_accept(window, cx));
    assert!(took);
    assert_eq!(
        c.read_with(vcx, |c, _| c.text().to_owned()),
        "read @src/lib.rs "
    );
    assert_eq!(c.read_with(vcx, |c, _| c.draft_labels()), None);
}

#[gpui::test]
fn slash_lists_harness_and_app_commands_and_says_when_the_harness_cannot_be_read(
    cx: &mut TestAppContext,
) {
    let (c, vcx) = home_composer(cx, Some(Variant::B), Capabilities::FAKE);
    type_in(vcx, &c, "/");
    let labels = c.read_with(vcx, |c, _| c.draft_labels()).unwrap();
    assert!(labels.contains(&"/compact".to_owned()) && labels.contains(&"/model".to_owned()));
    // A path is not a command.
    type_in(vcx, &c, "src/main.rs");
    assert_eq!(c.read_with(vcx, |c, _| c.draft_labels()), None);
}

#[gpui::test]
fn unreadable_harness_commands_are_said_in_words_and_the_apps_still_list(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    crate::tests::wait_until(cx, |cx| approvals(cx, &store) == 2);
    store.update(cx, |s, _| s.set_commands_unreadable(true));
    cx.update(|cx| crate::ui::draft31::install(Variant::A, cx));
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let c = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    type_in(vcx, &c, "/");
    assert_eq!(
        c.read_with(vcx, |c, _| c.draft_failure()).as_deref(),
        Some("Claude Code's commands could not be read")
    );
    let labels = c.read_with(vcx, |c, _| c.draft_labels()).unwrap();
    assert!(labels.contains(&"/model".to_owned()));
}

#[gpui::test]
fn the_real_daemon_keeps_app_commands_and_nothing_mocked(cx: &mut TestAppContext) {
    let (c, vcx) = home_composer(cx, Some(Variant::A), Capabilities::REAL);
    type_in(vcx, &c, "/");
    let labels = c.read_with(vcx, |c, _| c.draft_labels()).unwrap();
    assert!(labels.contains(&"/model".to_owned()));
    assert!(!labels.contains(&"/compact".to_owned()), "{labels:?}");
    // No project to search without the place chooser, no skills without the contract.
    type_in(vcx, &c, "@src/");
    assert_eq!(c.read_with(vcx, |c, _| c.draft_labels()), None);
    type_in(vcx, &c, "$");
    assert_eq!(c.read_with(vcx, |c, _| c.draft_labels()), None);
    // No place chooser: the box has no place, so the send carries none.
    c.read_with(vcx, |c, cx| assert!(c.draft_place(cx).is_none()));
}

#[gpui::test]
fn the_store_offers_the_queue_and_places_only_with_their_capabilities(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    let run = reno_proto::RunId::from_suffix("plan1").unwrap();
    store.update(cx, |s, cx| s.set_capabilities(Capabilities::REAL, cx));
    store.read_with(cx, |s, _| {
        assert!(s.queue(&run).is_none());
        assert!(s.place_desk().is_none());
        assert!(s.harness_commands("claude-code").is_none());
        assert!(s.new_thread_files("reno", "", 5).is_none());
    });
    store.update(cx, |s, cx| s.set_capabilities(Capabilities::FAKE, cx));
    store.read_with(cx, |s, _| {
        assert!(s.queue(&run).is_some());
        assert!(s.place_desk().is_some());
        assert!(s.harness_commands("claude-code").is_some());
        assert!(!s.new_thread_files("reno", "", 5).unwrap().is_empty());
    });
}
