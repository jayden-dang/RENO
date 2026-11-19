//! Settings (ui-brief 2026-11-19, settings): while it shows, its navigation takes the sidebar's
//! place (Back to app, search, the categories in groups) and the canvas holds one page in cards.
//! Executors and Add from registry are drawn by the [`ExecutorsScreen`] entity; Definitions
//! (live through `defs.watch`), Policy, Notifications and Daemon are drawn here.

use std::rc::Rc;

use gpui::{
    AnyElement, Context, Entity, FontWeight, IntoElement, Render, Role, SharedString, Subscription,
    Window, div, prelude::*, px,
};
use reno_client::ConnectionStatus;
use reno_notify::Delivery;
use reno_proto::defs::DefKind;

use crate::lifecycle;
use crate::prefs::{Pref, Prefs};
use crate::screens::executors::{ExecutorsScreen, Part};
use crate::store::Store;
use crate::ui::field::{Field, FieldEvent, Look};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::page::{self, CountTone};
use crate::ui::sidebar;
use crate::ui::theme::{self, Theme};
use crate::vm::rules;
use crate::vm::settings::{self, DaemonView, DefRow, DefsView, PolicyView};
use crate::vm::settings_nav::{self, Category};
use crate::vm::{Banner, Section};

/// The policy table's fixed cells, kept narrow so the Matches column (which flexes, and holds a
/// command glob) takes the rest of the row. Rows and header share [`page::cell`], so they align.
const EFFECT_W: f32 = 64.0;
const SCOPE_W: f32 = 120.0;
const NOTE_W: f32 = 170.0;

/// The search at the top of the navigation: a pill as wide as the column.
const SEARCH: Look = Look {
    pill: true,
    mono: false,
    multiline: false,
    rows: 1,
    width: None,
    bare: false,
};

/// What the navigation column needs from the root: the way back.
pub(crate) struct NavActions {
    pub(crate) back: page::Action,
}

