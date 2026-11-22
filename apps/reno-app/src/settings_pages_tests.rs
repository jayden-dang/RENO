//! Settings › Appearance, General and Accounts against the fake daemon (brief:
//! docs/design/2026-11-19-settings-pages-ui-brief.md).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::{Entity, TestAppContext, VisualTestContext};

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
    assert_eq!(
        shown(vcx).appearance,
        Appearance::Dark,
        "System: the window's appearance"
    );
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

// --- The pages ---------------------------------------------------------------------------

use crate::prefs::{Motion, SendWith};
use crate::tests::{click, painted};
use crate::ui::motion::{self, ReduceMotion};
use crate::vm::settings_nav::Category;

fn settings_at<'a>(
    cx: &'a mut TestAppContext,
    store: &Entity<Store>,
    page: Category,
) -> (Entity<RootView>, &'a mut VisualTestContext) {
    let (root, vcx) = window(cx, store, Appearance::Dark);
    draw(vcx);
    root.update_in(vcx, |r, window, cx| r.open_settings(page, window, cx));
    draw(vcx);
    (root, vcx)
}

fn saved(vcx: &mut VisualTestContext) -> prefs::Prefs {
    vcx.update(|_, cx| prefs::current(cx))
}

#[gpui::test]
fn appearance_saves_each_choice(cx: &mut TestAppContext) {
    let store = fake(cx);
    let (_root, vcx) = settings_at(cx, &store, Category::Appearance);
    vcx.update(|_, cx| motion::init(ReduceMotion::System, false, cx));
    for id in [
        "mode-system",
        "theme-vercel",
        "theme-reno",
        "theme-slate",
        "accent-blue",
        "accent-teal",
        "reduce-motion-system",
    ] {
        assert!(painted(vcx, id), "{id}");
    }
    click(vcx, "theme-slate");
    click(vcx, "accent-violet");
    click(vcx, "mode-light");
    click(vcx, "reduce-motion-on");
    let p = saved(vcx);
    assert_eq!(p.theme, ThemeName::Slate);
    assert_eq!(p.accent, AccentName::Violet);
    assert_eq!(p.mode, Mode::Light);
    assert_eq!(p.reduce_motion, Motion::On);
    assert_eq!(vcx.update(|_, cx| motion::preference(cx)), ReduceMotion::On);
    draw(vcx);
    assert_eq!(
        shown(vcx).appearance,
        Appearance::Light,
        "it applies at once"
    );
}

#[gpui::test]
fn general_saves_its_switches_and_the_composer_list(cx: &mut TestAppContext) {
    let store = fake(cx);
    let (_root, vcx) = settings_at(cx, &store, Category::General);
    assert!(painted(vcx, "send-enter"));
    click(vcx, "send-cmdenter");
    click(vcx, "escape-stops");
    click(vcx, "hidden-files");
    let p = saved(vcx);
    assert_eq!(p.send_with, SendWith::CommandEnter);
    assert!(p.escape_stops && !p.hidden_files);
    assert!(
        !painted(vcx, "wrap-lines"),
        "no wrap switch until the viewer can wrap"
    );
    click(vcx, "offer-claude-code");
    assert!(!store.read_with(vcx, |s, _| s.composer_prefs().offered("claude-code")));
}

#[gpui::test]
fn accounts_lists_each_account_under_the_finding(cx: &mut TestAppContext) {
    let store = fake(cx);
    let (_root, vcx) = settings_at(cx, &store, Category::Accounts);
    assert!(painted(vcx, "accounts-finding"));
    assert!(painted(vcx, "account-claude-code"));
    assert!(painted(vcx, "account-meter-claude-code"));
    assert!(!painted(vcx, "account-meter-codex"), "signed out: no meter");
}

#[gpui::test]
fn a_pinned_reduce_motion_holds_from_launch(cx: &mut TestAppContext) {
    cx.update(|cx| {
        motion::init(ReduceMotion::System, false, cx);
        prefs::update(cx, |p| p.reduce_motion = Motion::On);
        crate::screens::settings::pages::apply_saved_motion(cx);
        assert_eq!(motion::preference(cx), ReduceMotion::On);
        motion::init(ReduceMotion::On, false, cx);
        prefs::update(cx, |p| p.reduce_motion = Motion::System);
        crate::screens::settings::pages::apply_saved_motion(cx);
        assert_eq!(
            motion::preference(cx),
            ReduceMotion::On,
            "System keeps the launch preference"
        );
    });
}

#[gpui::test]
fn appearance_has_typeset_math_and_its_size_beside_the_existing_controls(cx: &mut TestAppContext) {
    let store = fake(cx);
    let (_root, vcx) = settings_at(cx, &store, Category::Appearance);
    vcx.update(|_, cx| motion::init(ReduceMotion::System, false, cx));
    for id in [
        "mode-system",
        "theme-vercel",
        "accent-blue",
        "reduce-motion-system",
        "typeset-math",
        "math-size-90",
        "math-size-100",
        "math-size-150",
    ] {
        assert!(painted(vcx, id), "{id}");
    }
    assert_eq!(saved(vcx).math_size, 100);
    assert!(saved(vcx).typeset_math);
    click(vcx, "math-size-120");
    assert_eq!(saved(vcx).math_size, 120);
    click(vcx, "reduce-motion-on");
    assert_eq!(
        saved(vcx).reduce_motion,
        Motion::On,
        "the old controls still apply"
    );
    click(vcx, "typeset-math");
    assert!(!saved(vcx).typeset_math);
    draw(vcx);
    assert!(
        !painted(vcx, "math-size-120"),
        "no size to choose while math is off"
    );
    assert_eq!(saved(vcx).math_size, 120, "the size is kept");
}
