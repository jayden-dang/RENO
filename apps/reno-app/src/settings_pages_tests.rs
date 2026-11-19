//! Settings › Appearance, General and Accounts against the fake daemon (brief:
//! docs/design/2026-11-19-settings-pages-ui-brief.md).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};

use crate::app::RootView;
use crate::prefs::{self, AccentName, Mode, ThemeName};
use crate::store::Store;
use crate::tests::{approvals, draw, wait_until, window};
use crate::ui::theme::{self, Appearance, Palette, palette_for};

fn fake(cx: &mut TestAppContext) -> Entity<Store> {
    cx.update(crate::actions::bind_keys);
    let store = crate::tests::fake_store(cx);
    wait_until(cx, |cx| approvals(cx, &store) == 2);
    store
}

fn shown(vcx: &mut VisualTestContext) -> theme::Theme {
    vcx.update(|_, cx| theme::current(cx))
}

fn hex(c: gpui::Hsla) -> u32 {
    let c = gpui::Rgba::from(c.opacity(1.0));
    let byte = |v: f32| (v * 255.0).round() as u32;
    (byte(c.r) << 16) | (byte(c.g) << 8) | byte(c.b)
}

#[gpui::test]
fn the_saved_theme_and_accent_colour_the_whole_window(cx: &mut TestAppContext) {
    let store = fake(cx);
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    vcx.update(|_, cx| {
        prefs::update(cx, |p| {
            p.theme = ThemeName::Slate;
            p.accent = AccentName::Violet;
        });
    });
    draw(vcx);
    let want: Palette = palette_for(ThemeName::Slate, Appearance::Dark, AccentName::Violet);
    let t = shown(vcx);
    assert_eq!(hex(t.canvas), want.canvas);
    assert_eq!(hex(t.accent), want.accent);
}

#[gpui::test]
fn a_saved_mode_wins_over_the_system(cx: &mut TestAppContext) {
    let store = fake(cx);
    let (_root, vcx) = window(cx, &store, Appearance::Dark);
    vcx.update(|_, cx| prefs::update(cx, |p| p.mode = Mode::Light));
    draw(vcx);
    assert_eq!(shown(vcx).appearance, Appearance::Light);
    vcx.update(|_, cx| prefs::update(cx, |p| p.mode = Mode::System));
    draw(vcx);
    assert_eq!(shown(vcx).appearance, Appearance::Dark, "System: the window's appearance");
}

#[gpui::test]
fn toggle_theme_writes_the_mode(cx: &mut TestAppContext) {
    let store = fake(cx);
    let (root, vcx): (Entity<RootView>, _) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    root.update_in(vcx, |r, window, cx| r.toggle_theme(window, cx));
    draw(vcx);
    assert_eq!(vcx.update(|_, cx| prefs::current(cx).mode), Mode::Light);
    assert_eq!(shown(vcx).appearance, Appearance::Light);
    root.update_in(vcx, |r, window, cx| r.toggle_theme(window, cx));
    draw(vcx);
    assert_eq!(vcx.update(|_, cx| prefs::current(cx).mode), Mode::Dark);
}
