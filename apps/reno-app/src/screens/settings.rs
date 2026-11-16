//! Settings: the definitions in force with their problems (live through `defs.watch`), the
//! policy's rules, and the daemon's status.

use std::rc::Rc;

use gpui::{
    AnyElement, Context, Entity, FontWeight, IntoElement, Render, SharedString, Subscription,
    Window, div, prelude::*, px,
};
use reno_client::ConnectionStatus;
use reno_proto::defs::DefKind;

use crate::store::Store;
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::page;
use crate::ui::theme::{self, Theme};
use crate::vm::rules;
use crate::vm::settings::{self, DaemonView, DefRow, DefsView, PolicyView};
use crate::vm::{Banner, Section};

/// The tabs of the page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Tab {
    #[default]
    Definitions,
    Policy,
    Daemon,
}

impl Tab {
    pub(crate) const ALL: [Self; 3] = [Self::Definitions, Self::Policy, Self::Daemon];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Definitions => "Definitions",
            Self::Policy => "Policy",
            Self::Daemon => "Daemon",
        }
    }
}

/// The policy table's fixed cells, kept narrow so the Matches column (which flexes, and holds a
/// command glob) takes the rest of the row. Rows and header share [`page::cell`], so they align.
const EFFECT_W: f32 = 64.0;
const SCOPE_W: f32 = 120.0;
const NOTE_W: f32 = 170.0;

pub(crate) struct SettingsScreen {
    store: Entity<Store>,
    tab: Tab,
    _store: Subscription,
}

impl SettingsScreen {
    pub(crate) fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
            tab: Tab::Definitions,
        }
    }

    #[cfg(test)]
    pub(crate) fn tab(&self) -> Tab {
        self.tab
    }

    pub(crate) fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        if tab == Tab::Daemon {
            self.shown(cx);
        }
        if tab == Tab::Policy {
            self.store.update(cx, |store, cx| store.load_rules(cx));
        }
        cx.notify();
    }

    /// Called when the screen is shown: `daemon.status` has no watch, so it is read now.
    pub(crate) fn shown(&self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, _| store.refresh_status());
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

    fn tabs(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .gap(px(4.0))
            .children(Tab::ALL.map(|tab| {
                let on = tab == self.tab;
                div()
                    .id(SharedString::from(format!("tab-{}", tab.label())))
                    .debug_selector(move || format!("tab-{}", tab.label()))
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
            .gap(px(10.0))
            .child(page::section_label(theme, label))
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
        let mut out = vec![page::lede(theme, title, detail).into_any_element()];
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
        out.push(self.defs_table("BOTS", &view.bots, "No bots defined.", theme, cx));
        out.push(self.defs_table(
            "TRIGGERS",
            &view.triggers,
            "No triggers defined.",
            theme,
            cx,
        ));
        if !view.policy.is_empty() {
            let policy = Section::Rows(view.policy.clone());
            out.push(self.defs_table("POLICY", &policy, "", theme, cx));
        }
        out
    }

    /// The standing rules: answers kept from the Inbox (or `reno approve --remember`), each
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
            .child(page::section_label(theme, "YOUR STANDING RULES"))
            .child(
                div()
                    .text_size(px(13.0))
                    .line_height(px(19.0))
                    .text_color(theme.muted)
                    .child(
                        "Answers you kept from the Inbox. Each holds exactly the action it came from, and a deny rule in the policy file always wins over it.",
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
                page::lede(
                    theme,
                    "No policy yet.",
                    "Without a policy every action asks a human, and an unanswered ask is denied.",
                )
                .into_any_element(),
                self.standing_rules(theme, cx),
            ];
        };
        let ttl = view.ttl.as_ref().map_or_else(String::new, |t| {
            format!(" Unanswered asks expire after {t}.")
        });
        let mut out = vec![
            page::lede(
                theme,
                "What bots may do without asking.",
                format!(
                    "The first rule that matches decides; when none does, the answer is \"{}\".{ttl}",
                    view.default
                ),
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
                .children(rows)
                .into_any_element(),
        );
        out.push(self.standing_rules(theme, cx));
        out
    }

    fn daemon_tab(&self, view: &DaemonView, theme: &Theme) -> Vec<AnyElement> {
        let (title, tone) = match view.connection {
            ConnectionStatus::Connected => ("renod is running.", theme.success),
            ConnectionStatus::Connecting => ("Connecting to renod\u{2026}", theme.warning),
            ConnectionStatus::Disconnected => ("renod is not reachable.", theme.danger),
        };
        let kv = |label: &'static str, value: String| {
            div()
                .flex()
                .items_start()
                .gap(px(14.0))
                .py(px(8.0))
                .child(
                    div()
                        .flex_none()
                        .w(px(120.0))
                        .text_color(theme.muted)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(value))
        };
        let dash = || "\u{2014}".to_owned();
        let pair = |a: Option<u32>, b: Option<u32>| match (a, b) {
            (Some(a), Some(b)) => format!("{a} active \u{b7} {b} queued"),
            _ => dash(),
        };
        vec![
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(kit::dot(tone, 9.0))
                .child(page::lede(
                    theme,
                    title,
                    "One daemon per user, JSON-RPC over a Unix socket only you can open.",
                ))
                .into_any_element(),
            div()
                .id("daemon-facts")
                .flex()
                .flex_col()
                .child(kv("Version", view.version.clone().unwrap_or_else(dash)))
                .child(kv(
                    "Protocol",
                    view.protocol.map_or_else(dash, |p| p.to_string()),
                ))
                .child(kv("Process", view.pid.map_or_else(dash, |p| p.to_string())))
                .child(kv("Uptime", view.uptime.clone().unwrap_or_else(dash)))
                .child(kv("Runs", pair(view.active, view.queued)))
                .child(kv(
                    "Executors",
                    if view.executors.is_empty() {
                        dash()
                    } else {
                        view.executors.join(", ")
                    },
                ))
                .child(kv(
                    "Socket",
                    view.socket
                        .clone()
                        .unwrap_or_else(|| "in-process (demo)".to_owned()),
                ))
                .child(kv(
                    "Features",
                    if view.features.is_empty() {
                        dash()
                    } else {
                        view.features.join(", ")
                    },
                ))
                .into_any_element(),
        ]
    }
}

impl Render for SettingsScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let defs = self.defs_view(cx);
        let banner: Option<Banner> = defs.banner.clone();
        let retry: page::Action = {
            let store = self.store.clone();
            Rc::new(move |_, cx| store.update(cx, |s, _| s.retry_now()))
        };
        let sections = match self.tab {
            Tab::Definitions => self.definitions(&defs, &theme, cx),
            Tab::Policy => {
                let policy = settings::policy(self.store.read(cx).state());
                self.policy_tab(policy.as_ref(), &theme, cx)
            }
            Tab::Daemon => {
                let view = self.daemon_view(cx);
                self.daemon_tab(&view, &theme)
            }
        };
        let body = page::content(page::WIDE)
            .when_some(banner.as_ref(), |c, b| {
                c.child(page::banner(&theme, b, Some(retry)))
            })
            .children(sections);
        div()
            .id("settings")
            .size_full()
            .flex()
            .flex_col()
            .child(page::header(&theme, "Settings", self.tabs(&theme, cx)))
            .child(page::column("settings-scroll").child(body))
    }
}
