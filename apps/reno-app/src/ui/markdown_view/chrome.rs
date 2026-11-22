//! The frame around a code block or a diagram: its Copy and wrap controls (TRANSCRIPT-7.1,
//! 7.2, 10.6). The frame is one family with three looks, chosen live by the ROAD-33 draft switch
//! (`ui::draft33::current`) and variant A outside it:
//!
//! - A: Copy and wrap are icon buttons at the top right, shown while the pointer is over the
//!   block (or a button has keyboard focus).
//! - B: a permanent header, the language and the line count on the left (Geist Mono 11), Copy
//!   and wrap on the right.
//! - C: a tab-like header carrying the language; Copy and wrap show on hover.
//!
//! Copy puts only the block's code on the clipboard and says so with a toast. Whether a block
//! wraps is remembered per block in a bounded store, so it survives the row scrolling away.

use std::hash::{Hash, Hasher};

use gpui::{
    AnyElement, App, ClipboardItem, ElementId, FontWeight, SharedString, Stateful, div, prelude::*,
    px,
};

use super::lru::Lru;
use crate::ui::icon::Icon;
use crate::ui::kit::{self, MONO};
use crate::ui::theme::Theme;
use crate::ui::toast;
use crate::vm::draft33::Variant;

/// Blocks whose wrap choice is remembered.
const WRAP_ENTRIES: usize = 512;

/// What a frame is around.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Kind<'a> {
    Code {
        language: Option<&'a str>,
        lines: usize,
    },
    Diagram,
}

/// A frame's contents and what its controls do.
pub(crate) struct Card<'a> {
    pub(crate) kind: Kind<'a>,
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

fn button(
    theme: &Theme,
    label: &'static str,
    glyph: Icon,
    hide_until_hover: bool,
) -> Stateful<gpui::Div> {
    let accent = theme.accent;
    let b = kit::icon_button(theme, label, glyph, 24.0);
    if hide_until_hover {
        b.opacity(0.0)
            .group_hover("md-card", |s| s.opacity(1.0))
            .focus_visible(move |s| s.opacity(1.0).border_1().border_color(accent))
    } else {
        b
    }
}

fn controls(card: &Card<'_>, theme: &Theme, hover_only: bool) -> Vec<AnyElement> {
    let code = card.copy.to_owned();
    let what = match card.kind {
        Kind::Code { .. } => "code",
        Kind::Diagram => "diagram source",
    };
    let mut out = vec![
        button(theme, "Copy code", Icon::Copy, hover_only)
            .tooltip(kit::tip("Copy"))
            .on_click(move |_, _, cx| copy(&code, what, cx))
            .into_any_element(),
    ];
    if card.wrappable {
        let key = card.key;
        let (label, tip) = if card.wrapped {
            ("Unwrap lines", "Scroll long lines")
        } else {
            ("Wrap lines", "Wrap long lines")
        };
        let mut b = button(theme, label, Icon::Wrap, hover_only)
            .tooltip(kit::tip(tip))
            .on_click(move |_, _, cx| toggle_wrap(key, cx));
        if card.wrapped {
            b = b.bg(theme.raised);
        }
        out.push(b.into_any_element());
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

/// "rust", "mermaid", or "text" for a block that names no language.
fn language_label(kind: Kind<'_>) -> SharedString {
    match kind {
        Kind::Code { language, .. } => language
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or("text")
            .to_owned()
            .into(),
        Kind::Diagram => "mermaid".into(),
    }
}

fn lines_label(kind: Kind<'_>) -> Option<String> {
    match kind {
        Kind::Code { lines, .. } => Some(format!(
            "{lines} {}",
            if lines == 1 { "line" } else { "lines" }
        )),
        Kind::Diagram => None,
    }
}

/// `body` inside the frame of `variant`.
pub(crate) fn card(
    card: &Card<'_>,
    variant: Variant,
    theme: &Theme,
    body: AnyElement,
) -> AnyElement {
    let id = ElementId::Name(format!("md-card-{}-{}", card.key, card.ordinal).into());
    let surface = theme.surface.opacity(0.92);
    match variant {
        Variant::A => frame(theme)
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
                    .children(controls(card, theme, true)),
            )
            .into_any_element(),
        Variant::B => frame(theme)
            .id(id)
            .child(
                div()
                    .debug_selector(|| "md-card-header".to_owned())
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(30.0))
                    .pl(px(14.0))
                    .pr(px(4.0))
                    .border_b_1()
                    .border_color(theme.hairline)
                    .font_family(MONO)
                    .text_size(px(11.0))
                    .text_color(theme.faint)
                    .child(div().child(language_label(card.kind)))
                    .children(lines_label(card.kind).map(|l| div().child(format!("\u{b7} {l}"))))
                    .child(div().flex_1())
                    .children(controls(card, theme, false)),
            )
            .child(body)
            .into_any_element(),
        Variant::C => frame(theme)
            .id(id)
            .child(
                div()
                    .debug_selector(|| "md-card-header".to_owned())
                    .flex()
                    .items_end()
                    .h(px(30.0))
                    .pl(px(8.0))
                    .pr(px(4.0))
                    .pt(px(6.0))
                    .bg(theme.canvas.opacity(0.5))
                    .border_b_1()
                    .border_color(theme.hairline)
                    .child(
                        div()
                            .px(px(10.0))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .rounded_t(px(6.0))
                            .bg(theme.surface)
                            .border_1()
                            .border_b_0()
                            .border_color(theme.hairline)
                            .text_size(px(12.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.muted)
                            .child(language_label(card.kind)),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(2.0))
                            .pb(px(3.0))
                            .children(controls(card, theme, true)),
                    ),
            )
            .child(body)
            .into_any_element(),
    }
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

    #[test]
    fn the_labels_say_what_the_block_is() {
        let code = Kind::Code {
            language: Some(" rust "),
            lines: 11,
        };
        assert_eq!(language_label(code), "rust");
        assert_eq!(lines_label(code).as_deref(), Some("11 lines"));
        let plain = Kind::Code {
            language: None,
            lines: 1,
        };
        assert_eq!(language_label(plain), "text");
        assert_eq!(lines_label(plain).as_deref(), Some("1 line"));
        assert_eq!(language_label(Kind::Diagram), "mermaid");
        assert_eq!(lines_label(Kind::Diagram), None);
    }
}
