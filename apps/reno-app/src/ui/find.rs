//! Find in the conversation, the parts the rest of the app reads: whether the bar is open (so
//! Escape goes to it and to nothing else), and how a match is painted.
//! The bar itself is the run screen's (`screens/run/find.rs`).

use gpui::{App, Global, HighlightStyle};

use super::theme::{Appearance, Theme};

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

/// How a match is painted. `current` is the one the bar is on. Every colour is a theme token: a
/// warning tint on each match, and a solid accent fill with the canvas as its ink on the current
/// one.
pub(crate) fn hit_style(theme: &Theme, current: bool) -> HighlightStyle {
    if current {
        HighlightStyle {
            background_color: Some(theme.accent),
            color: Some(theme.canvas),
            ..HighlightStyle::default()
        }
    } else {
        let dark = theme.appearance == Appearance::Dark;
        HighlightStyle {
            background_color: Some(theme.warning.opacity(if dark { 0.25 } else { 0.18 })),
            ..HighlightStyle::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_match_and_the_current_one_are_painted_apart_in_both_themes() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let theme = Theme::for_appearance(appearance);
            let any = hit_style(&theme, false);
            let cur = hit_style(&theme, true);
            assert_ne!(any, cur, "{appearance:?}");
            assert!(any.background_color.is_some() && cur.background_color.is_some());
        }
    }

    #[test]
    fn the_current_match_is_the_accent_with_the_canvas_for_ink() {
        let theme = Theme::for_appearance(Appearance::Light);
        let cur = hit_style(&theme, true);
        assert_eq!(cur.background_color, Some(theme.accent));
        assert_eq!(cur.color, Some(theme.canvas));
    }
}
