//! The app-wide "an overlay is open" signal: a menu, a picker or the palette is showing over
//! the window. Nothing reads it yet; the browser will, because a native surface paints above
//! any GPUI element and must step aside while one is open.
//!
//! It is keyed by owner, so one closing never clears another that is still open, and an owner
//! that goes away with its menu open must call [`release`] (a dropped composer does).

use std::collections::HashSet;

use gpui::{App, EntityId, Global};

#[derive(Default)]
struct Open(HashSet<EntityId>);

impl Global for Open {}

/// Says whether `owner` has an overlay open now. Idempotent.
pub(crate) fn set(cx: &mut App, owner: EntityId, open: bool) {
    let owners = &mut cx.default_global::<Open>().0;
    if open {
        owners.insert(owner);
    } else {
        owners.remove(&owner);
    }
}

/// `owner` is gone: whatever it had open is not any more.
pub(crate) fn release(cx: &mut App, owner: EntityId) {
    set(cx, owner, false);
}

/// Whether any overlay is open.
#[allow(dead_code)] // The browser surface reads it; nothing does yet.
pub(crate) fn is_open(cx: &App) -> bool {
    cx.try_global::<Open>().is_some_and(|o| !o.0.is_empty())
}
