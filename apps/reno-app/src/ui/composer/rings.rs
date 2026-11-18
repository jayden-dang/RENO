//! The rings: the context window beside the chip on a run's composer, and the account's plan
//! usage in the picker's Account row, each with the card its click opens.

use gpui::{Context, canvas, prelude::*};

use super::menu::card;
use super::{Composer, Mode};
use crate::ui::ring::{ring_chip, tone};
use crate::ui::theme::Theme;
use crate::vm::{context, models};

impl Composer {
    /// The context ring of a run's foot: "42%" and a ring, hidden until the harness has
    /// reported a window.
    pub(super) fn context_ring(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let Mode::Steer(run) = &self.mode else {
            return None;
        };
        let usage = models::context_usage(self.store.read(cx).state(), run);
        if !context::has_window(usage) {
            return None;
        }
        let fraction = usage.fraction();
        let level = fraction.map_or(context::Level::Muted, context::context_level);
        let label = fraction.map_or_else(|| "\u{2014}".to_owned(), context::percent);
        let bounds = self.ring_bounds.clone();
        Some(
            ring_chip(
                "context-ring",
                fraction.unwrap_or(0.0) as f32,
                tone(theme, level),
                label,
                self.context_card_open,
                theme,
            )
            .relative()
            .child(
                canvas(move |b, _, _| bounds.set(Some(b)), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.context_card_open = !this.context_card_open;
                this.class_open = false;
                this.close_picker(cx);
                cx.notify();
            })),
        )
    }

    /// The context card: tokens used, the window and what remains.
    pub(super) fn context_card(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Deferred> {
        let Mode::Steer(run) = &self.mode else {
            return None;
        };
        let usage = models::context_usage(self.store.read(cx).state(), run);
        let lines: Vec<String> = context::details(usage).lines().map(str::to_owned).collect();
        let content = card(theme, "context-card", "Context window", &lines);
        let at = self.ring_bounds.clone();
        self.popover(
            &at,
            content,
            |this, cx| {
                this.context_card_open = false;
                cx.notify();
            },
            None,
            cx,
        )
    }

    /// The Account row's usage ring: the most-used meter, for a signed-in harness the daemon has
    /// meters for. A click folds the meters out under the row.
    pub(super) fn usage_ring(
        &self,
        theme: &Theme,
        harness: &models::HarnessChoice,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        if !matches!(harness.account, models::Account::SignedIn(_)) {
            return None;
        }
        let usage = harness.usage.as_ref()?;
        let fraction = usage.used_fraction()?;
        let executor = harness.id.clone();
        Some(
            ring_chip(
                "picker-usage",
                fraction,
                tone(theme, context::usage_ring_level(fraction)),
                context::percent(f64::from(fraction)),
                self.usage_card_open,
                theme,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.usage_card_open = !this.usage_card_open;
                if this.usage_card_open {
                    // Looked at, so read again (the daemon reads at most once per 30 s).
                    let executor = executor.clone();
                    this.store
                        .update(cx, |store, cx| store.refresh_usage(executor, cx));
                }
                cx.notify();
            })),
        )
    }
}
