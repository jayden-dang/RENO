//! Toasts: short-lived notices stacked at the bottom right, built on [`notice_chip`].
//!
//! Any view can raise one through [`push`], which finds the hub the root installed. With no hub
//! (a test window without the shell) a push does nothing.

use std::time::Duration;

use gpui::{
    App, AppContext as _, Context, Entity, Global, IntoElement, Render, SharedString, Window, div,
    prelude::*, px,
};

use super::motion;
use super::notice::{NoticeChipIcon, NoticeTone, notice_chip};
use super::theme;

const LIFETIME: Duration = Duration::from_secs(4);
/// Older toasts drop off past this many, so a burst cannot grow the stack without bound.
const MAX_TOASTS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    /// A feature that is not built yet, or a request declined with nothing done: a caution.
    NotYet,
    /// Something failed.
    Error,
    /// Something happened that the screen does not show by itself.
    Info,
}

impl Tone {
    /// How the chip draws it: an error is a failure, a feature not built yet is a caution, and
    /// plain information is not a problem at all.
    pub(crate) fn notice_tone(self) -> NoticeTone {
        match self {
            Self::Error => NoticeTone::Danger,
            Self::NotYet => NoticeTone::Warning,
            Self::Info => NoticeTone::Info,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Toast {
    id: u64,
    pub(crate) tone: Tone,
    pub(crate) label: &'static str,
    pub(crate) message: SharedString,
}

#[derive(Debug, Default)]
pub(crate) struct Toasts {
    items: Vec<Toast>,
    next_id: u64,
}

struct ToastHub(Entity<Toasts>);

impl Global for ToastHub {}

/// Installs the hub; the root view calls this once and renders the entity.
pub(crate) fn install(cx: &mut App) -> Entity<Toasts> {
    let toasts = cx.new(|_| Toasts::default());
    cx.set_global(ToastHub(toasts.clone()));
    toasts
}

/// Raises a toast from anywhere.
pub(crate) fn push(
    cx: &mut App,
    tone: Tone,
    label: &'static str,
    message: impl Into<SharedString>,
) {
    let Some(hub) = cx.try_global::<ToastHub>().map(|h| h.0.clone()) else {
        return;
    };
    let message = message.into();
    hub.update(cx, |toasts, cx| toasts.push(tone, label, message, cx));
}

/// The tone and label of every toast now showing, oldest first.
#[cfg(test)]
pub(crate) fn shown_with_tone(cx: &App) -> Vec<(Tone, String)> {
    cx.try_global::<ToastHub>()
        .map(|h| {
            h.0.read(cx)
                .items
                .iter()
                .map(|t| (t.tone, t.label.to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

/// The label and message of every toast now showing, oldest first.
#[cfg(test)]
pub(crate) fn shown(cx: &App) -> Vec<(String, String)> {
    cx.try_global::<ToastHub>()
        .map(|h| {
            h.0.read(cx)
                .items
                .iter()
                .map(|t| (t.label.to_owned(), t.message.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// The words of a "not yet" toast. Every unbuilt screen says "a later phase", because the
/// current phase is the one being finished.
fn not_yet_message(what: &str) -> String {
    format!("{what} is coming in a later phase.")
}

/// "Flows is coming in a later phase" for a nav destination or action that is not built.
pub(crate) fn not_yet(cx: &mut App, what: &str) {
    push(cx, Tone::NotYet, "Not yet", not_yet_message(what));
}

impl Toasts {
    fn push(
        &mut self,
        tone: Tone,
        label: &'static str,
        message: SharedString,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(Toast {
            id,
            tone,
            label,
            message,
        });
        if self.items.len() > MAX_TOASTS {
            self.items.remove(0);
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LIFETIME).await;
            this.update(cx, |toasts, cx| toasts.dismiss(id, cx)).ok();
        })
        .detach();
    }

    pub(crate) fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        let before = self.items.len();
        self.items.retain(|t| t.id != id);
        if self.items.len() != before {
            cx.notify();
        }
    }
}

impl Render for Toasts {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let stack = div()
            .absolute()
            .right(px(24.0))
            .bottom(px(24.0))
            .w(px(340.0))
            .flex()
            .flex_col()
            .gap(px(8.0));
        stack.children(self.items.iter().map(|toast| {
            let id = toast.id;
            // The chip's wash is translucent; an opaque base keeps the toast readable over
            // whatever it covers.
            let body = div().rounded(px(12.0)).bg(theme.surface).child(notice_chip(
                &theme,
                toast.tone.notice_tone(),
                toast.label,
                toast.message.clone(),
                NoticeChipIcon::Plain,
            ));
            motion::fade_in(
                SharedString::from(format!("toast-{id}")),
                body.id(SharedString::from(format!("toast-body-{id}")))
                    .w_full()
                    .overflow_hidden()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.dismiss(id, cx))),
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::notice::NoticeTone;

    #[test]
    fn only_a_failure_looks_like_one_and_info_is_not_a_warning() {
        assert_eq!(Tone::Error.notice_tone(), NoticeTone::Danger);
        assert_eq!(Tone::NotYet.notice_tone(), NoticeTone::Warning);
        assert_eq!(Tone::Info.notice_tone(), NoticeTone::Info);
    }

    #[test]
    fn unbuilt_copy_names_no_current_phase() {
        let text = not_yet_message("Flows");
        assert_eq!(text, "Flows is coming in a later phase.");
        assert!(!text.contains("Phase 1"));
    }
}
