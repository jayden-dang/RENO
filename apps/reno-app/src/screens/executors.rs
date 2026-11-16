//! Executors: each agent CLI Reno drives, with what the daemon's last probe found out about it:
//! installed (where, which version), whether that version is one the adapter was verified on,
//! whether it is signed in, what it can do and how fresh all of this is. What needs the human
//! (signed out, too old, needs an install) is listed first. Live through `executors.watch`.
//!
//! "Check again" is `executor.refresh` and "Install" is `executor.install`, which fetches the
//! managed adapter; nothing probes on a timer and a run never installs.

use std::rc::Rc;

use gpui::{
    AnyElement, Context, Div, Entity, EventEmitter, FontWeight, IntoElement, Render, SharedString,
    Subscription, Window, div, prelude::*, px,
};
use reno_proto::ExecutorId;

use super::ScreenEvent;
use crate::store::Store;
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::page;
use crate::ui::theme::{self, Theme};
use crate::vm::executors::{self, ExecutorRow, ExecutorsView, Fact, Level, Need};

pub(crate) struct ExecutorsScreen {
    store: Entity<Store>,
    _store: Subscription,
}

impl EventEmitter<ScreenEvent> for ExecutorsScreen {}

fn level_color(theme: &Theme, level: Level) -> gpui::Hsla {
    match level {
        Level::Good => theme.success,
        Level::Warn => theme.warning,
        Level::Bad => theme.danger,
        Level::Quiet => theme.faint,
    }
}

/// One fact of an executor's health: a dot, what it is, and the detail beside it.
fn fact_line(theme: &Theme, fact: &Fact) -> Div {
    let color = level_color(theme, fact.level);
    div()
        .flex()
        .items_start()
        .gap(px(8.0))
        .child(div().pt(px(6.0)).child(kit::dot(color, 7.0)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_wrap()
                .gap_x(px(7.0))
                .line_height(px(19.0))
                .child(
                    div()
                        .text_color(match fact.level {
                            Level::Good | Level::Quiet => theme.text,
                            Level::Warn | Level::Bad => color,
                        })
                        .child(fact.text),
                )
                .when_some(fact.detail.clone(), |d, detail| {
                    d.child(
                        div()
                            .min_w_0()
                            .text_size(px(12.5))
                            .text_color(theme.muted)
                            .child(SharedString::from(detail)),
                    )
                }),
        )
}

/// The three health lines of an executor: installed, version, signed in. Also shown inline on a
/// run that failed because of its executor.
pub(crate) fn health_lines(theme: &Theme, row: &ExecutorRow) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(fact_line(theme, &row.installed))
        .children(row.version.as_ref().map(|f| fact_line(theme, f)))
        .children(row.signed_in.as_ref().map(|f| fact_line(theme, f)))
}

fn need_tone(theme: &Theme, need: Need) -> gpui::Hsla {
    match need {
        Need::SignIn | Need::Update => theme.warning,
        Need::Install | Need::Setup => theme.danger,
    }
}

