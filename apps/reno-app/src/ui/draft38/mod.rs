//! ROAD-38 draft, the switch and the pieces: which of the three first-run variants is showing,
//! and what each draws. Installed only by `--fake --draft38`; with no global installed [`on`]
//! says no and the app is exactly what it was. ⌘⌥← and ⌘⌥→ cycle the variants, and the status
//! bar's middle says which one is showing as "A \u{2014} Wizard", where it covers nothing and
//! every setup surface leaves it alone.
//!
//! - `flow`: the setup entity: where a person is, and what each click does.
//! - `parts`: what the three variants share (an agent's line, the look, the projects).
//! - `wizard`, `checklist`, `page`: setup as A, B and C.
//! - `gate`: the connecting gate as A, B and C.
//! - `restart`: the restart affordance and its confirm as A, B and C.

pub(crate) mod checklist;
pub(crate) mod flow;
pub(crate) mod gate;
pub(crate) mod page;
pub(crate) mod parts;
pub(crate) mod restart;
pub(crate) mod wizard;

use gpui::{AnyElement, App, KeyBinding, actions, div, prelude::*, px};

use super::kit;
use super::theme::Theme;
use crate::vm::draft38::Variant;

actions!(draft38, [CycleNext, CyclePrev, Dismiss]);

/// The key context the root view carries while the restart panel is open, so Escape reaches
/// [`Dismiss`] whatever has the focus.
pub(crate) const OPEN_CONTEXT: &str = "Draft38Open";

/// What the launch hooks asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Hooks {
    /// `RENO_DRAFT38_FIRST_RUN`: show setup even if it was done, from this step or item.
    pub(crate) first_run: Option<String>,
    /// `RENO_DRAFT38_GATE=hold`: the connecting gate stays up.
    pub(crate) gate_hold: bool,
    /// `RENO_DRAFT38_IMPORT=<path>`: the folder panel is taken to answer with that path once
    /// setup is up (screenshots, reviews).
    pub(crate) import: Option<String>,
}

struct Current(Variant);

impl gpui::Global for Current {}

impl gpui::Global for Hooks {}

/// Turns the draft on, showing `variant`, and binds its keys.
pub(crate) fn install(variant: Variant, hooks: Hooks, cx: &mut App) {
    cx.set_global(Current(variant));
    cx.set_global(hooks);
    cx.bind_keys([
        KeyBinding::new("cmd-alt-right", CycleNext, None),
        KeyBinding::new("cmd-alt-left", CyclePrev, None),
        KeyBinding::new("escape", Dismiss, Some(OPEN_CONTEXT)),
    ]);
    cx.on_action(|_: &CycleNext, cx| cycle(true, cx));
    cx.on_action(|_: &CyclePrev, cx| cycle(false, cx));
}

/// The variant showing, or `None` when the draft is off.
pub(crate) fn current(cx: &App) -> Option<Variant> {
    cx.try_global::<Current>().map(|c| c.0)
}

/// Whether the draft is on.
pub(crate) fn on(cx: &App) -> bool {
    current(cx).is_some()
}

/// What the launch hooks asked for.
pub(crate) fn hooks(cx: &App) -> Hooks {
    cx.try_global::<Hooks>().cloned().unwrap_or_default()
}

/// Shows the next (or previous) variant in every window.
pub(crate) fn cycle(forward: bool, cx: &mut App) {
    if let Some(now) = current(cx) {
        let to = if forward { now.next() } else { now.prev() };
        cx.set_global(Current(to));
        cx.refresh_windows();
    }
}

/// Shows `variant` (tests).
#[cfg(test)]
pub(crate) fn show(variant: Variant, cx: &mut App) {
    cx.set_global(Current(variant));
}

/// The switch's note: "A \u{2014} Wizard" and the keys. `None` when the draft is off. It sits in
/// the status bar's middle, which is empty in every state.
pub(crate) fn label(theme: &Theme, cx: &App) -> Option<AnyElement> {
    let variant = current(cx)?;
    Some(
        div()
            .id("draft38-label")
            .debug_selector(|| "draft38-label".to_owned())
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
