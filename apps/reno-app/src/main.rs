//! T0.10 GPUI spike: window material, translucent panel, tokio bridge, headless model test.

mod bridge;
mod model;
mod ui;

use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    App, Bounds, TitlebarOptions, WindowBackgroundAppearance, WindowBounds, WindowOptions, point,
    px, size,
};
use gpui_platform::application;

use model::Model;
use ui::spike::{Material, SpikeView};
use ui::theme::Appearance;

fn main() {
    // `RENO_MATERIAL=gradient` selects the app-painted fallback.
    let material = match std::env::var("RENO_MATERIAL").as_deref() {
        Ok("gradient") => Material::Gradient,
        _ => Material::Blurred,
    };
    // `RENO_APPEARANCE=light|dark` previews the panel palette without touching system settings.
    let appearance_override = match std::env::var("RENO_APPEARANCE").as_deref() {
        Ok("light") => Some(Appearance::Light),
        Ok("dark") => Some(Appearance::Dark),
        _ => None,
    };
    // `RENO_QUIT_AFTER_MS` exercises the clean-quit path without a human.
    let quit_after = std::env::var("RENO_QUIT_AFTER_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis);

    application().run(move |cx: &mut App| {
        gpui_tokio::init(cx);

        let model = cx.new(|_| Model::default());
        bridge::start_ticker(&model, cx);

        let background = match material {
            Material::Blurred => WindowBackgroundAppearance::Blurred,
            Material::Gradient => WindowBackgroundAppearance::Transparent,
        };
        let bounds = Bounds::centered(None, size(px(520.0), px(420.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Reno".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(14.0), px(14.0))),
                }),
                window_background: background,
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| SpikeView::new(model, material, appearance_override, window, cx))
            },
        );
        if opened.is_err() {
            cx.quit();
            return;
        }

        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        if let Some(after) = quit_after {
            cx.spawn(async move |cx| {
                cx.background_executor().timer(after).await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
        cx.activate(true);
    });
}
