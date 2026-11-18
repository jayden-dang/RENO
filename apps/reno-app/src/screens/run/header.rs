//! The chat header: where the run is (project, executor, title), its state, Stop, the tokens chip
//! and the panel toggle.

use gpui::{AnyElement, Context, FontWeight, IntoElement, SharedString, div, prelude::*, px};

use super::{RunScreen, ScreenEvent};
use crate::panel::surface::Surface;
use crate::summary;
use crate::ui::icon::Icon;
use crate::ui::kit;
use crate::ui::theme::Theme;
use crate::vm::panel_home;

impl RunScreen {
    pub(super) fn header(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let run = self.summary(cx).cloned();
        let (executor, what, status) = match &run {
            Some(r) => (
                r.executor.as_str().to_owned(),
                summary::parse_title(&r.title).what.to_owned(),
                Some((
                    kit::run_color(theme, r.status, summary::reported_problem(r)),
                    summary::run_status_label(r),
                )),
            ),
            None => (String::new(), format!("Run {}", self.run), None),
        };
        let run_id = self.run.clone();
        let project = {
            let store = self.store.read(cx);
            crate::vm::projects::key_of(run.as_ref(), |dir| store.root_of(dir))
        };

        div()
            .flex_none()
            .h(px(52.0))
            .px(px(22.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .border_b_1()
            .border_color(theme.hairline)
            .child(
                div()
                    .id("crumb-project")
                    .debug_selector(|| "crumb-project".to_owned())
                    .text_color(theme.muted)
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.text))
                    .child(project.name())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(ScreenEvent::OpenProject(project.clone()));
                    })),
            )
            .child(div().text_color(theme.faint).child("/"))
            .when(!executor.is_empty(), |h| {
                h.child(kit::exec_glyph(theme, &executor, 16.0))
            })
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(what),
            )
            .when_some(status, |h, status| {
                h.child(
                    kit::status_pill_worded(status.0, status.1)
                        .id(SharedString::from(format!("status-{run_id}"))),
                )
            })
            .child(div().flex_1())
            .children(self.stop_button(theme, cx))
            .children(self.usage_chip(theme, cx))
            .child(
                kit::icon_button(theme, "details-toggle", Icon::Sidebar, 30.0)
                    .debug_selector(|| "details-toggle".to_owned())
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_panel(window, cx))),
            )
    }

    /// The header's Stop, on a run that can be stopped.
    pub(super) fn stop_button(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Anything not finished can be stopped, a run waiting on you included.
        let stoppable = self
            .summary(cx)
            .is_some_and(|r| !r.status.is_terminal() && !summary::stopped_by_person(r));
        if !stoppable {
            return None;
        }
        let run = self.run.clone();
        Some(
            crate::ui::kit::line_button(
                theme,
                "header-stop",
                Some(crate::ui::icon::Icon::Stop),
                "Stop",
                crate::ui::kit::Tone::Quiet,
                true,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let run = run.clone();
                this.store.update(cx, |store, cx| store.cancel_run(run, cx));
            }))
            .into_any_element(),
        )
    }

    /// The tokens chip: a ring and "1.4k". Hidden until there are tokens; it opens Session.
    pub(super) fn usage_chip(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let usage = self.facts.usage.filter(|u| u.total() > 0)?;
        let hover = theme.text.opacity(0.06);
        Some(
            div()
                .id("usage-chip")
                .debug_selector(|| "usage-chip".to_owned())
                .flex_none()
                .h(px(30.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .rounded(px(8.0))
                .text_size(px(12.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(crate::ui::icon::icon(
                    crate::ui::icon::Icon::Ring,
                    14.0,
                    theme.muted,
                ))
                .child(panel_home::usage_chip(usage))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_surface(Surface::Session, window, cx);
                }))
                .into_any_element(),
        )
    }
}
