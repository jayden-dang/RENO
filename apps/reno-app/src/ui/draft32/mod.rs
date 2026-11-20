//! ROAD-32 draft, the switch and the pieces: which of the three sidebar variants is showing, and
//! what each draws. Installed only by `--fake --draft32`; with no global installed [`current`]
//! says `None` and the sidebar is exactly what it was. ⌘⌥← and
//! ⌘⌥→ cycle the variants, and the sidebar's lower area says which one is
//! showing as "key — name".
//!
//! - `state`: the interaction state (menu, rename, confirm, card, filters) and what each act does.
//! - `menu`: the ONE menu component A and B share.
//! - `list`: the thread rows, the pinned section and strip, the archived list, "Show N more".
//! - `card`: C's details card and the archived page.

mod card;
mod list;
mod menu;
mod state;

use gpui::{AnyElement, App, KeyBinding, actions, div, prelude::*, px};

pub(crate) use card::{archived_page, overlays};
pub(crate) use list::{Draft, SidebarDrag, folder_extras, folder_row, resize_handle, section};
pub(crate) use menu::ActHandler;
pub(crate) use state::{Act, DEFAULT_WIDTH, MAX_WIDTH, MIN_WIDTH, State};

use super::kit;
use super::theme::Theme;
use crate::vm::draft32::Variant;

actions!(
    draft32,
    [CycleNext, CyclePrev, Dismiss, PinCurrent, ArchiveCurrent]
);

/// The key context the root view carries while a menu, card, confirm or field of the draft is
/// open, so Escape reaches [`Dismiss`] whatever has the focus.
pub(crate) const OPEN_CONTEXT: &str = "Draft32Open";

struct Current(Variant);

/// Whether the palette offers "Archived \u{b7} N" (C only). A flag, not a global lookup, because
/// the palette's command list is built where no window is at hand.
static PALETTE_ARCHIVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn set_current(variant: Variant, cx: &mut App) {
    cx.set_global(Current(variant));
    PALETTE_ARCHIVED.store(variant == Variant::C, std::sync::atomic::Ordering::Relaxed);
}

/// The palette's "Archived threads" command, while variant C shows.
pub(crate) fn palette_entry() -> Option<super::palette::Entry> {
    PALETTE_ARCHIVED
        .load(std::sync::atomic::Ordering::Relaxed)
        .then_some(super::palette::Entry {
            label: "Archived threads",
            hint: "Go to",
            icon: super::icon::Icon::Inbox,
            action: super::palette::PaletteAction::Archived,
        })
}

impl gpui::Global for Current {}

/// Turns the draft on, showing `variant`, and binds its keys.
pub(crate) fn install(variant: Variant, cx: &mut App) {
    set_current(variant, cx);
    cx.bind_keys([
        KeyBinding::new("cmd-alt-right", CycleNext, None),
        KeyBinding::new("cmd-alt-left", CyclePrev, None),
        KeyBinding::new("escape", Dismiss, Some(OPEN_CONTEXT)),
        KeyBinding::new("cmd-shift-p", PinCurrent, None),
        KeyBinding::new("cmd-shift-a", ArchiveCurrent, None),
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
        set_current(to, cx);
        cx.refresh_windows();
    }
}

/// The sidebar's note: "A — Menu" and how to switch. `None` when the draft is off. It sits
/// in the sidebar's lower area, between the list and Settings, where it covers nothing.
pub(crate) fn label(theme: &Theme, cx: &App) -> Option<AnyElement> {
    let variant = current(cx)?;
    Some(
        div()
            .id("draft32-label")
            .debug_selector(|| "draft32-label".to_owned())
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
