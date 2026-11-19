//! Home is the new-thread state (ui-brief, "New-thread state"): the mark, "What should we build
//! in <project>?" (the project underlined, opening that project), a chip strip above the
//! composer, and nothing else. What needs the owner lives in the sidebar badges, All runs and the
//! composer's panels. Offline and connecting still say so.

use gpui::{
    Context, Entity, EventEmitter, FontWeight, IntoElement, Render, SharedString, Subscription,
    Window, div, prelude::*, px,
};
use reno_client::ConnectionStatus;

use super::ScreenEvent;
use crate::store::Store;
use crate::ui::composer::Composer;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, Tone};
use crate::ui::theme;
use crate::vm::new_thread;
use crate::vm::projects::FolderKey;

/// What the top of the column says besides the heading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Headline {
    /// No daemon: the reason, and the page says so plainly.
    Offline {
        reason: String,
    },
    Connecting,
    Ready,
}

pub(crate) struct HomeScreen {
    store: Entity<Store>,
    composer: Entity<Composer>,
    /// The project a new thread is for (a project's "+"); none for "Start a task".
    project: Option<(String, FolderKey)>,
    _store: Subscription,
}

impl HomeScreen {
    pub(crate) fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| Composer::new(store.clone(), cx));
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
            composer,
            project: None,
        }
    }

    /// The project's name a new thread is for, or `None`.
    pub(crate) fn set_project(
        &mut self,
        project: Option<(String, FolderKey)>,
        cx: &mut Context<Self>,
    ) {
        if self.project != project {
            self.project = project;
            cx.notify();
        }
    }

    pub(crate) fn heading(&self) -> new_thread::Heading {
        new_thread::heading(self.project.as_ref().map(|(name, _)| name.as_str()))
    }

    #[cfg(test)]
    pub(crate) fn project(&self) -> Option<&str> {
        self.project.as_ref().map(|(name, _)| name.as_str())
    }

    pub(crate) fn composer(&self) -> &Entity<Composer> {
        &self.composer
    }

    pub(crate) fn headline(&self, cx: &gpui::App) -> Headline {
        let store = self.store.read(cx);
        match store.connection() {
            ConnectionStatus::Disconnected => Headline::Offline {
                reason: store
                    .offline_reason()
                    .unwrap_or_else(|| "The daemon connection closed.".to_owned()),
            },
            ConnectionStatus::Connecting => Headline::Connecting,
            ConnectionStatus::Connected => Headline::Ready,
        }
    }

    fn open_project(&mut self, cx: &mut Context<Self>) {
        if let Some((_, key)) = &self.project {
            cx.emit(ScreenEvent::OpenProject(key.clone()));
        }
    }
}

impl EventEmitter<ScreenEvent> for HomeScreen {}

impl Render for HomeScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let headline = self.headline(cx);
        let heading = self.heading();
        let project = self.project.as_ref().map(|(name, _)| name.clone());

        let title = div()
            .id("new-thread-title")
            .debug_selector(|| "new-thread-title".to_owned())
            .flex()
            .items_baseline()
            .justify_center()
            .text_size(px(28.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text)
            .child(SharedString::from(heading.before.clone()))
            .children(heading.project.clone().map(|name| {
                div()
                    .id("new-thread-project")
                    .debug_selector(|| "new-thread-project".to_owned())
                    .cursor_pointer()
                    .underline()
                    .child(SharedString::from(name))
                    .on_click(cx.listener(|this, _, _, cx| this.open_project(cx)))
            }))
            .child(SharedString::from(heading.after.clone()));

        let chips = div().w_full().flex().gap(px(8.0)).children(
            new_thread::chips(project.as_deref())
                .into_iter()
                .map(|chip| {
                    let selector = format!("new-thread-chip-{}", chip.id);
                    let base = match chip.id {
                        "this-mac" => kit::icon_chip(&theme, Icon::Server, chip.label),
                        "project" => kit::icon_chip(&theme, Icon::Folder, chip.label),
                        _ => kit::chip(&theme, chip.label),
                    };
                    let shown = selector.clone();
                    let more = chip.id == "more";
                    base.id(SharedString::from(selector))
                        .debug_selector(move || shown.clone())
                        .when(more, |d| {
                            // "\u{2026}" opens the palette: more places and actions.
                            d.cursor_pointer().on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(crate::actions::OpenPalette), cx);
                            })
                        })
                }),
        );

        let notice = match &headline {
            Headline::Offline { reason } => Some(
                div()
                    .max_w(px(560.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.0))
                    .text_size(px(13.5))
                    .text_color(theme.muted)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child("Daemon not running"),
                    )
                    .children(
                        reason
                            .lines()
                            .map(|line| div().child(line.trim_end().to_owned())),
                    )
                    .child("Or run reno-app --fake for a demo daemon.")
                    .child(
                        kit::pill_button(&theme, "retry-now", "Retry now", Tone::Quiet).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.store.update(cx, |store, _| store.retry_now());
                            }),
                        ),
                    ),
            ),
            Headline::Connecting => Some(
                div()
                    .text_size(px(13.5))
                    .text_color(theme.muted)
                    .child("Connecting to the daemon\u{2026}"),
            ),
            Headline::Ready => None,
        };

        div()
            .id("home")
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .px(px(24.0))
            .pb(px(60.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(720.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(18.0))
                    .child(
                        div()
                            .id("new-thread-mark")
                            .debug_selector(|| "new-thread-mark".to_owned())
                            .child(icon(Icon::Mark, 40.0, theme.text)),
                    )
                    .child(title)
                    .children(notice)
                    .child(div().h(px(6.0)))
                    .child(chips)
                    .child(self.composer.clone()),
            )
    }
}
