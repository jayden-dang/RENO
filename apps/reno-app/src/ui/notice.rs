//! Notice chip: a tinted card for failures and warnings, used inline and as a toast body.
//!
//! The icon slots are drawn as text glyphs and the copy button is a text label, because the
//! source's icon set and tooltip widget are not ported.

#![allow(dead_code)] // Reno's own screens adopt this; only the spike draws it today.

use gpui::{AnyElement, Div, FontWeight, SharedString, div, prelude::*, px};

use super::theme::Theme;

/// The chip's header icon treatment, which also picks its metrics: the bare
/// triangle reads lighter and rem-scales with the composer's text; the tile
/// anchors the transcript's block chip at fixed metrics.
pub(crate) enum NoticeChipIcon {
    /// Bare 14px triangle in the label color; 12px inset/radius, rem-scaled
    /// 12px text on a 16px line height (the composer's inline notice).
    Plain,
    /// 20px accent-washed tile holding a 12px triangle; 10px inset/radius,
    /// fixed 12px text (the transcript's ErrorChip).
    Tile,
}

/// What a chip reports, which picks its colour and header mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoticeTone {
    /// A failure: the danger colour and a "!".
    Danger,
    /// Something to watch: the warning colour and a "!".
    Warning,
    /// Something that went fine: the success colour and a check.
    Info,
}

impl NoticeTone {
    fn color(self, theme: &Theme) -> gpui::Hsla {
        match self {
            Self::Danger => theme.danger,
            Self::Warning => theme.warning,
            Self::Info => theme.success,
        }
    }

    fn mark(self) -> &'static str {
        match self {
            Self::Danger | Self::Warning => "!",
            Self::Info => "\u{2713}",
        }
    }
}

/// The failure notice: a tinted rounded chip, a 16% border over a 5% wash of the accent (the
/// tile variant: a borderless 10% wash), never a bare stroke. It has a header row (icon and
/// medium label, with a tiny copy button pinned to its top-right corner, because failure
/// payloads are meant to be pasted) and the message below. The message wraps instead of
/// truncating: failure payloads carry exit statuses and stderr. `tone` picks the colour and mark. Callers chain their own chrome (an id and dismiss click,
/// `w_full().overflow_hidden()`).
pub(crate) fn notice_chip(
    theme: &Theme,
    tone: NoticeTone,
    label: &'static str,
    message: impl Into<SharedString>,
    icon: NoticeChipIcon,
) -> Div {
    notice_chip_with_action(theme, tone, label, message, icon, None)
}

/// [`notice_chip`] with an action under the message (the transcript's
/// "Log in").
pub(crate) fn notice_chip_with_action(
    theme: &Theme,
    tone: NoticeTone,
    label: &'static str,
    message: impl Into<SharedString>,
    icon: NoticeChipIcon,
    action: Option<AnyElement>,
) -> Div {
    let accent = tone.color(theme);
    let muted = theme.soften(accent);
    let mark = tone.mark();
    let message = message.into();
    let copy_message = message.clone();
    let tile = matches!(icon, NoticeChipIcon::Tile);
    // The transcript's block chip reads as an alert: a stronger red wash,
    // the message in the lighter red.
    let wash = if tile { 0.1 } else { 0.05 };
    let frame = |inset: f32| {
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .rounded(px(inset))
            // The transcript's alert is borderless: the wash alone.
            .when(!tile, |chip| {
                chip.border_1().border_color(accent.opacity(0.16))
            })
            .bg(accent.opacity(wash))
            .px(px(inset))
            .py(px(8.0))
    };
    let chip = if tile {
        frame(10.0).text_size(px(12.0))
    } else {
        frame(12.0)
            .text_size(px(12.0))
            .line_height(px(16.0))
            .text_color(muted.opacity(0.9))
    };
    let header_icon: AnyElement = if tile {
        div()
            .flex_none()
            .size(px(20.0))
            .rounded(px(6.0))
            .bg(accent.opacity(0.12))
            .flex()
            .items_center()
            .justify_center()
            .child(glyph(mark, 12.0, muted.opacity(0.8)))
            .into_any_element()
    } else {
        glyph(mark, 14.0, muted.opacity(0.9)).into_any_element()
    };
    // The tile pins its own label/message colors; the plain icon inherits the
    // chip-wide muted accent.
    let copy = div()
        .id("notice-copy")
        .flex_none()
        .h(px(20.0))
        .px(px(6.0))
        .rounded(px(6.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(move |s| s.bg(accent.opacity(0.12)))
        // The composer chip dismisses on click; copying must
        // not take the notice with it.
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy_message.to_string()));
        })
        .child(
            div()
                .text_size(px(10.0))
                .text_color(muted.opacity(0.8))
                .child("Copy"),
        );
    let header = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(header_icon)
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .when(tile, |label| label.text_color(muted.opacity(0.8)))
                .child(label),
        )
        .child(div().flex_1());
    let message = div()
        .min_w_0()
        .w_full()
        .when(tile, |message| message.text_color(muted))
        .child(message);
    // Copy pinned to the header's top-right corner; an action goes under
    // the message.
    chip.child(header.child(copy))
        .child(message)
        .when_some(action, |chip, action| {
            chip.child(div().pt(px(2.0)).flex().child(action))
        })
}

/// A one-character glyph on a fixed square, standing in for an icon.
fn glyph(text: &'static str, size: f32, color: gpui::Hsla) -> Div {
    div()
        .size(px(size))
        .flex()
        .items_center()
        .justify_center()
        .font_weight(FontWeight::BOLD)
        .text_size(px(size))
        .line_height(px(size))
        .text_color(color)
        .child(text)
}
