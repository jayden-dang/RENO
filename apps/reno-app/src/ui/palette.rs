//! The Cmd-K command palette: the nav destinations and a few actions, filtered as you type.

use gpui::{
    Context, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent, Render, SharedString,
    Window, div, prelude::*, px,
};

use super::icon::{Icon, icon};
use super::theme;
use crate::nav::Nav;

/// What choosing an entry does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaletteAction {
    Go(Nav),
    ToggleTheme,
    ToggleSidebar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) label: &'static str,
    pub(crate) hint: &'static str,
    pub(crate) icon: Icon,
    pub(crate) action: PaletteAction,
}

/// Every entry: nav destinations first, then actions.
pub(crate) fn entries() -> Vec<Entry> {
    let go = |nav: Nav| Entry {
        label: nav.label(),
        hint: if nav.is_built() {
            "Go to"
        } else {
            "Coming soon"
        },
        icon: nav.icon(),
        action: PaletteAction::Go(nav),
    };
    Nav::MAIN
        .into_iter()
        .chain(Nav::ORCHESTRATION)
        .chain([Nav::Settings])
        .map(go)
        .chain([
            Entry {
                label: "Toggle theme",
                hint: "Action",
                icon: Icon::Settings,
                action: PaletteAction::ToggleTheme,
            },
            Entry {
                label: "Hide or show sidebar",
                hint: "⌘B",
                icon: Icon::Sidebar,
                action: PaletteAction::ToggleSidebar,
            },
        ])
        .collect()
}

/// The entries matching `query`: case-insensitive, every word must appear in the label. An
/// empty query matches everything. Prefix matches sort first, otherwise order is kept.
pub(crate) fn filter(query: &str, all: &[Entry]) -> Vec<Entry> {
    let query = query.trim().to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    let mut hits: Vec<Entry> = all
        .iter()
        .filter(|e| {
            let label = e.label.to_lowercase();
            words.iter().all(|w| label.contains(w))
        })
        .copied()
        .collect();
    hits.sort_by_key(|e| !e.label.to_lowercase().starts_with(&query));
    hits
}

/// What the palette tells the shell.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PaletteEvent {
    Run(PaletteAction),
    Close,
}

impl EventEmitter<PaletteEvent> for Palette {}

#[derive(Debug)]
pub(crate) struct Palette {
    query: String,
    selected: usize,
    focus_handle: FocusHandle,
}

impl Palette {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        Self {
            query: String::new(),
            selected: 0,
            focus_handle: cx.focus_handle(),
        }
    }

    #[cfg(test)]
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    fn visible(&self) -> Vec<Entry> {
        filter(&self.query, &entries())
    }

    pub(crate) fn type_text(&mut self, text: &str) {
        self.query.push_str(text);
        self.selected = 0;
    }

    pub(crate) fn backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
    }

    pub(crate) fn move_selection(&mut self, down: bool) {
        let len = self.visible().len();
        if len == 0 {
            self.selected = 0;
        } else if down {
            self.selected = (self.selected + 1) % len;
        } else {
            self.selected = (self.selected + len - 1) % len;
        }
    }

    pub(crate) fn chosen(&self) -> Option<Entry> {
        self.visible().get(self.selected).copied()
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        match key.key.as_str() {
            "escape" => cx.emit(PaletteEvent::Close),
            "enter" => {
                if let Some(entry) = self.chosen() {
                    cx.emit(PaletteEvent::Run(entry.action));
                }
            }
            "down" => self.move_selection(true),
            "up" => self.move_selection(false),
            "backspace" => self.backspace(),
            _ => {
                let m = key.modifiers;
                if let Some(text) = key.key_char.as_deref()
                    && !(m.platform || m.control || m.alt)
                    && !text.chars().any(char::is_control)
                {
                    self.type_text(text);
                }
            }
        }
        cx.notify();
    }
}

impl Focusable for Palette {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let rows = self.visible();
        let selected = self.selected;

