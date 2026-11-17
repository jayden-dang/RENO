//! The Project page: Allowlist and History of one project (ui-brief 2026-11-17). Reached from a
//! project folder's shield button and from a run's project crumb.

use gpui::{
    AnyElement, Context, Entity, FontWeight, IntoElement, Render, SharedString, Subscription,
    Window, div, prelude::*, px,
};

use crate::store::Store;
use crate::ui::composer::{Composer, ReasonEvent};
use crate::ui::kit::{self, MONO};
use crate::ui::page;
use crate::ui::theme::{self, Theme};
use crate::vm::Section;
use crate::vm::history::Verdict;
use crate::vm::project::{self, AllowRow, HereRow, ProjectView};
use crate::vm::projects::FolderKey;

const EFFECT_W: f32 = 150.0;
const FROM_W: f32 = 170.0;
const ANSWER_W: f32 = 110.0;
const BOT_W: f32 = 110.0;
const WHEN_W: f32 = 90.0;
const REMOVE_W: f32 = 90.0;

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
    /// "Add a command": Enter keeps it for this project. Only for a repository.
    add: Option<(Entity<Composer>, Subscription)>,
    _store: Subscription,
}

impl ProjectScreen {
    pub(crate) fn new(store: Entity<Store>, key: FolderKey, cx: &mut Context<Self>) -> Self {
        store.update(cx, |store, cx| {
            store.load_history(cx);
            store.load_rules(cx);
        });
        let add = key.root().map(|root| {
            let root = root.to_path_buf();
            let name = key.name();
            let field = cx.new(|cx| {
                Composer::field(
                    store.clone(),
                    "Add a command, e.g. cargo clippy --workspace",
                    format!("Enter to allow it in {name} \u{b7} Esc to clear"),
                    cx,
                )
            });
            let events = cx.subscribe(&field, move |this, field, event: &ReasonEvent, cx| {
                if let ReasonEvent::Submitted(Some(command)) = event {
                    let (root, command) = (root.clone(), command.clone());
                    this.store
                        .update(cx, |store, cx| store.add_project_rule(root, command, cx));
                    field.update(cx, |field, cx| field.clear(cx));
                }
            });
            (field, events)
        });
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
            key,
            tab: Tab::Allowlist,
            add,
        }
    }

    #[cfg(test)]
    pub(crate) fn add_field(&self) -> Option<&Entity<Composer>> {
        self.add.as_ref().map(|(field, _)| field)
    }

    fn here_table(&self, theme: &Theme, rows: &[HereRow], cx: &mut Context<Self>) -> AnyElement {
        let count = rows.len();
        page::table_frame(theme, page::table())
            .child(page::table_head(
                theme,
                &[
                    ("Command", None),
                    ("Effect", Some(EFFECT_W)),
                    ("From", Some(FROM_W)),
                    ("", Some(REMOVE_W)),
                ],
            ))
            .children(rows.iter().enumerate().map(|(i, rule)| {
                let (tone, words) = match rule.effect.as_str() {
                    "allow" => (theme.success, "runs without asking"),
                    _ => (theme.danger, "never"),
                };
                let busy = self.store.read(cx).is_revoking(&rule.id);
                let id = rule.id.clone();
                let remove = kit::pill_button(
                    theme,
                    format!("remove-{}", rule.id),
                    "Remove",
                    kit::Tone::Quiet,
                )
                .when(busy, |b| b.opacity(0.4))
                .when(!busy, |b| {
                    b.on_click(cx.listener(move |this, _, _, cx| {
                        let id = id.clone();
                        this.store.update(cx, |store, cx| store.revoke_rule(id, cx));
                    }))
                });
                page::table_row(theme, i + 1 == count)
                    .child(
                        page::cell(div(), None)
                            .font_family(MONO)
                            .text_size(px(12.5))
                            .child(rule.matches.clone()),
                    )
                    .child(page::cell(
                        div().flex().child(kit::tag(tone, words)),
                        Some(EFFECT_W),
                    ))
                    .child(page::cell(
                        div().text_color(theme.muted).child(rule.from.clone()),
                        Some(FROM_W),
                    ))
                    .child(page::cell(div().child(remove), Some(REMOVE_W)))
            }))
            .into_any_element()
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
                        div().flex().child(kit::tag(tone, words)),
                        Some(EFFECT_W),
                    ))
                    .child(page::cell(
                        div().text_color(theme.muted).child(rule.from.clone()),
                        Some(FROM_W),
                    ))
            }))
            .into_any_element()
    }

    fn allowlist(
        &self,
        view: &ProjectView,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
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
        ];
        let note = |text: String| {
            div()
                .id("project-rules-here")
                .debug_selector(|| "project-rules-here".to_owned())
                .text_size(px(13.0))
                .text_color(theme.muted)
                .child(text)
                .into_any_element()
        };
        if view.path.is_none() {
            out.push(note(
                "Runs outside a repository share the rules every project has.".to_owned(),
            ));
        } else if !view.project_rules {
            out.push(note(
                "This renod keeps no rules for one project; update it to set them here.".to_owned(),
            ));
        } else {
            if view.here.is_empty() {
                out.push(note(format!(
                    "Nothing yet. \u{201c}Always in {}\u{201d} on an approval, or a command added below, lands here.",
                    view.name
                )));
            } else {
                out.push(self.here_table(theme, &view.here, cx));
            }
            if let Some((field, _)) = &self.add {
                out.push(
                    div()
                        .id("project-add")
                        .w_full()
                        .rounded(px(12.0))
                        .border_1()
                        .border_color(theme.hairline)
                        .child(field.clone())
                        .into_any_element(),
                );
            }
        }
        out.push(
            page::section_label(theme, "Every project \u{b7} ~/.reno/policy.toml")
                .into_any_element(),
        );
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
                    "A \u{201c}never\u{201d} always wins over a rule that allows, so a project rule cannot loosen one. Anything no rule allows gets \u{201c}{}\u{201d}.",
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
                            div().flex().child(kit::tag(tone, word)),
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
            Tab::Allowlist => self.allowlist(&view, &theme, cx),
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
