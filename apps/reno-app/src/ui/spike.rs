//! The spike window: an opaque two-tone layout (shell and inset canvas) with a live tick counter,
//! exercising the theme tokens, glass plates, the count pill, the notice chip and motion.

use gpui::prelude::*;
use gpui::{
    Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Window, canvas, div,
    hsla, px,
};

use super::notice::{NoticeChipIcon, notice_chip};
use super::running_pill::running_pill;
use super::theme::{Appearance, Theme};
use super::{glass, motion};
use crate::model::{Event, Model};

#[derive(Debug)]
pub(crate) struct SpikeView {
    model: Entity<Model>,
    appearance_override: Option<Appearance>,
    _observe_model: Subscription,
    _observe_appearance: Subscription,
}

impl SpikeView {
    pub(crate) fn new(
        model: Entity<Model>,
        appearance_override: Option<Appearance>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            _observe_model: cx.observe(&model, |_, _, cx| cx.notify()),
            _observe_appearance: cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
            model,
            appearance_override,
        }
    }
}

impl Render for SpikeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let appearance = self
            .appearance_override
            .unwrap_or_else(|| Appearance::from(window.appearance()));
        let theme = Theme::for_appearance(appearance);
        let state = &self.model.read(cx).state;

        // Shell: the window background. Canvas: the inset content panel below.
        let root = div()
            .size_full()
            .bg(theme.shell)
            .p(px(10.0))
            .pt(px(38.0))
            .text_color(theme.text);

        let row = |label: &'static str, value: String| {
            div()
                .flex()
                .justify_between()
                .gap(px(24.0))
                .child(div().text_color(theme.muted).child(label))
                .child(div().text_color(theme.text).child(value))
        };

        let panel = div()
            .size_full()
            .p(px(20.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .rounded(px(14.0))
            .bg(theme.canvas)
            .border_1()
            .border_color(theme.hairline)
            .text_size(px(13.5))
            .child(
                div()
                    .text_size(px(20.0))
                    .text_color(theme.text)
                    .child("Reno GPUI spike"),
            )
            .child(row("ticks", state.count.to_string()))
            .child(row("resets", state.resets.to_string()))
            .child(row("appearance", format!("{:?}", theme.appearance)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().text_color(theme.muted).child("running"))
                    .child(running_pill("spike-pill", 3, theme.accent, "Menlo")),
            )
            .child(
                div()
                    .rounded(px(10.0))
                    .bg(theme.card)
                    .border_1()
                    .border_color(theme.hairline)
                    .p(px(10.0))
                    .child(
                        div()
                            .text_color(theme.muted)
                            .child("A raised card: card fill with a hairline."),
                    ),
            )
            .child(motion::fade_in(
                "spike-notice",
                notice_chip(
                    &theme,
                    true,
                    "Held for you",
                    "The push to origin needs approval.",
                    NoticeChipIcon::Plain,
                ),
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .pt(px(6.0))
                    .child(
                        glass::light(div(), &theme, 1.0)
                            .id("reset")
                            .px(px(12.0))
                            .py(px(5.0))
                            .rounded(px(999.0))
                            .text_color(theme.text)
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.model.update(cx, |m, cx| m.dispatch(Event::Reset, cx));
                            }))
                            .child("reset"),
                    )
                    .child(
                        glass::accent_plate(&theme, 1.0, 0.0)
                            .apply(div())
                            .px(px(12.0))
                            .py(px(5.0))
                            .rounded(px(999.0))
                            .text_color(hsla(0.0, 0.0, 1.0, 1.0))
                            .child("live"),
                    )
                    .child(glass::thumb(div(), &theme).size(px(16.0)).rounded(px(8.0)))
                    // The canvas path: a plate painted directly rather than via styles.
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, (), window, _| {
                                glass::light_plate(&theme, 1.0).paint(window, bounds, 8.0);
                            },
                        )
                        .w(px(40.0))
                        .h(px(16.0)),
                    ),
            );

        root.child(panel)
    }
}
