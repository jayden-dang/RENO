//! The Project page: Allowlist and History of one project (ui-brief 2026-11-17). Reached from a
//! project folder's shield button and from a run's project crumb.

use gpui::{
    AnyElement, Context, Entity, FontWeight, IntoElement, Render, SharedString, Subscription,
    Window, div, prelude::*, px,
};

use crate::store::Store;
use crate::ui::kit::{self, MONO};
use crate::ui::page;
use crate::ui::theme::{self, Theme};
use crate::vm::Section;
use crate::vm::history::Verdict;
use crate::vm::project::{self, AllowRow, ProjectView};
use crate::vm::projects::FolderKey;

const EFFECT_W: f32 = 150.0;
const FROM_W: f32 = 170.0;
const ANSWER_W: f32 = 110.0;
const BOT_W: f32 = 110.0;
const WHEN_W: f32 = 90.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Allowlist,
    History,
}

impl Tab {
    const ALL: [Self; 2] = [Self::Allowlist, Self::History];

    const fn label(self) -> &'static str {
        match self {
            Self::Allowlist => "Allowlist",
            Self::History => "History",
        }
    }
}

pub(crate) struct ProjectScreen {
    store: Entity<Store>,
    key: FolderKey,
    tab: Tab,
    _store: Subscription,
}

