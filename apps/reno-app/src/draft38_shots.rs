//! Screenshots of the ROAD-38 draft, rendered off-screen (the real Metal renderer, a window that
//! is never shown), so they do not depend on what else is on the screen. Built only with the
//! `shots` feature, and run with one shot named:
//!
//! `SHOT=a-welcome SHOT_DIR=/tmp/shots cargo run -p reno-app --features shots`
//!
//! A shot is `<variant>-<state>`: states are `welcome`, `agents`, `project`, `done`, `gate`,
//! `restart`, `restart-confirm`, `restart-fail`, `updates`, `menu`; `light` adds the light theme.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::sync::Arc;
use std::time::Duration;

use gpui::{AnyWindowHandle, AppContext as _, VisualTestAppContext, point, px, size};

use crate::app::{RootView, register_fonts};
use crate::prefs::PrefStore;
use crate::store::Store;
use crate::ui::draft38::{self, Hooks};
use crate::ui::icon::Assets;
use crate::ui::motion::{self, ReduceMotion};
use crate::ui::theme::Appearance;
use crate::vm::draft38::Variant;

/// Lets the app and the fake daemon (which is on real time) get on with it.
fn pump(cx: &mut VisualTestAppContext, ms: u64) {
    let mut left = ms;
    while left > 0 {
        let step = left.min(50);
        cx.advance_clock(Duration::from_millis(step));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(step.min(25)));
        left -= step;
    }
}

pub(crate) fn run() {
    let Ok(shot) = std::env::var("SHOT") else {
        return;
    };
    let dir = std::env::var("SHOT_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let (key, state) = shot.split_once('-').expect("SHOT is <variant>-<state>");
    let variant = Variant::parse(key).expect("variant a, b or c");
    let light = state.ends_with("-light") || state == "light";
    let state = state.trim_end_matches("-light");

    let platform = gpui_platform::current_platform(false);
    let mut cx = VisualTestAppContext::with_asset_source(platform, Arc::new(Assets));
    let first_run = match state {
        "agents" => Some("agents"),
        "project" | "project-empty" | "project-known" | "imported-git" | "imported-plain"
        | "import-error" => Some("project"),
        "done" => Some("done"),
        _ => None,
    };
    // The folder the panel is taken to answer with, and whether Reno knows no project yet.
    let import = match state {
        "imported-git" => Some("/demo/reno"),
        "imported-plain" | "done" => Some("/demo/notes"),
        "import-error" => Some("/demo/todo.txt"),
        _ => None,
    };
    let no_projects = matches!(state, "project-empty" | "done");
    let skip_setup = !(first_run.is_some() || matches!(state, "welcome" | "light" | "gate"));
    let hold = state == "gate";
    let store = cx.update(|app| {
        gpui_tokio::init(app);
        register_fonts(app);
        motion::init(ReduceMotion::On, false, app);
        crate::actions::bind_keys(app);
        crate::ui::composer::bind_keys(app);
        app.set_global(PrefStore::in_memory());
        crate::links::mute();
        draft38::install(
            variant,
            Hooks {
                first_run: first_run.map(str::to_owned),
                gate_hold: hold,
                import: import.map(str::to_owned),
            },
            app,
        );
        app.new(|cx| {
            let mut store = Store::connecting();
            store.start_fake_first_run(no_projects, cx);
            store
        })
    });
    let toasts = cx.update(crate::ui::toast::install);
    let appearance = if light {
        Appearance::Light
    } else {
        Appearance::Dark
    };
    // `SHOT_SIZE=1100x700` tries the smallest window.
    let (width, height) = std::env::var("SHOT_SIZE")
        .ok()
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1480.0, 960.0));
    let handle = cx
        .open_offscreen_window(size(px(width), px(height)), |window, app| {
            let root = app.new(|cx| {
                RootView::new(store.clone(), toasts.clone(), Some(appearance), window, cx)
            });
            root.update(app, |root, cx| root.init_draft38(skip_setup, window, cx));
            root
        })
        .unwrap();
    let any: AnyWindowHandle = handle.into();
    // Connected, and the gate gone (or held).
    // The gate shot is taken early, while the daemon is still being read.
    pump(&mut cx, if hold { 500 } else { 3_200 });
    let act =
        |cx: &mut VisualTestAppContext,
         f: &dyn Fn(&mut RootView, &mut gpui::Window, &mut gpui::Context<RootView>)| {
            handle
                .update(cx, |root, window, cx| f(root, window, cx))
                .unwrap();
        };
    match state {
        "restart" if variant == Variant::C => {
            act(&mut cx, &|r, w, cx| r.draft38_hook("restart-card", w, cx))
        }
        "restart-confirm" => act(&mut cx, &|r, w, cx| r.draft38_hook("restart", w, cx)),
        "restart-fail" => {
            act(&mut cx, &|r, w, cx| r.draft38_hook("restart-fail", w, cx));
            pump(&mut cx, 2_400);
        }
        "updates" => act(&mut cx, &|r, w, cx| r.draft38_hook("updates", w, cx)),
        "menu" => act(&mut cx, &|r, w, cx| {
            let thread = reno_proto::ThreadId::from_suffix("plan1").unwrap();
            r.sidebar_act(
                crate::ui::sidebar::Act::Menu(thread, Some(point(px(150.0), px(200.0)))),
                w,
                cx,
            );
        }),
        _ => {}
    }
    pump(&mut cx, if hold { 100 } else { 700 });
    // Draw the frame, then read it back.
    cx.update_window(any, |_, window, app| {
        window.draw(app).clear(app);
    })
    .unwrap();
    let image = cx.capture_screenshot(any).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let path = format!("{dir}/{shot}.png");
    image.save(&path).unwrap();
    eprintln!("saved {path}");
}