pub(crate) struct SettingsScreen {
    store: Entity<Store>,
    executors: Entity<ExecutorsScreen>,
    category: Category,
    search: Entity<Field>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsScreen {
    pub(crate) fn new(
        store: Entity<Store>,
        executors: Entity<ExecutorsScreen>,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            Field::new(
                "settings-search",
                "Search settings",
                "Search settings",
                SEARCH,
                cx,
            )
        });
        let subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe(&search, |this, _, event: &FieldEvent, cx| {
                if *event == FieldEvent::Changed {
                    this.searched(cx);
                }
            }),
        ];
        Self {
            store,
            executors,
            category: Category::default(),
            search,
            _subscriptions: subscriptions,
        }
    }

    #[cfg(test)]
    pub(crate) fn category(&self) -> Category {
        self.category
    }

    /// Shows a category's page and reads what it needs.
    pub(crate) fn set_category(&mut self, category: Category, cx: &mut Context<Self>) {
        self.category = category;
        self.entered(cx);
        cx.notify();
    }

    /// Called when Settings is shown: `daemon.status` has no watch, so it is read now, and the
    /// page in front reads what it needs.
    pub(crate) fn shown(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, _| store.refresh_status());
        self.entered(cx);
    }

    /// Settings opened from another screen: the navigation lists every category again.
    pub(crate) fn opened(&mut self, cx: &mut Context<Self>) {
        if !self.search.read(cx).text().is_empty() {
            self.search.update(cx, |field, cx| field.set_text("", cx));
            cx.notify();
        }
    }

    fn entered(&mut self, cx: &mut Context<Self>) {
        match self.category {
            Category::Daemon => self.store.update(cx, |store, _| store.refresh_status()),
            Category::Policy => self.store.update(cx, |store, cx| store.load_rules(cx)),
            Category::Executors | Category::Registry => {
                let part = if self.category == Category::Registry {
                    Part::Registry
                } else {
                    Part::Roster
                };
                self.executors.update(cx, |screen, cx| {
                    screen.set_part(part, cx);
                    screen.shown(cx);
                });
            }
            Category::Definitions
            | Category::Notifications
            | Category::Accounts
            | Category::General
            | Category::Appearance => {}
        }
    }

    /// The search changed: keep the page while it still matches, else show the first match.
    fn searched(&mut self, cx: &mut Context<Self>) {
        let shown = settings_nav::matching(self.search.read(cx).text());
        let next = settings_nav::after_search(self.category, &shown);
        if next == self.category {
            cx.notify();
        } else {
            self.set_category(next, cx);
        }
    }

    pub(crate) fn defs_view(&self, cx: &gpui::App) -> DefsView {
        settings::defs(self.store.read(cx).state())
    }

    pub(crate) fn daemon_view(&self, cx: &gpui::App) -> DaemonView {
        let store = self.store.read(cx);
        settings::daemon(
            store.state(),
            store.socket(),
            store.now_ms(),
            store.status_seen_at_ms(),
        )
    }

    pub(crate) fn validate(&mut self, kind: DefKind, name: &str, cx: &mut Context<Self>) {
        let name = name.to_owned();
        self.store
            .update(cx, |store, cx| store.validate_def(kind, name, cx));
    }

    /// The navigation column that takes the sidebar's place while Settings shows. `needing` is
    /// the executors that can't take runs, the same count the sidebar's Settings row carries.
    pub(crate) fn nav_column(
        &mut self,
        theme: &Theme,
        needing: usize,
        connection: ConnectionStatus,
        actions: &NavActions,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let back = Rc::clone(&actions.back);
        let hover = theme.row_selected();
        let back_row = div()
            .id("settings-back")
            .debug_selector(|| "settings-back".to_owned())
            .h(px(30.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(8.0))
            .text_size(px(13.5))
            .text_color(theme.muted)
            .cursor_pointer()
            .role(Role::Button)
            .aria_label("Back to app")
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .hover(move |s| s.bg(hover))
            .on_click(move |_, window, cx| back(window, cx))
            .child(icon(Icon::Back, 15.0, theme.muted))
            .child("Back to app");
        let title = div()
            .px(px(10.0))
            .pt(px(6.0))
            .pb(px(10.0))
            .text_size(px(18.0))
            .font_weight(FontWeight::SEMIBOLD)
            .child("Settings");

        let shown = settings_nav::matching(self.search.read(cx).text());
        let mut list: Vec<AnyElement> = Vec::new();
        let mut group = "";
        for category in &shown {
            if category.group() != group {
                group = category.group();
                list.push(
                    div()
                        .px(px(10.0))
                        .pt(px(18.0))
                        .pb(px(6.0))
                        .text_size(px(11.5))
                        .text_color(theme.faint)
                        .child(group)
                        .into_any_element(),
                );
            }
            list.push(self.category_row(theme, *category, needing, connection, cx));
        }
        if shown.is_empty() {
            list.push(
                div()
                    .id("settings-nothing")
                    .debug_selector(|| "settings-nothing".to_owned())
                    .px(px(10.0))
                    .pt(px(14.0))
                    .text_size(px(13.0))
                    .text_color(theme.faint)
                    .child("Nothing matches")
                    .into_any_element(),
            );
        }

        div()
            .id("settings-nav")
            .debug_selector(|| "settings-nav".to_owned())
            .w(px(sidebar::WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme.shell)
            .child(
                div()
                    .id("settings-nav-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(10.0))
                    .pb(px(14.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(back_row)
                    .child(title)
                    .child(div().pb(px(2.0)).child(self.search.clone()))
                    .children(list),
            )
            .into_any_element()
    }

    fn category_row(
        &self,
        theme: &Theme,
        category: Category,
        needing: usize,
        connection: ConnectionStatus,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = category == self.category;
        let hover = theme.row_selected();
        let ink = if selected { theme.accent } else { theme.text };
        let glyph = match category {
            Category::Executors => Icon::PhCpu,
            Category::Registry => Icon::PhCube,
            Category::Definitions => Icon::File,
            Category::Policy => Icon::Shield,
            Category::Notifications => Icon::Bell,
            Category::Daemon => Icon::Server,
            Category::Accounts => Icon::Team,
            Category::General => Icon::Settings,
            Category::Appearance => Icon::Eye,
        };
        let key = category.key();
        let trailing: Option<AnyElement> = match category {
            Category::Executors if needing > 0 => Some(
                page::count(theme, needing, CountTone::Danger)
                    .id("settings-executors-count")
                    .debug_selector(|| "settings-executors-count".to_owned())
                    .into_any_element(),
            ),
            Category::Daemon => Some(
                kit::dot(
                    match connection {
                        ConnectionStatus::Connected => theme.success,
                        ConnectionStatus::Connecting => theme.warning,
                        ConnectionStatus::Disconnected => theme.danger,
                    },
                    7.0,
                )
                .into_any_element(),
            ),
            _ => None,
        };
        div()
            .id(SharedString::from(format!("settings-{key}")))
            .debug_selector(move || format!("settings-{key}"))
            .h(px(32.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .rounded(px(8.0))
            .text_size(px(14.0))
            .text_color(ink)
            .cursor_pointer()
            .role(Role::Button)
            .aria_label(category.label())
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .when(selected, |r| {
                r.bg(theme.accent.opacity(0.11))
                    .font_weight(FontWeight::MEDIUM)
            })
            .when(!selected, |r| r.hover(move |s| s.bg(hover)))
            .on_click(cx.listener(move |this, _, _, cx| this.set_category(category, cx)))
            .child(icon(
                glyph,
                16.0,
                if selected { theme.accent } else { theme.muted },
            ))
            .child(div().flex_1().min_w_0().truncate().child(category.label()))
            .children(trailing)
            .into_any_element()
    }

    fn def_row(
        &self,
        row: &DefRow,
        last: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.store.read(cx).is_validating(row.kind, &row.name);
        let (kind, name) = (row.kind, row.name.clone());
        let button = kit::pill_button(
            theme,
            format!("validate-{}-{}", row.kind, row.name),
            if busy { "Checking…" } else { "Validate" },
            Tone::Quiet,
        );
        let button = if busy {
            button.opacity(0.4)
        } else {
            button.on_click(cx.listener(move |this, _, _, cx| this.validate(kind, &name, cx)))
        };
        let problems = row.problems.iter().map(|p| {
            div()
                .text_size(px(12.5))
                .line_height(px(18.0))
                .text_color(theme.danger)
                .child(p.clone())
        });
        let validation = row.validation.as_ref().map(|v| {
            let (tone, head) = if v.valid {
                (theme.success, "valid".to_owned())
            } else {
                (theme.danger, "not valid".to_owned())
            };
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .text_size(px(12.5))
                .child(div().text_color(tone).child(head))
                .children(
                    v.errors
                        .iter()
                        .map(|e| div().text_color(theme.danger).child(e.clone())),
                )
        });
        page::table_row(theme, last)
            .id(SharedString::from(format!("def-{}-{}", row.kind, row.name)))
            .items_start()
            .child(
                div()
                    .flex_none()
                    .w(px(170.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(kit::dot(
                        if row.has_problems() {
                            theme.danger
                        } else {
                            theme.success
                        },
                        7.0,
                    ))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .truncate()
                            .child(row.name.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(theme.faint)
                            .child(format!("v{}", row.version)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .text_color(theme.muted)
                            .child(if row.detail.is_empty() {
                                "\u{2026}".to_owned()
                            } else {
                                row.detail.clone()
                            }),
                    )
                    .children(problems)
                    .children(validation),
            )
            .child(div().flex_none().child(button))
            .into_any_element()
    }

    fn defs_table(
        &self,
        label: &'static str,
        section: &Section<DefRow>,
        empty: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = section.rows();
        let rendered: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| self.def_row(r, i + 1 == rows.len(), theme, cx))
            .collect();
        div()
            .flex()
            .flex_col()
            .child(page::section_head(theme, label, None, None))
            .when_some(page::note(theme, section, empty), ParentElement::child)
            .when(!rendered.is_empty(), |d| {
                d.child(page::table_frame(theme, page::table()).children(rendered))
            })
            .into_any_element()
    }

    fn definitions(
        &self,
        view: &DefsView,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let (title, detail) = match view.problem_count {
            0 => (
                "Definitions are valid.".to_owned(),
                "Bots, triggers and the policy are files in the definitions directory. Edits apply as soon as they validate.".to_owned(),
            ),
            n => (
                format!("{n} problem{} in definition files.", if n == 1 { "" } else { "s" }),
                "The daemon keeps running on the last definitions that validated, until the files are fixed.".to_owned(),
            ),
        };
        let mut out =
            vec![page::title(theme, title, detail, view.problem_count > 0).into_any_element()];
        if !view.orphan_problems.is_empty() {
            out.push(
                div()
                    .id("def-problems")
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .p(px(14.0))
                    .rounded(px(12.0))
                    .bg(theme.surface)
                    .border_1()
                    .border_color(theme.danger.opacity(0.4))
                    .children(view.orphan_problems.iter().map(|p| {
                        div()
                            .font_family(MONO)
                            .text_size(px(12.5))
                            .text_color(theme.danger)
                            .child(p.clone())
                    }))
                    .into_any_element(),
            );
        }
        out.push(self.defs_table("Bots", &view.bots, "No bots defined.", theme, cx));
        out.push(self.defs_table(
            "Triggers",
            &view.triggers,
            "No triggers defined.",
            theme,
            cx,
        ));
        if !view.policy.is_empty() {
            let policy = Section::Rows(view.policy.clone());
            out.push(self.defs_table("Policy", &policy, "", theme, cx));
        }
        out
    }

    /// The standing rules: answers kept on a run's card (or `reno approve --remember`), each
    /// holding exactly the action it came from. The policy file's own deny rules always win over
    /// them.
    fn standing_rules(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let section = {
            let store = self.store.read(cx);
            let supported = store.state().supports(reno_proto::FEATURE_RULES);
            rules::rows(store.rules(), supported, store.now_ms())
        };
        let rows = section.rows();
        let count = rows.len();
        let rendered: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let busy = self.store.read(cx).is_revoking(&row.id);
                let id = row.id.clone();
                let button = kit::pill_button(
                    theme,
                    format!("revoke-{}", row.id),
                    if busy { "Revoking\u{2026}" } else { "Revoke" },
                    Tone::Quiet,
                );
                let button = if busy {
                    button.opacity(0.4)
                } else {
                    button.on_click(cx.listener(move |this, _, _, cx| {
                        let id = id.clone();
                        this.store.update(cx, |store, cx| store.revoke_rule(id, cx));
                    }))
                };
                let tone = match row.effect {
                    reno_proto::StandingEffect::Allow => theme.success,
                    reno_proto::StandingEffect::Deny => theme.danger,
                };
                let word = match row.effect {
                    reno_proto::StandingEffect::Allow => "allow",
                    reno_proto::StandingEffect::Deny => "deny",
                };
                page::table_row(theme, i + 1 == count)
                    .id(SharedString::from(format!("rule-{}", row.id)))
                    .debug_selector({
                        let id = row.id.clone();
                        move || format!("rule-{id}")
                    })
                    .child(
                        div()
                            .map(|d| page::cell(d, Some(70.0)))
                            .child(kit::tag(tone, word)),
                    )
                    .child(
                        div()
                            .map(|d| page::cell(d, None))
                            // What the rule holds is the point of the row: it wraps rather than
                            // being cut, so the exact command is always readable.
                            .font_family(MONO)
                            .text_size(px(12.5))
                            .line_height(px(18.0))
                            .child(row.matches.clone()),
                    )
                    .child(
                        div()
                            .map(|d| page::cell(d, Some(120.0)))
                            .truncate()
                            .text_color(theme.muted)
                            .child(row.scope.clone()),
                    )
                    .child(
                        div()
                            .map(|d| page::cell(d, Some(90.0)))
                            .text_color(if row.expired {
                                theme.warning
                            } else {
                                theme.muted
                            })
                            .child(row.ends.clone()),
                    )
                    .child(div().flex_none().child(button))
                    .into_any_element()
            })
            .collect();
        div()
            .id("standing-rules")
            .debug_selector(|| "standing-rules".to_owned())
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(page::section_head(theme, "Your standing rules", None, None).pb(px(0.0)))
            .child(
                div()
                    .text_size(px(12.5))
                    .line_height(px(19.0))
                    .text_color(theme.muted)
                    .child(
                        "Answers you kept on a run's card. Each holds exactly the action it came from, and a deny rule in the policy file always wins over it.",
                    ),
            )
            .when_some(
                page::note(theme, &section, "No standing rules. Choose a scope on a held action to make one."),
                ParentElement::child,
            )
            .when(!rendered.is_empty(), |d| {
                d.child(
                    page::table_frame(theme, page::table())
                        .child(page::table_head(
                            theme,
                            &[
                                ("Effect", Some(70.0)),
                                ("Holds", None),
                                ("Applies to", Some(120.0)),
                                ("Ends", Some(90.0)),
                                ("", Some(80.0)),
                            ],
                        ))
                        .children(rendered),
                )
            })
            .into_any_element()
    }

    fn policy_tab(
        &self,
        view: Option<&PolicyView>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(view) = view else {
            return vec![
                page::title(
                    theme,
                    "No policy yet.",
                    "Without a policy every action asks a human, and an unanswered ask is denied.",
                    false,
                )
                .into_any_element(),
                self.standing_rules(theme, cx),
            ];
        };
        let ttl = view.ttl.as_ref().map_or_else(String::new, |t| {
            format!(" Unanswered asks expire after {t}.")
        });
        let mut out = vec![
            page::title(
                theme,
                "What bots may do without asking.",
                format!(
                    "The first rule that matches decides; when none does, the answer is \"{}\".{ttl}",
                    view.default
                ),
                false,
            )
            .into_any_element(),
        ];
        let count = view.rules.len();
        let rows = view.rules.iter().enumerate().map(|(i, rule)| {
            let tone = match rule.effect.as_str() {
                "allow" => theme.success,
                "deny" => theme.danger,
                _ => theme.warning,
            };
            page::table_row(theme, i + 1 == count)
                .child(page::cell(
                    div().child(kit::tag(tone, rule.effect.clone())),
                    Some(EFFECT_W),
                ))
                .child(
                    page::cell(div(), None)
                        .font_family(MONO)
                        .text_size(px(12.5))
                        .child(rule.matches.clone()),
                )
                .child(page::cell(
                    div().text_color(theme.muted).child(rule.scope.clone()),
                    Some(SCOPE_W),
                ))
                .child(page::cell(
                    div().text_color(theme.muted).child(rule.note.clone()),
                    Some(NOTE_W),
                ))
        });
        out.push(
            div()
                .flex()
                .flex_col()
                .child(page::section_head(theme, "Rules", Some(count), None))
                .child(
                    page::table_frame(theme, page::table())
                        .child(page::table_head(
                            theme,
                            &[
                                ("Effect", Some(EFFECT_W)),
                                ("Matches", None),
                                ("Applies to", Some(SCOPE_W)),
                                ("Note", Some(NOTE_W)),
                            ],
                        ))
                        .children(rows),
                )
                .into_any_element(),
        );
        out.push(self.standing_rules(theme, cx));
        out
    }

    fn daemon_page(&self, view: &DaemonView, theme: &Theme) -> Vec<AnyElement> {
        let (title, tone) = match view.connection {
            ConnectionStatus::Connected => ("renod is running.", theme.success),
            ConnectionStatus::Connecting => ("Connecting to renod\u{2026}", theme.warning),
            ConnectionStatus::Disconnected => ("renod is not reachable.", theme.danger),
        };
        let dash = || "\u{2014}".to_owned();
        let pair = |a: Option<u32>, b: Option<u32>| match (a, b) {
            (Some(a), Some(b)) => format!("{a} active \u{b7} {b} queued"),
            _ => dash(),
        };
        let list = |items: &[String]| {
            if items.is_empty() {
                dash()
            } else {
                items.join(", ")
            }
        };
        let facts: [(&str, String); 8] = [
            ("Version", view.version.clone().unwrap_or_else(dash)),
            (
                "Protocol",
                view.protocol.map_or_else(dash, |p| p.to_string()),
            ),
            ("Process", view.pid.map_or_else(dash, |p| p.to_string())),
            ("Uptime", view.uptime.clone().unwrap_or_else(dash)),
            ("Runs", pair(view.active, view.queued)),
            ("Executors", list(&view.executors)),
            (
                "Socket",
                view.socket
                    .clone()
                    .unwrap_or_else(|| "in-process (demo)".to_owned()),
            ),
            ("Features", list(&view.features)),
        ];
        let count = facts.len();
        let rows = facts.into_iter().enumerate().map(|(i, (label, value))| {
            page::table_row(theme, i + 1 == count)
                .items_start()
                .child(
                    div()
                        .flex_none()
                        .w(px(120.0))
                        .text_color(theme.muted)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(value))
        });
        vec![
            div()
                .flex()
                .items_start()
                .gap(px(10.0))
                .child(div().pt(px(10.0)).child(kit::dot(tone, 9.0)))
                .child(page::title(
                    theme,
                    title,
                    "One daemon per user, JSON-RPC over a Unix socket only you can open.",
                    false,
                ))
                .into_any_element(),
            page::table_frame(theme, page::table())
                .id("daemon-facts")
                .debug_selector(|| "daemon-facts".to_owned())
                .children(rows)
                .into_any_element(),
        ]
    }
}

/// The switches of the Notifications page, with the help line under each.
const NOTIFICATION_SWITCHES: [(Pref, &str, &str); 4] = [
    (
        Pref::Notifications,
        "Banners",
        "Show a banner when a run needs you, finishes or stops.",
    ),
    (
        Pref::BackgroundOnly,
        "Only when Reno is in the background",
        "Hold banners while a Reno window is the active window. A held banner is not shown later.",
    ),
    (
        Pref::Finished,
        "Finished runs",
        "Also show a banner when a run finishes. Actions that need you and runs that stop always do.",
    ),
    (
        Pref::DockBadge,
        "Dock badge",
        "Show how many runs need you on Reno's Dock icon.",
    ),
];

/// Whether banners can be shown here, and as whom.
pub(crate) const fn delivery_line(delivery: Delivery) -> &'static str {
    match delivery {
        Delivery::Native => "Banners show as Reno.",
        Delivery::Script => {
            "Reno is not running from Reno.app, so banners show as Script Editor and clicking them does nothing. Run cargo xtask bundle."
        }
        Delivery::None => "Banners cannot be shown here.",
    }
}

impl SettingsScreen {
    /// Banners, the badge, and what can be said about whether they will show here.
    fn notifications_page(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let banners = lifecycle::banners(cx);
        let (prefs, delivery) = banners.as_ref().map_or_else(
            || (Prefs::default(), Delivery::None),
            |b| {
                let b = b.read(cx);
                (b.prefs(cx), b.delivery())
            },
        );
        let rows = NOTIFICATION_SWITCHES.map(|(pref, label, help)| {
            let on = prefs.get(pref);
            let banners = banners.clone();
            page::table_row(theme, pref == Pref::DockBadge)
                .min_h(px(56.0))
                .gap(px(14.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(div().font_weight(FontWeight::MEDIUM).child(label))
                        .child(
                            div()
                                .text_size(px(12.5))
                                .line_height(px(18.0))
                                .text_color(theme.muted)
                                .child(help),
                        ),
                )
                .child(
                    kit::switch(theme, format!("notify-{}", pref.key()), on).on_click(cx.listener(
                        move |_, _, _, cx| {
                            if let Some(banners) = &banners {
                                banners.update(cx, |b, cx| b.set_pref(pref, !on, cx));
                            }
                            cx.notify();
                        },
                    )),
                )
                .into_any_element()
        });
        let delivery_line = delivery_line(delivery);
        let mut out = vec![
            page::title(
                theme,
                "Notifications",
                "Banners and the Dock badge for what needs you.",
                false,
            )
            .into_any_element(),
            page::table_frame(theme, page::table())
                .id("notification-switches")
                .children(rows)
                .into_any_element(),
        ];
        out.push(
            div()
                .id("notification-delivery")
                .debug_selector(|| "notification-delivery".to_owned())
                .text_size(px(13.0))
                .line_height(px(19.0))
                .text_color(theme.muted)
                .child(delivery_line)
                .into_any_element(),
        );
        out.push(
            div()
                .id("notification-quit-note")
                .text_size(px(13.0))
                .line_height(px(19.0))
                .text_color(theme.muted)
                .child(
                    "After you quit Reno (\u{2318}Q) there are no banners until you open it again.",
                )
                .into_any_element(),
        );
        out
    }
}

impl Render for SettingsScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = div().id("settings").size_full().flex().flex_col();
        if matches!(self.category, Category::Executors | Category::Registry) {
            // The executors entity owns its scroll and its banner.
            return page.child(self.executors.clone());
        }
        let theme = theme::current(cx);
        let defs = self.defs_view(cx);
        let banner: Option<Banner> = defs.banner.clone();
        let retry: page::Action = {
            let store = self.store.clone();
            Rc::new(move |_, cx| store.update(cx, |s, _| s.retry_now()))
        };
        let sections = match self.category {
            Category::Policy => {
                let policy = settings::policy(self.store.read(cx).state());
                self.policy_tab(policy.as_ref(), &theme, cx)
            }
            Category::Daemon => {
                let view = self.daemon_view(cx);
                self.daemon_page(&view, &theme)
            }
            Category::Notifications => self.notifications_page(&theme, cx),
            Category::Definitions | Category::Executors | Category::Registry => {
                self.definitions(&defs, &theme, cx)
            }
            Category::Accounts | Category::General | Category::Appearance => vec![],
        };
        let body = page::content(page::SETTINGS)
            .gap(px(30.0))
            .when_some(banner.as_ref(), |c, b| {
                c.child(page::banner(&theme, b, Some(retry)))
            })
            .children(sections);
        page.child(page::settings_column("settings-scroll").child(body))
    }
}