impl ProjectScreen {
    pub(crate) fn new(store: Entity<Store>, key: FolderKey, cx: &mut Context<Self>) -> Self {
        store.update(cx, |store, cx| {
            store.load_history(cx);
            store.load_rules(cx);
        });
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
            key,
            tab: Tab::Allowlist,
        }
    }

    #[cfg(test)]
    pub(crate) fn key(&self) -> &FolderKey {
        &self.key
    }

    pub(crate) fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        cx.notify();
    }

    pub(crate) fn view(&self, cx: &gpui::App) -> ProjectView {
        let store = self.store.read(cx);
        project::build(
            store.state(),
            &self.key,
            |dir| store.root_of(dir),
            store.rules(),
            store.now_ms(),
        )
    }

    fn tabs(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .gap(px(4.0))
            .children(Tab::ALL.map(|tab| {
                let on = tab == self.tab;
                div()
                    .id(SharedString::from(format!("project-tab-{}", tab.label())))
                    .debug_selector(move || format!("project-tab-{}", tab.label()))
                    .h(px(28.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .rounded(px(8.0))
                    .text_size(px(13.0))
                    .cursor_pointer()
                    .text_color(if on { theme.text } else { theme.muted })
                    .when(on, |t| t.bg(theme.raised).font_weight(FontWeight::MEDIUM))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
                    .child(tab.label())
            }))
            .into_any_element()
    }

    fn rule_table(theme: &Theme, rows: &[AllowRow]) -> AnyElement {
        let count = rows.len();
        page::table_frame(theme, page::table())
            .child(page::table_head(
                theme,
                &[
                    ("Command", None),
                    ("Effect", Some(EFFECT_W)),
                    ("From", Some(FROM_W)),
                ],
            ))
            .children(rows.iter().enumerate().map(|(i, rule)| {
                let (tone, words) = match rule.effect.as_str() {
                    "allow" => (theme.success, "runs without asking"),
                    "deny" => (theme.danger, "never"),
                    _ => (theme.warning, "asks you"),
                };
                page::table_row(theme, i + 1 == count)
                    .child(
                        page::cell(div(), None)
                            .font_family(MONO)
                            .text_size(px(12.5))
                            .child(rule.matches.clone()),
                    )
                    .child(page::cell(
                        div().child(kit::tag(tone, words)),
                        Some(EFFECT_W),
                    ))
                    .child(page::cell(
                        div().text_color(theme.muted).child(rule.from.clone()),
                        Some(FROM_W),
                    ))
            }))
            .into_any_element()
    }

    fn allowlist(&self, view: &ProjectView, theme: &Theme) -> Vec<AnyElement> {
        let mut out = vec![
            div()
                .text_size(px(13.5))
                .text_color(theme.muted)
                .child(format!(
                    "Commands that run without asking you in {}. Anything not listed is held for you.",
                    view.name
                ))
                .into_any_element(),
            page::section_label(theme, "This project").into_any_element(),
            div()
                .id("project-rules-here")
                .text_size(px(13.0))
                .text_color(theme.muted)
                .child(if view.path.is_some() {
                    "Nothing yet. \u{201c}Always in this project\u{201d} on an approval adds here."
                } else {
                    "Runs outside a repository share the rules every project has."
                })
                .into_any_element(),
            page::section_label(theme, "Every project \u{b7} ~/.reno/policy.toml")
                .into_any_element(),
        ];
        if view.everywhere.is_empty() {
            out.push(
                div()
                    .text_size(px(13.0))
                    .text_color(theme.muted)
                    .child(format!(
                        "No rules. Every action gets \u{201c}{}\u{201d}.",
                        view.default
                    ))
                    .into_any_element(),
            );
        } else {
            out.push(Self::rule_table(theme, &view.everywhere));
        }
        out.push(
            div()
                .text_size(px(12.5))
                .text_color(theme.faint)
                .child(format!(
                    "Order: never \u{2192} this project \u{2192} every project \u{2192} \u{201c}{}\u{201d}. A project rule cannot loosen a \u{201c}never\u{201d}.",
                    view.default
                ))
                .into_any_element(),
        );
        out
    }

    fn history(view: &ProjectView, theme: &Theme) -> Vec<AnyElement> {
        let rows = match &view.history {
            Section::Rows(rows) => rows,
            other => {
                let text = match other {
                    Section::Loading => "Loading\u{2026}".to_owned(),
                    Section::Failed(why) => format!("Could not load the history: {why}"),
                    Section::NotConnected => "Not connected.".to_owned(),
                    Section::Unavailable => "This daemon keeps no history.".to_owned(),
                    _ => format!("Nothing decided in {} yet.", view.name),
                };
                return vec![
                    div()
                        .id("project-history-empty")
                        .text_size(px(13.0))
                        .text_color(theme.muted)
                        .child(text)
                        .into_any_element(),
                ];
            }
        };
        let count = rows.len();
        vec![
            page::table_frame(theme, page::table())
                .child(page::table_head(
                    theme,
                    &[
                        ("Answer", Some(ANSWER_W)),
                        ("Command", None),
                        ("Bot", Some(BOT_W)),
                        ("By", None),
                        ("When", Some(WHEN_W)),
                    ],
                ))
                .children(rows.iter().enumerate().map(|(i, row)| {
                    let (tone, word) = match row.verdict {
                        Verdict::Approved => (theme.success, "approved"),
                        Verdict::Denied => (theme.danger, "denied"),
                        Verdict::Expired => (theme.faint, "expired"),
                        Verdict::Withdrawn => (theme.faint, "withdrawn"),
                    };
                    let by = match &row.message {
                        Some(message) => format!("{} \u{2014} \u{201c}{message}\u{201d}", row.by),
                        None => row.by.clone(),
                    };
                    page::table_row(theme, i + 1 == count)
                        .id(SharedString::from(format!("project-history-{i}")))
                        .child(page::cell(
                            div().child(kit::tag(tone, word)),
                            Some(ANSWER_W),
                        ))
                        .child(
                            page::cell(div(), None)
                                .font_family(MONO)
                                .text_size(px(12.5))
                                .truncate()
                                .child(row.line.clone()),
                        )
                        .child(page::cell(
                            div()
                                .text_color(theme.muted)
                                .child(row.bot.clone().unwrap_or_default()),
                            Some(BOT_W),
                        ))
                        .child(page::cell(div().text_color(theme.muted).child(by), None))
                        .child(page::cell(
                            div().text_color(theme.faint).child(row.when.clone()),
                            Some(WHEN_W),
                        ))
                }))
                .into_any_element(),
        ]
    }
}

impl Render for ProjectScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let view = self.view(cx);
        let body = match self.tab {
            Tab::Allowlist => self.allowlist(&view, &theme),
            Tab::History => Self::history(&view, &theme),
        };
        div()
            .id("project")
            .size_full()
            .flex()
            .flex_col()
            .child(page::header(
                &theme,
                format!("{} / {}", view.name, self.tab.label()),
                div(),
            ))
            .child(
                page::column("project-column").child(
                    page::content(page::NARROW)
                        .child(page::lede(
                            &theme,
                            view.name.clone(),
                            view.path
                                .clone()
                                .unwrap_or_else(|| "Runs outside a repository".to_owned()),
                        ))
                        .child(self.tabs(&theme, cx))
                        .children(body),
                ),
            )
    }
}
