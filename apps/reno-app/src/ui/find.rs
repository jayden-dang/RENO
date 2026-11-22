//! Find in the conversation, the parts the rest of the app reads: whether the bar is open (so
//! Escape goes to it and to nothing else), and how a match is painted in each draft variant.
//! The bar itself is the run screen's (`screens/run/find.rs`).

use gpui::{App, FontWeight, Global, HighlightStyle, UnderlineStyle, px};

use super::theme::{Appearance, Theme};
use crate::vm::draft33::Variant;

/// The key context the root view carries while the find bar is open.
pub(crate) const OPEN_CONTEXT: &str = "FindOpen";
/// The key context of the bar itself.
pub(crate) const BAR_CONTEXT: &str = "FindBar";

struct BarOpen(bool);

impl Global for BarOpen {}

/// Whether a find bar is open in this app. While it is, Escape closes it and does nothing else.
pub(crate) fn is_open(cx: &App) -> bool {
    cx.try_global::<BarOpen>().is_some_and(|b| b.0)
}

pub(crate) fn set_open(cx: &mut App, open: bool) {
    cx.set_global(BarOpen(open));
}

/// How a match is painted. `current` is the one the bar is on. Every colour is a theme token.
///
/// - A: a warning tint, and the current one a solid accent fill with the canvas as its ink.
/// - B: the accent spent on navigation: an accent underline and a faint accent fill on every
///   match, and a solid accent fill on the current one.
/// - C: neutral: a raised fill, and the current one bold with an accent underline.
pub(crate) fn hit_style(theme: &Theme, variant: Variant, current: bool) -> HighlightStyle {
    let dark = theme.appearance == Appearance::Dark;
    let underline = UnderlineStyle {
        thickness: px(1.0),
        color: Some(theme.accent),
        wavy: false,
    };
    match (variant, current) {
        (Variant::A, false) => HighlightStyle {
            background_color: Some(theme.warning.opacity(if dark { 0.25 } else { 0.18 })),
            ..HighlightStyle::default()
        },
        (Variant::B, false) => HighlightStyle {
            background_color: Some(theme.accent.opacity(0.14)),
            underline: Some(underline),
            ..HighlightStyle::default()
        },
        (Variant::C, false) => HighlightStyle {
            background_color: Some(theme.raised),
            ..HighlightStyle::default()
        },
        (Variant::A | Variant::B, true) => HighlightStyle {
            background_color: Some(theme.accent),
            color: Some(theme.canvas),
            ..HighlightStyle::default()
        },
        (Variant::C, true) => HighlightStyle {
            background_color: Some(theme.raised),
            font_weight: Some(FontWeight::BOLD),
            underline: Some(underline),
            ..HighlightStyle::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_paints_a_match_and_the_current_one_apart_in_both_themes() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let theme = Theme::for_appearance(appearance);
            for variant in [Variant::A, Variant::B, Variant::C] {
                let any = hit_style(&theme, variant, false);
                let cur = hit_style(&theme, variant, true);
                assert_ne!(any, cur, "{variant:?} {appearance:?}");
                assert!(any.background_color.is_some() && cur.background_color.is_some());
            }
        }
    }

    #[test]
    fn the_current_match_in_a_and_b_is_the_accent_with_the_canvas_for_ink() {
        let theme = Theme::for_appearance(Appearance::Light);
        for variant in [Variant::A, Variant::B] {
            let cur = hit_style(&theme, variant, true);
            assert_eq!(cur.background_color, Some(theme.accent));
            assert_eq!(cur.color, Some(theme.canvas));
        }
    }
}
