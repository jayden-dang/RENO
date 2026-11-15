//! Home: what needs me right now, and is Reno making progress without me?
//!
//! A centred column of at most 720 px: the finding pill, context chips, the composer, the
//! settings line and three suggestion chips. The attention pill docks at the bottom; the corner
//! carries live count, ⌘K and the daemon status.

use gpui::{
    Context, Entity, FontWeight, IntoElement, Render, SharedString, Subscription, Window, div,
    prelude::*, px,
};
use reno_client::ConnectionStatus;

use crate::actions::OpenPalette;
use crate::route;
use crate::store::Store;
use crate::summary::{self, Finding};
use crate::ui::attention::AttentionDock;
use crate::ui::composer::Composer;
use crate::ui::kit;
use crate::ui::sidebar::daemon_label;
use crate::ui::theme::{self, Theme};
use crate::ui::toast;

/// The suggestion chips: label, then the text they put in the composer.
const SUGGESTIONS: [(&str, &str); 3] = [
    (
        "Add retry to the webhook trigger",
        "Add bounded retry with jitter to the webhook trigger",
    ),
    (
        "Run nightly-deps now",
        "Run nightly-deps now for web-console and atlas-api",
    ),
    (
        "Research ACP v2",
        "Research how ACP v2 changes permission requests",
    ),
];

/// What the top of the column says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Headline {
    /// No daemon: the reason, and the page says so plainly.
    Offline {
        reason: String,
    },
    Connecting,
    Finding(Finding),
}

pub(crate) struct HomeScreen {
    store: Entity<Store>,
    composer: Entity<Composer>,
    dock: Entity<AttentionDock>,
    _store: Subscription,
}

impl HomeScreen {
    pub(crate) fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| Composer::new(store.clone(), cx));
        let dock = cx.new(|cx| AttentionDock::new(store.clone(), cx));
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
            composer,
            dock,
        }
    }

    pub(crate) fn composer(&self) -> &Entity<Composer> {
        &self.composer
    }

    #[cfg(test)]
    pub(crate) fn dock(&self) -> &Entity<AttentionDock> {
        &self.dock
    }

    pub(crate) fn headline(&self, cx: &gpui::App) -> Headline {
        let store = self.store.read(cx);
        match store.connection() {
            ConnectionStatus::Disconnected => Headline::Offline {
                reason: store
                    .offline_reason()
                    .unwrap_or("The daemon connection closed.")
                    .to_owned(),
            },
            ConnectionStatus::Connecting => Headline::Connecting,
            ConnectionStatus::Connected => {
                Headline::Finding(summary::finding(store.state(), store.now_ms()))
            }
        }
    }

    fn finding_pill(&self, headline: &Headline, theme: &Theme) -> gpui::Div {
        let pill = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .h(px(40.0))
            .px(px(18.0))
            .rounded(px(999.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.hairline)
            .text_size(px(14.5));
        match headline {
            Headline::Finding(Finding::Held { lead, detail }) => pill
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(lead.clone()),
                )
                .child(div().text_color(theme.muted).child(detail.clone()))
                .child(
                    div()
                        .id("review")
                        .h(px(26.0))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .rounded(px(999.0))
                        .border_1()
                        .border_color(theme.hairline)
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover({
                            let hover = theme.raised;
                            move |s| s.bg(hover)
                        })
                        .on_click(|_, _, cx| toast::not_yet(cx, "Inbox"))
                        .child("Review"),
                ),
            Headline::Finding(Finding::Calm { text }) => pill
                .child(div().text_color(theme.success).child("✓"))
                .child(div().text_color(theme.muted).child(text.clone())),
            Headline::Offline { .. } => pill
                .border_color(theme.warning.opacity(0.4))
                .child(div().text_color(theme.warning).child("!"))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child("Daemon not running"),
                ),
            Headline::Connecting => pill.child(
                div()
                    .text_color(theme.muted)
                    .child("Connecting to the daemon…"),
            ),
        }
    }

    fn corner(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let live = summary::attention(store.state()).running;
        let status = store.connection();
        let dot = match status {
            ConnectionStatus::Connected => theme.success,
            ConnectionStatus::Connecting => theme.warning,
            ConnectionStatus::Disconnected => theme.danger,
        };
        div()
            .absolute()
            .top(px(14.0))
            .right(px(16.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(kit::chip(theme, format!("▷ {live} live")))
            .child(
                kit::chip(theme, "⌕ ⌘K")
                    .id("open-palette")
                    .cursor_pointer()
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenPalette), cx)),
            )
            .child(
                kit::chip(theme, daemon_label(status))
                    .flex_row_reverse()
                    .child(kit::dot(dot, 7.0)),
            )
    }
}

impl Render for HomeScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let headline = self.headline(cx);
        let class = self.composer.read(cx).class();
        let route = route::route_for(class);
        let offline_reason = match &headline {
            Headline::Offline { reason } => Some(reason.clone()),
            _ => None,
        };

        let settings_line = div()
            .w_full()
            .flex()
            .items_center()
            .gap(px(22.0))
            .px(px(14.0))
            .text_size(px(13.5))
            .text_color(theme.muted)
            .child(format!("⑂ {}", route.worktree))
            .child(format!("⛨ {}", route.access))
            .child("◷ now · or schedule");

        let suggestions = div()
            .flex()
            .flex_wrap()
            .justify_center()
            .gap(px(10.0))
            .children(SUGGESTIONS.map(|(label, fill)| {
                kit::chip(&theme, label)
                    .id(SharedString::from(format!("suggest-{label}")))
                    .h(px(36.0))
                    .px(px(16.0))
                    .text_size(px(14.0))
                    .cursor_pointer()
                    .hover({
                        let hover = theme.raised;
                        move |s| s.bg(hover)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.composer
                            .update(cx, |composer, cx| composer.fill(fill, window, cx));
                    }))
            }));

        // Placeholder context: team and repository come from state once the protocol has them.
        let context_chips = div()
            .w_full()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .child(kit::chip(&theme, "⚇ team core"))
            .child(kit::chip(&theme, "▭ reno"));

        let column = div()
            .w_full()
            .max_w(px(720.0))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(14.0))
            .child(self.finding_pill(&headline, &theme))
            .when_some(offline_reason, |col, reason| {
                col.child(
                    div()
                        .max_w(px(560.0))
                        .text_size(px(13.5))
                        .text_color(theme.muted)
                        .child(format!(
                            "{reason} Start renod, or run reno-app --fake for a demo daemon."
                        )),
                )
            })
            .child(context_chips)
            .child(self.composer.clone())
            .child(settings_line)
            .child(div().pt(px(8.0)).child(suggestions));

        div()
            .id("home")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .px(px(24.0))
                    .pb(px(150.0))
                    .child(column),
            )
            .child(
                div()
                    .absolute()
                    .bottom(px(16.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .flex()
                    .justify_center()
                    .px(px(24.0))
                    .child(self.dock.clone()),
            )
            .child(self.corner(&theme, cx))
    }
}
