//! The frame around a code block or a diagram: its Copy and wrap controls (TRANSCRIPT-7.1, 7.2,
//! 10.6). They are icon buttons at the top right, shown only while the pointer is over the block
//! (or a button has keyboard focus).
//!
//! Copy puts only the block's code on the clipboard and says so with a toast. Whether a block
//! wraps is remembered per block in a bounded store, so it survives the row scrolling away.

use std::hash::{Hash, Hasher};

use gpui::{AnyElement, App, ClipboardItem, ElementId, Stateful, div, prelude::*, px};

use super::lru::Lru;
use crate::ui::icon::Icon;
use crate::ui::kit;
use crate::ui::theme::Theme;
use crate::ui::toast;

/// Blocks whose wrap choice is remembered.
const WRAP_ENTRIES: usize = 512;

/// What a frame is around.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Kind {
    Code,
    Diagram,
}

/// A frame's contents and what its controls do.
pub(crate) struct Card<'a> {
    pub(crate) kind: Kind,
    /// What Copy puts on the clipboard.
    pub(crate) copy: &'a str,
    /// Identifies the block for its remembered wrap choice.
    pub(crate) key: u64,
    /// Show a wrap toggle (a diagram has none).
    pub(crate) wrappable: bool,
    pub(crate) wrapped: bool,
    /// Which block of its draw this is: two identical blocks share a `key` (and a wrap choice)
    /// but not an element id.
    pub(crate) ordinal: usize,
}

struct Wraps(Lru<u64, bool>);

impl gpui::Global for Wraps {}

impl Default for Wraps {
    fn default() -> Self {
        Self(Lru::new(WRAP_ENTRIES, usize::MAX))
    }
}

/// A key for a block: its kind, language and text.
pub(crate) fn key_of(language: Option<&str>, text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    language.hash(&mut hasher);
    text.hash(&mut hasher);
    hasher.finish()
}

/// Whether the block `key` wraps its long lines. Unwrapped (scrolling sideways) until the owner
/// says otherwise.
pub(crate) fn wraps(key: u64, cx: &mut App) -> bool {
    cx.default_global::<Wraps>()
        .0
        .get(&key)
        .copied()
        .unwrap_or(false)
}

/// Flips the block's wrap choice and redraws.
pub(crate) fn toggle_wrap(key: u64, cx: &mut App) {
    let wraps = cx.default_global::<Wraps>();
    let now = wraps.0.get(&key).copied().unwrap_or(false);
    wraps.0.insert(key, !now, 1);
    super::settled(cx);
}

/// Puts `text` on the clipboard and says so.
pub(crate) fn copy(text: &str, what: &'static str, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
    toast::push(cx, toast::Tone::Info, "Copied", what);
}

/// A control that shows only while the pointer is over the block or it has keyboard focus.
fn button(theme: &Theme, label: &'static str, glyph: Icon) -> Stateful<gpui::Div> {
    let accent = theme.accent;
    kit::icon_button(theme, label, glyph, 24.0)
        .opacity(0.0)
        .group_hover("md-card", |s| s.opacity(1.0))
        .focus_visible(move |s| s.opacity(1.0).border_1().border_color(accent))
}

fn controls(card: &Card<'_>, theme: &Theme) -> Vec<Stateful<gpui::Div>> {
    let code = card.copy.to_owned();
    let what = match card.kind {
        Kind::Code => "code",
        Kind::Diagram => "diagram source",
    };
    let mut out = vec![
        button(theme, "Copy code", Icon::Copy)
            .tooltip(kit::tip("Copy"))
            .on_click(move |_, _, cx| copy(&code, what, cx)),
    ];
    if card.wrappable {
        let key = card.key;
        let (label, tip) = if card.wrapped {
            ("Unwrap lines", "Scroll long lines")
        } else {
            ("Wrap lines", "Wrap long lines")
        };
        let mut b = button(theme, label, Icon::Wrap)
            .tooltip(kit::tip(tip))
            .on_click(move |_, _, cx| toggle_wrap(key, cx));
        if card.wrapped {
            b = b.bg(theme.raised);
        }
        out.push(b);
    }
    out
}

fn frame(theme: &Theme) -> gpui::Div {
    div()
        .group("md-card")
        .debug_selector(|| "md-card".to_owned())
        .relative()
        .w_full()
        .min_w_0()
        .rounded(px(10.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
        .overflow_hidden()
}

/// `body` inside its frame, with the controls at the top right.
pub(crate) fn card(card: &Card<'_>, theme: &Theme, body: AnyElement) -> AnyElement {
    let id = ElementId::Name(format!("md-card-{}-{}", card.key, card.ordinal).into());
    let surface = theme.surface.opacity(0.92);
    frame(theme)
        .id(id)
        .child(body)
        .child(
            div()
                .absolute()
                .top(px(6.0))
                .right(px(6.0))
                .flex()
                .gap(px(2.0))
                .p(px(1.0))
                .rounded(px(9.0))
                .group_hover("md-card", move |s| s.bg(surface))
                .children(controls(card, theme)),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn wrap_is_off_until_chosen_remembered_per_block_and_bounded(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let key = key_of(Some("rust"), "fn a() {}");
            assert!(!wraps(key, cx));
            toggle_wrap(key, cx);
            assert!(wraps(key, cx));
            assert!(!wraps(key_of(Some("rust"), "fn b() {}"), cx), "per block");
            toggle_wrap(key, cx);
            assert!(!wraps(key, cx));
            for n in 0..(WRAP_ENTRIES + 50) {
                toggle_wrap(key_of(None, &n.to_string()), cx);
            }
            assert!(cx.default_global::<Wraps>().0.len() <= WRAP_ENTRIES);
        });
    }

    /// Copy and Wrap are hidden until the pointer is over the block: at rest they have no
    /// opacity. A diagram has the same Copy, and no Wrap.
    #[test]
    fn the_controls_are_hidden_at_rest_on_a_code_block_and_a_diagram() {
        let theme = Theme::for_appearance(crate::ui::theme::Appearance::Dark);
        let card = |kind, wrappable| Card {
            kind,
            copy: "x",
            key: 1,
            wrappable,
            wrapped: false,
            ordinal: 0,
        };
        let code = controls(&card(Kind::Code, true), &theme);
        let diagram = controls(&card(Kind::Diagram, false), &theme);
        assert_eq!((code.len(), diagram.len()), (2, 1), "Copy and Wrap; Copy");
        for mut control in code.into_iter().chain(diagram) {
            assert_eq!(control.style().opacity, Some(0.0));
        }
    }
}
