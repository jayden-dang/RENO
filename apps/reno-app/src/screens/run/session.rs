//! The Session tab: today's details pane, regrouped (`vm::panel_home::session` decides what the
//! rows say; this draws them). The executor-health block, with its two buttons, sits above Run.

use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

use super::super::executors::health_lines;
use super::RunScreen;
use crate::panel::view::{finding_text, tone_ink};
use crate::summary;
use crate::ui::kit;
use crate::ui::theme::Theme;
use crate::vm::changes::Loaded;
use crate::vm::executors as executor_facts;
use crate::vm::panel_home::{self, KvRow, SessionFacts};

impl RunScreen {
    pub(super) fn session_body(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let body = div()
            .id("details")
            .debug_selector(|| "details".to_owned())
            .size_full()
            .overflow_y_scroll()
            .px(px(22.0))
            .py(px(18.0))
            .flex()
            .flex_col();
        let (sections, failed, finding, executor) = {
            let store = self.store.read(cx);
            let Some(run) = store.state().run(&self.run) else {
                return body
                    .child(kv_row(
                        theme,
                        &KvRow {
                            label: "State",
                            value: "starting".to_owned(),
                            sub: None,
                        },
                        None,
                        "",
                    ))
                    .into_any_element();
            };
            let now = store.now_ms();
            let facts = SessionFacts {
                run,
                model: self.model_label(cx).map(|(_, long)| long),
                access: run.bot.as_ref().map(|_| {
                    crate::vm::runs::access_label(store.state(), &self.run)
                        .unwrap_or("\u{2026}")
                        .to_owned()
                }),
                repo: summary::run_repo(run),
                workspace: store.changes(&self.run).and_then(|c| match &c.workspace {
                    Loaded::Ready(Some(info)) => Some(info),
                    _ => None,
                }),
                usage: self.facts.usage,
                cache_tokens: None,
                executor_health: false,
                now_ms: now,
            };
            // A run its executor could not take shows that executor's health, as the daemon has
            // it now, so the reason and the way out are on the same screen.
            let failed = executor_facts::failed_executor(store.state(), &self.run, now);
            let facts = SessionFacts {
                executor_health: failed.is_some(),
                ..facts
            };
            (
                panel_home::session(&facts),
                failed,
                self.home(cx),
                run.executor.as_str().to_owned(),
            )
        };

        let heading = |text: &'static str| {
            div()
                .pt(px(18.0))
                .pb(px(4.0))
                .text_size(px(12.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.faint)
                .child(text)
        };
        let state_ink = tone_ink(theme, sections.tone);
        let state_dot = match sections.tone {
            panel_home::NowTone::Done => theme.success,
            other => tone_ink(theme, other),
        };
        let group = |rows: &[KvRow]| {
            rows.iter()
                .map(|row| kv_row(theme, row, Some((state_dot, state_ink)), &executor))
                .collect::<Vec<_>>()
        };

        let finding_block = finding.map(|home| {
            div()
                .id("session-finding")
                .debug_selector(|| "session-finding".to_owned())
                .px(px(14.0))
                .py(px(12.0))
                .rounded(px(10.0))
                .bg(theme.text.opacity(0.06))
                .text_size(px(13.5))
                .line_height(px(20.0))
                .child(finding_text(theme, &home.now, theme.muted))
        });

        body.children(finding_block)
            .children(failed.map(|failed| {
                div()
                    .flex()
                    .flex_col()
                    .child(heading("Executor health"))
                    .child(health_lines(theme, &failed.row))
                    .child(self.executor_actions(&failed.row, theme, cx))
            }))
            .child(heading("Run"))
            .children(group(&sections.run))
            .when(!sections.workspace.is_empty(), |d| {
                d.child(heading("Workspace"))
                    .children(group(&sections.workspace))
            })
            .child(heading("Usage"))
            .children(group(&sections.usage))
            .into_any_element()
    }
}

/// One `kv` row: a muted label and the value, with a smaller line under it. The State row's
/// value carries the state's dot.
fn kv_row(
    theme: &Theme,
    row: &KvRow,
    state: Option<(gpui::Hsla, gpui::Hsla)>,
    executor: &str,
) -> AnyElement {
    let selector = format!("details-{}", row.label.to_lowercase());
    let value = div()
        .flex_1()
        .min_w_0()
        .text_size(px(13.5))
        .child(match (row.label, state) {
            ("State", Some((dot, ink))) => div()
                .flex()
                .items_center()
                .gap(px(7.0))
                .child(kit::dot(dot, 7.0))
                .child(
                    div()
                        .text_color(ink)
                        .child(SharedString::from(row.value.clone())),
                ),
            ("Executor", _) => div()
                .flex()
                .items_center()
                .gap(px(7.0))
                .child(kit::exec_glyph(theme, executor, 14.0))
                .child(SharedString::from(row.value.clone())),
            _ => div().child(SharedString::from(row.value.clone())),
        })
        .children(row.sub.as_ref().map(|sub| {
            div()
                .text_size(px(12.0))
                .text_color(theme.faint)
                .child(SharedString::from(sub.clone()))
        }));
    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex()
        .items_start()
        .gap(px(12.0))
        .py(px(6.0))
        .child(
            div()
                .flex_none()
                .w(px(86.0))
                .text_size(px(13.5))
                .text_color(theme.muted)
                .child(row.label),
        )
        .child(value)
        .into_any_element()
}
