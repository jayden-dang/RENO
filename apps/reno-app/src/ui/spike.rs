//! The spike window: a translucent panel on the window material, with a live tick counter.

use gpui::prelude::*;
use gpui::{
    Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Window, canvas, div,
    hsla, linear_color_stop, linear_gradient, px,
};

use super::glass;
use super::theme::{Appearance, Theme};
use crate::model::{Event, Model};

/// What paints behind the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Material {
    /// System blur (`NSVisualEffectView` on macOS).
    Blurred,
    /// The named fallback: an app-painted gradient on a transparent window.
    Gradient,
}

#[derive(Debug)]
pub(crate) struct SpikeView {
    model: Entity<Model>,
    material: Material,
    appearance_override: Option<Appearance>,
    _observe_model: Subscription,
    _observe_appearance: Subscription,
}

impl SpikeView {
    pub(crate) fn new(
        model: Entity<Model>,
        material: Material,
        appearance_override: Option<Appearance>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            _observe_model: cx.observe(&model, |_, _, cx| cx.notify()),
            _observe_appearance: cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
            model,
            material,
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

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(72.0));
        if self.material == Material::Gradient {
            root = root.bg(linear_gradient(
                135.0,
                linear_color_stop(hsla(0.62, 0.55, 0.30, 0.95), 0.0),
                linear_color_stop(hsla(0.85, 0.50, 0.38, 0.95), 1.0),
            ));
        }

        let row = |label: &'static str, value: String| {
            div()
                .flex()
                .justify_between()
                .gap(px(24.0))
                .child(div().text_color(theme.muted).child(label))
                .child(div().text_color(theme.text).child(value))
        };

        let panel = div()
            .w(px(340.0))
            .p(px(20.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .rounded(px(16.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.line)
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
            .child(row("material", format!("{:?}", self.material)))
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
