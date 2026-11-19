//! Slice 4 of the app shell: Home is the new-thread state (heading, chip strip, composer), and
//! what the brief replaced is gone.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::TestAppContext;

use crate::nav::Nav;
use crate::tests::{approvals, click, draw, fake_store, painted, wait_drawn, wait_until, window};
use crate::ui::theme::Appearance;

#[gpui::test]
fn with_no_project_it_asks_plainly_and_shows_the_chip_strip(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    assert!(painted(vcx, "new-thread-mark"), "the mark is centred above");
    assert!(painted(vcx, "new-thread-title"));
    assert_eq!(
        root.read_with(vcx, |r, cx| r.home().read(cx).heading().text()),
        "What should we build?"
    );
    assert!(painted(vcx, "new-thread-chip-this-mac") && painted(vcx, "new-thread-chip-more"));
    assert!(!painted(vcx, "new-thread-chip-project"));
    let composer = root.read_with(vcx, |r, cx| r.home().read(cx).composer().clone());
    assert_eq!(
        composer.read_with(vcx, |c, cx| c.placeholder_text(cx).to_string()),
        "Do anything"
    );
}

#[gpui::test]
fn a_projects_plus_opens_it_with_that_project_and_its_name_opens_the_project(
    cx: &mut TestAppContext,
) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    click(vcx, "recent-run_keeper1");
    wait_drawn(vcx, |vcx| painted(vcx, "new-thread-atlas-api"));
    click(vcx, "new-thread-atlas-api");
    draw(vcx);
    assert_eq!(
        root.read_with(vcx, |r, cx| r.home().read(cx).heading().text()),
        "What should we build in atlas-api?"
    );
    assert!(painted(vcx, "new-thread-chip-project"));
    click(vcx, "new-thread-project");
    draw(vcx);
    assert!(root.read_with(vcx, |r, _| r.project_screen().is_some()));
    assert_eq!(root.read_with(vcx, |r, _| r.nav()), Nav::Runs);
}

#[gpui::test]
fn the_held_cards_the_suggestions_and_the_need_you_strip_are_gone(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    for gone in [
        "approve-appr_keeperpush",
        "deny-appr_scoutfetch",
        "suggest-Research ACP v2",
        "review",
    ] {
        assert!(!painted(vcx, gone), "{gone} is not on Home any more");
    }
}
