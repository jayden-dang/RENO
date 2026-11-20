//! ROAD-31 draft, the switch: which of the three composer variants is showing. Installed only by
//! `--fake --draft31`; with no global installed [`current`] says `None` and the composer is
//! exactly what it was. ⌘⌥← and ⌘⌥→ cycle the variants, and the sidebar's lower area says which
//! one is showing as "key — name".

use gpui::{AnyElement, App, KeyBinding, actions, div, prelude::*, px};

use super::kit;
use super::theme::Theme;
use crate::vm::draft31::Variant;

actions!(draft31, [CycleNext, CyclePrev]);

struct Current(Variant);

impl gpui::Global for Current {}

/// Turns the draft on, showing `variant`, and binds ⌘⌥← / ⌘⌥→ (and Tab for a composer's
/// completions, which falls back to its normal job when none are open).
pub(crate) fn install(variant: Variant, cx: &mut App) {
    cx.set_global(Current(variant));
    cx.bind_keys([
        KeyBinding::new("cmd-alt-right", CycleNext, None),
        KeyBinding::new("cmd-alt-left", CyclePrev, None),
    ]);
    super::composer::bind_draft_keys(cx);
    cx.on_action(|_: &CycleNext, cx| cycle(true, cx));
    cx.on_action(|_: &CyclePrev, cx| cycle(false, cx));
}

/// The variant showing, or `None` when the draft is off.
pub(crate) fn current(cx: &App) -> Option<Variant> {
    cx.try_global::<Current>().map(|c| c.0)
}

/// Shows the next (or previous) variant in every window.
pub(crate) fn cycle(forward: bool, cx: &mut App) {
    if let Some(now) = current(cx) {
        let to = if forward { now.next() } else { now.prev() };
        cx.set_global(Current(to));
        cx.refresh_windows();
    }
}

/// The sidebar's note: "A — Popovers" and how to switch. `None` when the draft is off. It sits in
/// the sidebar's empty lower area, where it covers nothing.
pub(crate) fn label(theme: &Theme, cx: &App) -> Option<AnyElement> {
    let variant = current(cx)?;
    Some(
        div()
            .id("draft31-label")
            .debug_selector(|| "draft31-label".to_owned())
            .flex_none()
            .mx(px(10.0))
            .mb(px(6.0))
            .px(px(10.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .rounded(px(9.0))
            .border_1()
            .border_color(theme.hairline)
            .child(
                div()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(variant.label()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .text_size(px(11.5))
                    .text_color(theme.faint)
                    .child(kit::keycap(theme, "\u{2318}\u{2325}\u{2190}", None))
                    .child(kit::keycap(theme, "\u{2318}\u{2325}\u{2192}", None))
                    .child("switch"),
            )
            .into_any_element(),
    )
}
