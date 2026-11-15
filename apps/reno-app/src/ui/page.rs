//! The pieces every list page shares: the header band, the lede, the daemon banner, the
//! empty / loading / failed note, and table rows. Pages sit directly on the `canvas` panel with
//! no wrapping box (UI brief); only tables and cards are solid raised surfaces.

use std::rc::Rc;

use gpui::{App, Div, FontWeight, SharedString, Stateful, Window, div, prelude::*, px};

use super::icon::{Icon, icon};
use super::kit;
use super::theme::Theme;
use crate::vm::{Banner, Section};

/// Width of a narrow page's column (Inbox).
pub(crate) const NARROW: f32 = 760.0;
/// Width of a wide page's column (tables).
pub(crate) const WIDE: f32 = 1_040.0;

/// A click handler that needs no event.
pub(crate) type Action = Rc<dyn Fn(&mut Window, &mut App)>;

/// The 54 px header: the title (and crumbs) on the left, `trailing` on the right.
pub(crate) fn header(
    theme: &Theme,
    title: impl Into<SharedString>,
    trailing: impl IntoElement,
) -> Div {
    div()
        .flex_none()
        .h(px(54.0))
        .px(px(22.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .border_b_1()
        .border_color(theme.hairline)
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(14.5))
                .child(title.into()),
        )
        .child(div().flex_1())
        .child(trailing)
}

/// A scrolling area that centres its content: put [`content`] inside it.
pub(crate) fn column(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .items_center()
        .px(px(24.0))
        .py(px(26.0))
}

/// The column of at most `width` px that holds the page's content.
pub(crate) fn content(width: f32) -> Div {
    div()
        .w_full()
        .max_w(px(width))
        .flex()
        .flex_col()
        .gap(px(22.0))
}

/// The lede: a 20 px headline and one muted line.
pub(crate) fn lede(
    theme: &Theme,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .text_size(px(20.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(
            div()
                .text_size(px(13.5))
                .line_height(px(21.0))
                .text_color(theme.muted)
                .child(detail.into()),
        )
}

/// A small section label: "HELD ACTIONS".
pub(crate) fn section_label(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(11.5))
        .text_color(theme.faint)
        .pb(px(2.0))
        .child(text.into())
}

/// The strip shown while the daemon is not connected: why, whether it retries, and a retry
/// button.
pub(crate) fn banner(theme: &Theme, banner: &Banner, retry: Option<Action>) -> Stateful<Div> {
    let tone = if banner.connecting {
        theme.muted
    } else {
        theme.warning
    };
    div()
        .id("daemon-banner")
        .w_full()
        .flex()
        .items_center()
        .gap(px(12.0))
        .px(px(14.0))
        .py(px(10.0))
        .rounded(px(12.0))
        .bg(theme.surface)
        .border_1()
        .border_color(tone.opacity(0.4))
        .child(kit::dot(tone, 8.0))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(13.5))
                        .text_color(theme.text)
                        .child(banner.reason.clone()),
                )
                .when_some(banner.retry.clone(), |col, line| {
                    col.child(
                        div()
                            .text_size(px(12.5))
                            .text_color(theme.muted)
                            .child(line),
                    )
                }),
        )
        .when_some(retry.filter(|_| !banner.connecting), |row, retry| {
            row.child(
                kit::pill_button(theme, "retry-now", "Retry now", kit::Tone::Quiet)
                    .on_click(move |_, window, cx| retry(window, cx)),
            )
        })
}

/// What a list says when it has no rows: loading, nothing yet, unavailable or failed.
/// `empty` is the sentence for a list that loaded and is empty.
pub(crate) fn note<T>(theme: &Theme, section: &Section<T>, empty: &str) -> Option<Div> {
    let (glyph, tone, text) = match section {
        Section::Rows(_) => return None,
        Section::Loading => (Icon::Clock, theme.faint, "Loading…".to_owned()),
        Section::Empty => (Icon::Check, theme.faint, empty.to_owned()),
        Section::Unavailable => (
            Icon::Lock,
            theme.faint,
            "This daemon does not offer this yet. Update the daemon to use it.".to_owned(),
        ),
        Section::Failed(reason) => (
            Icon::X,
            theme.danger,
            format!("Could not load this: {reason}"),
        ),
    };
    Some(
        div()
            .flex()
            .items_center()
            .gap(px(9.0))
            .py(px(10.0))
            .text_size(px(13.5))
            .text_color(theme.muted)
            .child(icon(glyph, 15.0, tone))
            .child(text),
    )
}

/// A solid table: hairline border, `surface` fill, rounded.
pub(crate) fn table() -> Div {
    div().w_full().flex().flex_col().overflow_hidden()
}

/// The frame of a table, applied by [`table_frame`].
pub(crate) fn table_frame(theme: &Theme, table: Div) -> Div {
    table
        .rounded(px(12.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
}

/// A table's header row: faint labels in the given cell widths (`None` flexes).
pub(crate) fn table_head(theme: &Theme, cols: &[(&'static str, Option<f32>)]) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(12.0))
        .px(px(16.0))
        .h(px(34.0))
        .border_b_1()
        .border_color(theme.hairline)
        .text_size(px(12.0))
        .text_color(theme.faint)
        .children(
            cols.iter()
                .map(|(label, width)| cell(div(), *width).child(*label)),
        )
}

/// One cell of `width` px, or a flexing cell.
pub(crate) fn cell(cell: Div, width: Option<f32>) -> Div {
    match width {
        Some(w) => cell.flex_none().w(px(w)),
        None => cell.flex_1().min_w_0(),
    }
}

/// A table row: the base styling; the caller adds cells, an id and a click handler.
pub(crate) fn table_row(theme: &Theme, last: bool) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(12.0))
        .px(px(16.0))
        .py(px(10.0))
        .text_size(px(13.5))
        .when(!last, |row| row.border_b_1().border_color(theme.hairline))
}