impl ExecutorsScreen {
    pub(crate) fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
        }
    }

    pub(crate) fn view(&self, cx: &gpui::App) -> ExecutorsView {
        let store = self.store.read(cx);
        executors::build(store.state(), store.now_ms())
    }

    /// "Check again" for one executor, or for all of them.
    pub(crate) fn check(&mut self, id: Option<ExecutorId>, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, cx| store.refresh_executors(id, cx));
    }

    pub(crate) fn install(&mut self, id: ExecutorId, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, cx| store.install_executor(id, cx));
    }

    fn actions(&self, row: &ExecutorRow, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Ok(id) = row.id.parse::<ExecutorId>() else {
            return div().into_any_element();
        };
        let store = self.store.read(cx);
        let installing = store.is_installing(&id);
        let checking = store.is_checking(Some(&id)) || store.is_checking(None);

        let check = {
            let id = id.clone();
            cx.listener(move |this, _, _, cx| this.check(Some(id.clone()), cx))
        };
        let check_button = kit::pill_button(
            theme,
            format!("check-{}", row.id),
            if checking {
                "Checking\u{2026}"
            } else {
                "Check again"
            },
            Tone::Quiet,
        );
        let check_button = if checking || installing {
            check_button.opacity(0.4).cursor_default()
        } else {
            check_button.on_click(check)
        };

        let install_button = row.can_install.then(|| {
            let install = {
                let id = id.clone();
                cx.listener(move |this, _, _, cx| this.install(id.clone(), cx))
            };
            let button = kit::pill_button(
                theme,
                format!("install-{}", row.id),
                if installing {
                    "Installing\u{2026}"
                } else {
                    "Install"
                },
                Tone::Primary,
            );
            if installing {
                button.opacity(0.6).cursor_default()
            } else {
                button.on_click(install)
            }
        });

        div()
            .flex_none()
            .flex()
            .flex_col()
            .items_end()
            .gap(px(8.0))
            .children(install_button)
            .child(check_button)
            .into_any_element()
    }

    fn row(
        &self,
        row: &ExecutorRow,
        last: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Label over value: the column is narrow at the window's minimum width, and a value
        // such as "Unrestricted" must never break inside a word.
        let kv = |label: &'static str, value: String| {
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.faint)
                        .child(label),
                )
                .child(
                    div()
                        .min_w_0()
                        .text_size(px(12.5))
                        .line_height(px(18.0))
                        .text_color(theme.muted)
                        .child(SharedString::from(value)),
                )
        };
        let list = |items: &[&'static str]| {
            if items.is_empty() {
                "\u{2014}".to_owned()
            } else {
                items.join(", ")
            }
        };

        page::table_row(theme, last)
            .id(SharedString::from(format!("executor-{}", row.id)))
            .items_start()
            .child(
                div()
                    .map(|d| page::cell(d, Some(150.0)))
                    .flex()
                    .flex_col()
                    .gap(px(5.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(kit::exec_glyph(theme, &row.id, 16.0))
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .truncate()
                                    .child(SharedString::from(row.name.clone())),
                            ),
                    )
                    .when(row.name != row.id, |d| {
                        d.child(
                            div()
                                .font_family(MONO)
                                .text_size(px(11.5))
                                .text_color(theme.faint)
                                .truncate()
                                .child(SharedString::from(row.id.clone())),
                        )
                    })
                    .when_some(row.need, |d, need| {
                        d.child(
                            div()
                                .flex()
                                .child(kit::tag(need_tone(theme, need), need.label())),
                        )
                    })
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.faint)
                            .child(SharedString::from(row.checked.clone())),
                    ),
            )
            .child(
                div()
                    .map(|d| page::cell(d, None))
                    .child(health_lines(theme, row)),
            )
            .child(
                div()
                    .map(|d| page::cell(d, Some(200.0)))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(kv("Access", list(&row.access)))
                    .child(kv("Sandbox", list(&row.sandbox)))
                    .child(kv("Models", row.models.clone())),
            )
            .child(self.actions(row, theme, cx))
            .into_any_element()
    }
}

impl Render for ExecutorsScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let view = self.view(cx);
        let retry: page::Action = {
            let store = self.store.clone();
            Rc::new(move |_, cx| store.update(cx, |s, _| s.retry_now()))
        };
        let rows = view.rows.rows();
        let table_rows: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| self.row(r, i + 1 == rows.len(), &theme, cx))
            .collect();

        let checking_all = self.store.read(cx).is_checking(None);
        let check_all = {
            let button = kit::pill_button(
                &theme,
                "check-all",
                if checking_all {
                    "Checking\u{2026}"
                } else {
                    "Check all again"
                },
                Tone::Quiet,
            );
            if checking_all || rows.is_empty() {
                button.opacity(0.4).cursor_default()
            } else {
                button.on_click(cx.listener(|this, _, _, cx| this.check(None, cx)))
            }
        };

        let body = page::content(page::WIDE)
            .when_some(view.banner.as_ref(), |c, b| {
                c.child(page::banner(&theme, b, Some(retry)))
            })
            .child(page::lede(&theme, view.lede.clone(), view.detail.clone()))
            .when_some(
                page::note(
                    &theme,
                    &view.rows,
                    "The daemon lists no executors. Start it with an agent CLI installed.",
                ),
                ParentElement::child,
            )
            .when(!table_rows.is_empty(), |c| {
                c.child(
                    page::table_frame(&theme, page::table())
                        .child(page::table_head(
                            &theme,
                            &[
                                ("Executor", Some(150.0)),
                                ("Health", None),
                                ("Can do", Some(230.0)),
                                ("", Some(110.0)),
                            ],
                        ))
                        .children(table_rows),
                )
            });

        div()
            .id("executors")
            .size_full()
            .flex()
            .flex_col()
            .child(page::header(&theme, "Executors", check_all))
            .child(page::column("executors-scroll").child(body))
    }
}
