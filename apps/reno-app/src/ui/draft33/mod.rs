//! ROAD-33 draft, the switch: which of the three transcript variants is showing. Installed only
//! by `--fake --draft33`; with no global installed [`current`] says none and the app is exactly
//! what it was. Cmd-Alt-Right and Cmd-Alt-Left cycle the variants, and the status bar's middle
//! says which one is showing as "A — Quiet strip", with the keys.

use gpui::{AnyElement, App, KeyBinding, actions, div, prelude::*, px};

use super::kit;
use super::theme::Theme;
use crate::vm::draft33::Variant;

actions!(draft33, [CycleNext, CyclePrev]);

struct Current(Variant);

impl gpui::Global for Current {}

/// Turns the draft on, showing `variant`, and binds its keys.
pub(crate) fn install(variant: Variant, cx: &mut App) {
    cx.set_global(Current(variant));
    cx.bind_keys([
        KeyBinding::new("cmd-alt-right", CycleNext, None),
        KeyBinding::new("cmd-alt-left", CyclePrev, None),
    ]);
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

/// The switch's note: "A — Quiet strip" and the keys. `None` when the draft is off. It sits
/// in the status bar's middle.
pub(crate) fn label(theme: &Theme, cx: &App) -> Option<AnyElement> {
    let variant = current(cx)?;
    Some(
        div()
            .id("draft33-label")
            .debug_selector(|| "draft33-label".to_owned())
            .flex_none()
            .h(px(26.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(999.0))
            .border_1()
            .border_color(theme.hairline)
            .text_size(px(12.0))
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(variant.label()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(3.0))
                    .text_color(theme.faint)
                    .child(kit::keycap(theme, "\u{2318}\u{2325}\u{2190}", None))
                    .child(kit::keycap(theme, "\u{2318}\u{2325}\u{2192}", None)),
            )
            .into_any_element(),
    )
}