        let input = div()
            .px(px(18.0))
            .h(px(52.0))
            .flex()
            .items_center()
            .border_b_1()
            .border_color(theme.hairline)
            .text_size(px(15.0))
            .child(if self.query.is_empty() {
                div()
                    .text_color(theme.faint)
                    .child("Search destinations and actions")
            } else {
                div().text_color(theme.text).child(self.query.clone())
            });

        let list = div()
            .p(px(8.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(rows.iter().enumerate().map(|(i, entry)| {
                let action = entry.action;
                div()
                    .id(SharedString::from(format!("palette-{i}")))
                    .h(px(34.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .rounded(px(10.0))
                    .text_size(px(13.5))
                    .text_color(theme.text)
                    .cursor_pointer()
                    .when(i == selected, |row| row.bg(theme.row_selected()))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(PaletteEvent::Run(action));
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(icon(entry.icon, 15.0, theme.muted))
                            .child(entry.label),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.faint)
                            .child(entry.hint),
                    )
            }))
            .when(rows.is_empty(), |list| {
                list.child(
                    div()
                        .px(px(12.0))
                        .py(px(10.0))
                        .text_size(px(13.0))
                        .text_color(theme.faint)
                        .child("No matches"),
                )
            });

        div()
            .id("palette-scrim")
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(110.0))
            .bg(gpui::black().opacity(0.35))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(PaletteEvent::Close)))
            .child(
                div()
                    .id("palette")
                    .key_context("Palette")
                    .track_focus(&self.focus_handle)
                    .on_key_down(cx.listener(Self::on_key_down))
                    // A click inside must not reach the scrim.
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .w(px(540.0))
                    .h_auto()
                    .max_h(px(620.0))
                    .overflow_hidden()
                    .rounded(px(16.0))
                    .bg(theme.surface)
                    .border_1()
                    .border_color(theme.hairline)
                    .child(input)
                    .child(list),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(hits: &[Entry]) -> Vec<&'static str> {
        hits.iter().map(|e| e.label).collect()
    }

    #[test]
    fn entries_cover_every_destination_and_the_actions() {
        let all = entries();
        assert_eq!(all.len(), 15);
        assert!(all.iter().any(|e| e.action == PaletteAction::ToggleTheme));
        assert!(all.iter().any(|e| e.action == PaletteAction::ToggleSidebar));
        assert_eq!(all[0].action, PaletteAction::Go(Nav::Home));
    }

    #[test]
    fn filter_matches_case_insensitively_by_word() {
        let all = entries();
        assert_eq!(filter("", &all).len(), all.len());
        assert_eq!(labels(&filter("INB", &all)), ["Inbox"]);
        assert_eq!(
            labels(&filter("sidebar hide", &all)),
            ["Hide or show sidebar"]
        );
        assert!(filter("zzz", &all).is_empty());
    }

    #[test]
    fn filter_puts_prefix_matches_first() {
        let all = entries();
        let hits = labels(&filter("e", &all));
        let first_non_prefix = hits.iter().position(|l| !l.to_lowercase().starts_with('e'));
        let last_prefix = hits.iter().rposition(|l| l.to_lowercase().starts_with('e'));
        if let (Some(non), Some(last)) = (first_non_prefix, last_prefix) {
            assert!(last < non, "{hits:?}");
        }
        assert_eq!(hits[0], "Evaluations");
    }

    #[gpui::test]
    fn typing_selection_and_choice(cx: &mut gpui::TestAppContext) {
        let palette = cx.new(Palette::new);
        palette.update(cx, |p, _| {
            assert_eq!(p.chosen().map(|e| e.label), Some("Home"));
            p.move_selection(false);
            assert_eq!(
                p.chosen().map(|e| e.label),
                Some("Hide or show sidebar"),
                "wraps"
            );
            p.type_text("tog");
            assert_eq!(
                p.chosen().map(|e| e.action),
                Some(PaletteAction::ToggleTheme)
            );
            p.backspace();
            assert_eq!(p.query(), "to");
            p.type_text("zzz");
            assert_eq!(p.chosen(), None);
            p.move_selection(true);
        });
    }
}
