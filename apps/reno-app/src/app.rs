//! The shell: window setup and the root view.
//!
//! The window is opaque and two-tone (UI brief amendment 6): the window, sidebar and rail sit on
//! `shell`; every page sits in one inset rounded `canvas` panel with a hairline. The titlebar is
//! transparent so the traffic lights float over `shell`. Light and dark follow the system
//! appearance unless `RENO_APPEARANCE` or the palette's "Toggle theme" says otherwise.

use std::borrow::Cow;
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    Subscription, TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds, WindowOptions,
    div, point, prelude::*, px, size,
};
use gpui_platform::application;
use reno_proto::{FiringOutcome, RunId};

use crate::actions::{
    self, ClosePanelTab, GoRuns, JumpToPanel, NewPanelTab, NextPanelTab, OpenPalette, PrevPanelTab,
    Quit, ReopenPanelTab, TogglePanel, ToggleSidebar, ToggleTheme,
};
use crate::lifecycle;
use crate::nav::Nav;
use crate::panel::layout::GUTTER;
use crate::panel::surface::Surface;
use crate::screens::ScreenEvent;
use crate::screens::executors::ExecutorsScreen;
use crate::screens::home::HomeScreen;
use crate::screens::run::{PanelAction, RunScreen};
use crate::screens::runs::RunsScreen;
use crate::screens::settings::{NavActions, SettingsScreen};
use crate::screens::triggers::TriggersScreen;
use crate::store::{Store, StoreEvent};
use crate::ui::bar::{self, Anchor_, Bar};
use crate::ui::composer;
use crate::ui::icon::Assets;
use crate::ui::kit::SANS;
use crate::ui::motion::{self, ReduceMotion};
use crate::ui::overlay;
use crate::ui::palette::{Palette, PaletteAction, PaletteEvent};
use crate::ui::sidebar::{self, SidebarHandlers};
use crate::ui::theme::{self, Appearance, Theme};
use crate::ui::toast::{self, Toasts};
use crate::vm::draft33::{
    FindHook as Draft33Find, OpenHook as Draft33Hook, Variant as Draft33Variant,
};
use crate::vm::executors::{self, AgentUpdate};
use crate::vm::settings_nav::Category;
use crate::vm::shell::{self, Chip, History, Place};

/// How the app was launched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Options {
    /// `--fake`: run `FakeDaemon` in-process.
    pub(crate) fake: bool,
    /// `RENO_APPEARANCE=light|dark`: preview a palette without touching system settings.
    pub(crate) appearance: Option<Appearance>,
    /// `RENO_QUIT_AFTER_MS`: exercise the clean-quit path without a human.
    pub(crate) quit_after: Option<Duration>,
    /// `RENO_REDUCE_MOTION=1`: force reduced motion (the OS setting is not read, see `motion`).
    pub(crate) reduce_motion: bool,
    /// `RENO_OPEN=runs|triggers|executors|settings`: open on that screen (screenshots, reviews).
    pub(crate) open: Option<Nav>,
    /// `RENO_OPEN=run_<id>`: open on that run's screen (screenshots, reviews).
    pub(crate) open_run: Option<RunId>,
    /// `RENO_OPEN=project:<root>`: open on that project's page (screenshots, reviews).
    pub(crate) open_project: Option<PathBuf>,
    /// `RENO_OPEN=newthread:<root>`: open on the new-thread state for that project (screenshots).
    pub(crate) open_new_thread: Option<PathBuf>,
    /// `RENO_SPLIT=1`: draw diffs side by side from the start (screenshots, reviews).
    pub(crate) split: bool,
    /// `RENO_PANEL=home|session|review|history|commit:<sha>|file:<path>`: with `RENO_OPEN=run_<id>`, put that tab of the side
    /// panel in front (screenshots, reviews).
    pub(crate) panel: Option<Surface>,
    /// `RENO_FILES_OPEN=<path>` or `<path>:<line>`: with `RENO_PANEL=files`, open that file in
    /// the Files tab (screenshots, reviews).
    pub(crate) files_open: Option<String>,
    /// `RENO_FILES_SEARCH=<text>`: with `RENO_PANEL=files`, type that in the Files tab's search.
    pub(crate) files_search: Option<String>,
    /// `RENO_TERMINAL_INPUT=<text>`: with `RENO_PANEL=terminal`, type that into the new terminal
    /// (`\e` is Escape, `\n` is Enter; the fake daemon echoes it, so colours show).
    pub(crate) terminal_input: Option<String>,
    /// `RENO_FOLLOW_UP=<text>`: with `RENO_OPEN=run_<id>`, send `text` from that run's box once it
    /// is open, so a thread with two runs can be looked at (screenshots, reviews).
    pub(crate) follow_up: Option<String>,
    /// `RENO_OPEN_FOLDS=1`: with `RENO_OPEN=run_<id>`, open every "Worked for" fold of that run
    /// (screenshots, reviews).
    pub(crate) open_folds: bool,
    /// `RENO_DEMO_MENU=plus|permission|effort|approval-more|approval-note`: with `RENO_OPEN=run_<id>`, open that
    /// composer menu (screenshots, reviews).
    pub(crate) demo_menu: Option<String>,
    /// `RENO_ACTIVITY_OPEN=plan,tasks,stop:dev`: with `--fake` and `RENO_OPEN=run_plan1`, open the
    /// plan and background trays and press Stop on a task (screenshots, reviews).
    pub(crate) activity_open: Option<String>,
    /// `RENO_REGISTRY_DEMO=open:auggie,filter:cant-run,search:go,signin:terminal,add:goose,
    /// remove:acp.cline,offline,own`: open Settings › Add from registry and put the registry
    /// group in a state once it has loaded (screenshots, reviews). Steps on an added agent's line
    /// (`remove:`, `signin:`) act on Settings › Executors, which is not the page shown.
    pub(crate) registry_demo: Option<String>,
    /// `RENO_OPEN=settings:<page>` (`executors`, `registry`, `definitions`, `policy`,
    /// `notifications`, `daemon`): open Settings on that page (screenshots, reviews).
    pub(crate) open_settings: Option<Category>,
    /// `RENO_DATA_DIR`: where the app keeps its own files (the saved panel state). With
    /// `--fake` and no such directory, nothing is saved: a fake run never writes to the real
    /// data dir.
    pub(crate) data_dir: Option<PathBuf>,
    /// `RENO_COMPOSER_TYPE=@src/` (with `--fake`): put that in the box once the screen is up, so
    /// its completion list is open (screenshots, reviews).
    pub(crate) composer_type: Option<String>,
    /// `RENO_COMPOSER_OPEN=project|branch|checkout|queue-edit|clone-ok|clone-fail` (with
    /// `--fake`): open that picker, edit the first queued message, or run a clone (screenshots).
    pub(crate) composer_open: Option<String>,
    /// `RENO_COMPOSER_FAIL=commands` (with `--fake`): the harness's commands cannot be read.
    pub(crate) composer_fail: Option<String>,
    /// `RENO_SIDEBAR_OPEN=menu|copy|rename|archived|more|archive|resize|close` (optionally
    /// `close:atlas01`, a run's suffix; with `--fake`): act on the "fix the ACP v2 handshake"
    /// thread's row once the screen is up (screenshots, reviews).
    pub(crate) sidebar_open: Option<String>,
    /// `RENO_SETUP=skip|welcome|look|agents|project|done` (with `--fake`): skip first-run setup,
    /// or open it at that step even when it was done (screenshots, reviews).
    pub(crate) setup: Option<crate::vm::first_run::Hook>,
    /// `--fake --draft33` (`RENO_DRAFT33=a|b|c`, default `a`): the ROAD-33 transcript draft, with
    /// its variant. It skips first-run setup. Without the flag the app is exactly as it was.
    pub(crate) draft33: Option<Draft33Variant>,
    /// `RENO_DRAFT33_OPEN=thread|folds|render|stream`: with `--draft33`, open that state once the
    /// screen is up (screenshots, reviews). An unknown word is ignored, with a line on the console.
    pub(crate) draft33_open: Option<Draft33Hook>,
    /// The find words of `RENO_DRAFT33_OPEN` (`find|enter|enter3|escape`).
    pub(crate) draft33_find: Option<Draft33Find>,
    /// `RENO_BENCH=1`: print the lines `cargo xtask bench --app` reads (`bench.rs`), and scroll
    /// the open run's transcript. Off, nothing changes.
    pub(crate) bench: bool,
}

impl Options {
    /// Where the panel state lives, or `None` when it is not kept.
    pub(crate) fn panel_state_path(&self) -> Option<PathBuf> {
        match (&self.data_dir, self.fake) {
            (Some(dir), _) => Some(crate::panel::persist::path_in(dir)),
            (None, false) => crate::panel::persist::default_path(),
            (None, true) => None,
        }
    }
}

impl Options {
    pub(crate) fn parse(
        args: impl IntoIterator<Item = String>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let mut options = Self::default();
        let mut draft33 = false;
        for arg in args {
            match arg.as_str() {
                "--fake" => options.fake = true,
                "--draft33" => draft33 = true,
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        if draft33 {
            if !options.fake {
                return Err("--draft33 needs --fake".to_owned());
            }
            options.draft33 = Some(match env("RENO_DRAFT33").filter(|v| !v.is_empty()) {
                None => Draft33Variant::A,
                Some(key) => Draft33Variant::parse(&key)
                    .ok_or_else(|| format!("RENO_DRAFT33: {key} is not a, b or c"))?,
            });
            if let Some(word) = env("RENO_DRAFT33_OPEN").filter(|v| !v.is_empty()) {
                options.draft33_open = Draft33Hook::parse(&word);
                options.draft33_find = Draft33Find::parse(&word);
                if options.draft33_open.is_none() && options.draft33_find.is_none() {
                    crate::vm::draft33::ignore_unknown_hook(&word);
                }
            }
        }
        if options.fake
            && let Some(word) = env("RENO_SETUP").filter(|v| !v.is_empty())
        {
            options.setup = Some(
                crate::vm::first_run::Hook::parse(&word)
                    .ok_or_else(|| format!("RENO_SETUP: unknown value {word}"))?,
            );
        }
        if options.fake
            && let Some(open) = env("RENO_SIDEBAR_OPEN").filter(|v| !v.is_empty())
        {
            let what = open.split(':').next().unwrap_or_default();
            if ![
                "menu", "copy", "rename", "archived", "more", "archive", "resize", "close",
            ]
            .contains(&what)
            {
                return Err(format!("RENO_SIDEBAR_OPEN: unknown hook {open}"));
            }
            options.sidebar_open = Some(open);
        }
        if options.fake {
            options.composer_type = env("RENO_COMPOSER_TYPE").filter(|v| !v.is_empty());
            options.composer_open = env("RENO_COMPOSER_OPEN").filter(|v| !v.is_empty());
            options.composer_fail = env("RENO_COMPOSER_FAIL").filter(|v| !v.is_empty());
        }
        options.appearance = match env("RENO_APPEARANCE").as_deref() {
            Some("light") => Some(Appearance::Light),
            Some("dark") => Some(Appearance::Dark),
            _ => None,
        };
        options.quit_after = env("RENO_QUIT_AFTER_MS")
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis);
        options.reduce_motion = env("RENO_REDUCE_MOTION").is_some_and(|v| v == "1");
        options.bench = env("RENO_BENCH").is_some_and(|v| v == "1");
        options.open = match env("RENO_OPEN").as_deref() {
            None | Some("" | "home") => None,
            Some("runs") => Some(Nav::Runs),
            Some("triggers") => Some(Nav::Triggers),
            Some("executors") => Some(Nav::Executors),
            Some("settings") => Some(Nav::Settings),
            Some(other) if other.starts_with("settings:") => {
                let key = &other["settings:".len()..];
                options.open_settings = Some(
                    Category::ALL
                        .into_iter()
                        .find(|c| c.key() == key)
                        .ok_or_else(|| format!("RENO_OPEN: no settings page {key}"))?,
                );
                Some(Nav::Settings)
            }
            Some(other) if other.starts_with("project:") => {
                options.open_project = Some(PathBuf::from(&other["project:".len()..]));
                None
            }
            Some(other) if other.starts_with("newthread:") => {
                options.open_new_thread = Some(PathBuf::from(&other["newthread:".len()..]));
                None
            }
            Some(other) if other.starts_with("run_") => {
                options.open_run = Some(
                    other
                        .parse()
                        .map_err(|e| format!("RENO_OPEN: not a run id: {e}"))?,
                );
                None
            }
            Some(other) => return Err(format!("unknown RENO_OPEN screen: {other}")),
        };
        options.panel = match env("RENO_PANEL").as_deref() {
            Some("home") => Some(Surface::Home),
            Some("session") => Some(Surface::Session),
            Some("review") => Some(Surface::Review),
            Some("files") => Some(Surface::Files),
            Some("history") => Some(Surface::History),
            Some(other) if other.starts_with("commit:") && other.len() > "commit:".len() => {
                Some(Surface::Commit(other["commit:".len()..].to_owned()))
            }
            Some("terminal") => Some(Surface::Terminal(0)),
            // A subagent's tab (`agent:researcher`), with the fake's run activity.
            Some(other) if other.starts_with("agent:") && other.len() > "agent:".len() => {
                RunId::from_suffix(&reno_client::activity::agent_run_suffix(
                    &other["agent:".len()..],
                ))
                .ok()
                .map(Surface::SideChat)
            }
            Some(other) if other.starts_with("file:") && other.len() > "file:".len() => {
                Some(Surface::File(other["file:".len()..].to_owned()))
            }
            None | Some("") => None,
            Some(other) => return Err(format!("unknown RENO_PANEL tab: {other}")),
        };
        options.split = env("RENO_SPLIT").is_some_and(|v| v == "1");
        options.files_open = env("RENO_FILES_OPEN").filter(|v| !v.is_empty());
        options.files_search = env("RENO_FILES_SEARCH").filter(|v| !v.is_empty());
        options.terminal_input = env("RENO_TERMINAL_INPUT").filter(|v| !v.is_empty());
        options.follow_up = env("RENO_FOLLOW_UP").filter(|v| !v.is_empty());
        options.open_folds = env("RENO_OPEN_FOLDS").is_some_and(|v| v == "1");
        options.demo_menu = env("RENO_DEMO_MENU").filter(|v| !v.is_empty());
        options.activity_open = env("RENO_ACTIVITY_OPEN").filter(|v| !v.is_empty());
        options.registry_demo = env("RENO_REGISTRY_DEMO").filter(|v| !v.is_empty());
        options.data_dir = env("RENO_DATA_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        Ok(options)
    }
}

/// The Geist regular face. Registered with GPUI below and also read by the diagram renderer, which
/// measures and rasterises labels in the font the app draws everything else in.
pub(crate) const GEIST_REGULAR: &[u8] = include_bytes!("../assets/fonts/Geist.ttf");

/// Registers Geist and Geist Mono (SIL OFL, see `assets/fonts/Geist-OFL.txt`).
pub(crate) fn register_fonts(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(GEIST_REGULAR),
        Cow::Borrowed(include_bytes!("../assets/fonts/Geist-Medium.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/Geist-SemiBold.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/GeistMono.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/GeistMono-Medium.ttf").as_slice()),
    ];
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        // Text falls back to the system font; say so once on the console.
        #[allow(clippy::print_stderr)]
        {
            eprintln!("reno-app: could not register the bundled fonts: {error}");
        }
    }
}

/// Where the first window opens (screenshots, reviews); a window opened later opens on Home.
#[derive(Clone, Debug, Default)]
pub(crate) struct Start {
    pub(crate) open: Option<Nav>,
    pub(crate) open_run: Option<RunId>,
    pub(crate) open_project: Option<PathBuf>,
    pub(crate) open_new_thread: Option<PathBuf>,
    /// A panel tab to put in front once `open_run` is open.
    pub(crate) panel: Option<Surface>,
    /// A file to open in the Files tab, as `path` or `path:line`, and a search to type there.
    pub(crate) files_open: Option<String>,
    pub(crate) files_search: Option<String>,
    /// `RENO_TERMINAL_INPUT=<text>`: with `RENO_PANEL=terminal`, type that into the new terminal
    /// (`\e` is Escape, `\n` is Enter; the fake daemon echoes it, so colours show).
    pub(crate) terminal_input: Option<String>,
    /// A message to send from that run's box once it is open.
    pub(crate) follow_up: Option<String>,
    /// Open every fold of that run's transcript.
    pub(crate) open_folds: bool,
    /// `RENO_DEMO_MENU=plus|permission|effort|approval-more`: with `RENO_OPEN=run_<id>`, open that
    /// composer menu (screenshots, reviews).
    pub(crate) demo_menu: Option<String>,
    /// `RENO_ACTIVITY_OPEN`: trays to open and a task to stop once the run is open.
    pub(crate) activity_open: Option<String>,
    /// `RENO_REGISTRY_DEMO=open:auggie,filter:cant-run,search:go,signin:terminal,add:goose,
    /// remove:acp.cline,offline,own`: with `RENO_OPEN=executors`, put the registry group in a
    /// state once it has loaded (screenshots, reviews).
    pub(crate) registry_demo: Option<String>,
    /// The Settings page to open on.
    pub(crate) open_settings: Option<Category>,
    /// `RENO_COMPOSER_TYPE` and `RENO_COMPOSER_OPEN`: text to type and a picker to open.
    pub(crate) composer_type: Option<String>,
    pub(crate) composer_open: Option<String>,
    /// The sidebar's start hook.
    pub(crate) sidebar_open: Option<String>,
    /// `RENO_SETUP`.
    pub(crate) setup: Option<crate::vm::first_run::Hook>,
    /// `RENO_DRAFT33_OPEN`.
    pub(crate) draft33_open: Option<Draft33Hook>,
    /// The find words of `RENO_DRAFT33_OPEN`.
    pub(crate) draft33_find: Option<Draft33Find>,
}

pub(crate) fn run(options: Options) {
    let app = application().with_assets(Assets);
    // A Dock click with no window open: build a new one around the running store.
    app.on_reopen(lifecycle::reopen);
    app.run(move |cx: &mut App| {
        gpui_tokio::init(cx);
        register_fonts(cx);
        let preference = if options.reduce_motion {
            ReduceMotion::On
        } else {
            ReduceMotion::System
        };
        motion::init(preference, false, cx);
        actions::bind_keys(cx);
        composer::bind_keys(cx);
        crate::screens::run::bind_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        crate::ui::restart::bind_keys(cx);
        if let Some(variant) = options.draft33 {
            crate::ui::draft33::install(variant, cx);
        }
        if options.fake {
            // A demo against the fake daemon never opens a browser.
            crate::links::mute();
        }

        if let Some(path) = options.panel_state_path() {
            crate::panel::saved::Saved::install(path, cx);
        }
        if options.split {
            let prefs = crate::panel::view::Prefs::get(cx);
            crate::panel::view::Prefs {
                split: true,
                ..prefs
            }
            .set_without_saving(cx);
        }
        let toasts = toast::install(cx);
        let store = cx.new(|cx| {
            let mut store = Store::connecting();
            if options.fake {
                store.start_fake(cx);
                if options.composer_fail.as_deref() == Some("commands") {
                    store.set_commands_unreadable(true);
                }
            } else {
                store.start_socket(cx);
            }
            store
        });
        // The store, the toasts and the notifier outlive the window: closing it does not quit.
        lifecycle::install(cx, store.clone(), toasts.clone(), &options);

        let start = Start {
            open: options.open,
            open_run: options.open_run.clone(),
            open_project: options.open_project.clone(),
            open_new_thread: options.open_new_thread.clone(),
            panel: options.panel.clone(),
            files_open: options.files_open.clone(),
            files_search: options.files_search.clone(),
            terminal_input: options.terminal_input.clone(),
            follow_up: options.follow_up.clone(),
            open_folds: options.open_folds,
            demo_menu: options.demo_menu.clone(),
            activity_open: options.activity_open.clone(),
            registry_demo: options.registry_demo.clone(),
            open_settings: options.open_settings,
            composer_type: options.composer_type.clone(),
            composer_open: options.composer_open.clone(),
            sidebar_open: options.sidebar_open.clone(),
            // The draft shows on a fresh data dir, where setup would cover it.
            setup: options
                .setup
                .or(options.draft33.map(|_| crate::vm::first_run::Hook::Skip)),
            draft33_open: options.draft33_open,
            draft33_find: options.draft33_find,
        };
        if open_window(store, toasts, options.appearance, start, cx).is_none() {
            cx.quit();
            return;
        }

        // Closing the last window does not quit (lifecycle.rs); the panel state is written now
        // all the same, as it would be on quit.
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                crate::panel::saved::Saved::flush(cx);
            }
        })
        .detach();

        if let Some(after) = options.quit_after {
            cx.spawn(async move |cx| {
                cx.background_executor().timer(after).await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
        cx.activate(true);
    });
}

/// Opens a Reno window on `store`. `None` when the platform would not.
pub(crate) fn open_window(
    store: Entity<Store>,
    toasts: Entity<Toasts>,
    appearance: Option<Appearance>,
    start: Start,
    cx: &mut App,
) -> Option<gpui::WindowHandle<RootView>> {
    let bounds = default_bounds(cx);
    let Start {
        open,
        open_run,
        open_project,
        open_new_thread,
        panel,
        files_open,
        files_search,
        terminal_input,
        follow_up,
        open_folds,
        demo_menu,
        activity_open,
        registry_demo,
        open_settings,
        composer_type,
        composer_open,
        sidebar_open,
        setup,
        draft33_open,
        draft33_find,
    } = start;
    let named = open.is_some()
        || open_run.is_some()
        || open_project.is_some()
        || open_new_thread.is_some()
        || open_settings.is_some()
        || sidebar_open.is_some()
        || composer_open.is_some();
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(900.0), px(600.0))),
            titlebar: Some(TitlebarOptions {
                title: Some("Reno".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(14.0), px(14.0))),
            }),
            window_background: WindowBackgroundAppearance::Opaque,
            ..Default::default()
        },
        |window, cx| {
            crate::bench::first_frame(window);
            crate::bench::bring_to_front(window, cx);
            let root = cx.new(|cx| RootView::new(store, toasts, appearance, window, cx));
            root.update(cx, |root, cx| {
                root.focus_composer(window, cx);
                if composer_type.is_some() || composer_open.is_some() {
                    // After the runs are read and any opened run is up.
                    cx.spawn_in(window, async move |root, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(3000))
                            .await;
                        root.update_in(cx, |root, window, cx| {
                            root.composer_hooks(composer_type, composer_open, window, cx);
                        })
                        .ok();
                    })
                    .detach();
                }
                if let Some(hook) = sidebar_open {
                    // After the runs and their threads are read and the rows are painted.
                    cx.spawn_in(window, async move |root, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(3000))
                            .await;
                        root.update_in(cx, |root, window, cx| {
                            root.sidebar_hook(&hook, window, cx);
                        })
                        .ok();
                    })
                    .detach();
                }
                root.init_first_run(setup, named, window, cx);
                if let Some(hook) = draft33_find {
                    // After the runs and their threads are read.
                    cx.spawn_in(window, async move |root, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(1500))
                            .await;
                        root.update_in(cx, |root, window, cx| {
                            root.draft33_find_hook(hook, window, cx);
                        })
                        .ok();
                    })
                    .detach();
                }
                if let Some(hook) = draft33_open {
                    // After the runs and their threads are read.
                    cx.spawn_in(window, async move |root, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(1500))
                            .await;
                        root.update_in(cx, |root, window, cx| {
                            root.draft33_hook(hook, window, cx);
                        })
                        .ok();
                        if hook == Draft33Hook::Latest {
                            // The reply streams: take the view off its tail once it can scroll.
                            for _ in 0..12 {
                                cx.background_executor()
                                    .timer(Duration::from_millis(1000))
                                    .await;
                                let done = root
                                    .update(cx, |root, cx| {
                                        root.run_screen().cloned().is_some_and(|screen| {
                                            screen.update(cx, |s, cx| s.demo_scroll_up(cx))
                                        })
                                    })
                                    .unwrap_or(true);
                                if done {
                                    break;
                                }
                            }
                        }
                        if matches!(hook, Draft33Hook::Select | Draft33Hook::CopyCode) {
                            // The reply has to be drawn (and its diagram and picture landed)
                            // before there is anything to select.
                            cx.background_executor()
                                .timer(Duration::from_millis(5000))
                                .await;
                            root.update_in(cx, |root, window, cx| {
                                if let Some(screen) = root.run_screen().cloned() {
                                    screen.update(cx, |screen, cx| {
                                        screen.demo_copy(hook == Draft33Hook::Select, window, cx);
                                    });
                                }
                            })
                            .ok();
                        }
                        if hook == Draft33Hook::Folds {
                            // A long thread's runs arrive one after another: open what is there,
                            // twice (an open fold stays open).
                            for _ in 0..2 {
                                cx.background_executor()
                                    .timer(Duration::from_millis(1500))
                                    .await;
                                root.update(cx, |root, cx| {
                                    if let Some(screen) = root.run_screen().cloned() {
                                        screen.update(cx, |screen, cx| screen.open_all_folds(cx));
                                    }
                                })
                                .ok();
                            }
                        }
                    })
                    .detach();
                }
                if let Some(nav) = open {
                    root.navigate(nav, window, cx);
                }
                if let Some(category) = open_settings {
                    root.open_settings(category, window, cx);
                }
                if let Some(script) = registry_demo {
                    // After the registry has been read.
                    cx.spawn_in(window, async move |root, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(1500))
                            .await;
                        root.update_in(cx, |root, window, cx| {
                            root.open_settings(Category::Registry, window, cx);
                            root.executors
                                .update(cx, |screen, cx| screen.registry_demo(&script, cx));
                        })
                        .ok();
                    })
                    .detach();
                }
                if let Some(dir) = open_new_thread {
                    cx.spawn_in(window, async move |view, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(1500))
                            .await;
                        view.update_in(cx, |view, window, cx| {
                            view.new_thread(
                                Some(crate::vm::projects::FolderKey::Repo(dir)),
                                window,
                                cx,
                            );
                        })
                        .ok();
                    })
                    .detach();
                }
                if let Some(dir) = open_project {
                    cx.spawn_in(window, async move |view, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(1500))
                            .await;
                        view.update_in(cx, |view, window, cx| {
                            view.open_project(
                                crate::vm::projects::FolderKey::Repo(dir),
                                window,
                                cx,
                            );
                        })
                        .ok();
                    })
                    .detach();
                }
                if let Some(run) = open_run {
                    // After the store has connected, so the run's transcript is followed.
                    cx.spawn_in(window, async move |root, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(1500))
                            .await;
                        root.update_in(cx, |root, window, cx| {
                            root.open_run(run, window, cx);
                            match panel {
                                Some(Surface::Terminal(_)) => {
                                    if let Some(screen) = root.run_screen().cloned() {
                                        let input = terminal_input.clone();
                                        screen.update(cx, |screen, cx| {
                                            screen.open_demo_terminal(input, window, cx);
                                        });
                                    }
                                }
                                Some(tab) => root.show_panel_tab(tab, window, cx),
                                None => {}
                            }
                        })
                        .ok();
                        if files_open.is_some() || files_search.is_some() {
                            // The Files tab reads its root first.
                            cx.background_executor()
                                .timer(Duration::from_millis(800))
                                .await;
                            root.update_in(cx, |root, window, cx| {
                                if let Some(screen) = root.run_screen().cloned() {
                                    screen.update(cx, |screen, cx| {
                                        if let Some(target) = &files_open {
                                            let (path, line) = match target.rsplit_once(':') {
                                                Some((p, l)) if l.parse::<u32>().is_ok() => {
                                                    (p, l.parse().ok())
                                                }
                                                _ => (target.as_str(), None),
                                            };
                                            screen.open_file_at(path, line, window, cx);
                                        }
                                        if let Some(text) = &files_search {
                                            screen.type_files_search(text, window, cx);
                                        }
                                    });
                                }
                            })
                            .ok();
                        }
                        if open_folds {
                            cx.background_executor()
                                .timer(Duration::from_millis(500))
                                .await;
                            root.update(cx, |root, cx| {
                                if let Some(screen) = root.run_screen().cloned() {
                                    screen.update(cx, |screen, cx| screen.open_all_folds(cx));
                                }
                            })
                            .ok();
                        }
                        if crate::bench::scroll_script() {
                            root.update_in(cx, |root, window, cx| {
                                if let Some(screen) = root.run_screen().cloned() {
                                    screen.update(cx, |screen, cx| screen.bench_scroll(window, cx));
                                }
                            })
                            .ok();
                        }
                        if let Some(hook) = activity_open {
                            cx.background_executor()
                                .timer(Duration::from_millis(1200))
                                .await;
                            root.update_in(cx, |root, _, cx| {
                                if let Some(screen) = root.run_screen().cloned() {
                                    screen.update(cx, |s, cx| s.open_activity_demo(&hook, cx));
                                }
                            })
                            .ok();
                        }
                        if let Some(menu) = demo_menu {
                            cx.background_executor()
                                .timer(Duration::from_millis(1200))
                                .await;
                            root.update_in(cx, |root, window, cx| {
                                if let Some(screen) = root.run_screen().cloned() {
                                    screen.update(cx, |s, cx| s.open_demo_menu(&menu, window, cx));
                                }
                            })
                            .ok();
                        }
                        if let Some(text) = follow_up {
                            // Once the threads are read, so the message continues the thread.
                            cx.background_executor()
                                .timer(Duration::from_millis(1500))
                                .await;
                            root.update_in(cx, |root, window, cx| {
                                if let Some(screen) = root.run_screen().cloned() {
                                    let steer = screen.read(cx).steer_input().clone();
                                    steer.update(cx, |c, cx| {
                                        c.fill(&text, window, cx);
                                        c.submit(window, cx);
                                    });
                                }
                            })
                            .ok();
                        }
                    })
                    .detach();
                }
            });
            root
        },
    )
    .ok()
}

/// The window's opening size: most of the visible screen, within a range that keeps the
/// sidebar, a 760 px transcript and the details pane readable, centred on the primary display.
pub(crate) fn window_size(visible: gpui::Size<gpui::Pixels>) -> gpui::Size<gpui::Pixels> {
    let width = (visible.width * 0.82).clamp(px(1100.0), px(1480.0));
    let height = (visible.height * 0.86).clamp(px(700.0), px(960.0));
    size(width.min(visible.width), height.min(visible.height))
}

fn default_bounds(cx: &App) -> Bounds<gpui::Pixels> {
    match cx.primary_display() {
        Some(display) => {
            let visible = display.visible_bounds();
            Bounds::centered_at(visible.center(), window_size(visible.size))
        }
        None => Bounds::new(point(px(0.0), px(0.0)), size(px(1280.0), px(820.0))),
    }
}

/// What the canvas panel shows.
#[derive(Clone)]
enum Screen {
    Home,
    Runs,
    Run(Entity<RunScreen>),
    Project(Entity<crate::screens::project::ProjectScreen>),
    Triggers,
    Settings,
}

pub(crate) struct RootView {
    store: Entity<Store>,
    home: Entity<HomeScreen>,
    runs: Entity<RunsScreen>,
    triggers: Entity<TriggersScreen>,
    executors: Entity<ExecutorsScreen>,
    settings: Entity<SettingsScreen>,
    toasts: Entity<Toasts>,
    screen: Screen,
    /// What Settings opened over: "Back to app" returns to it.
    before_settings: Option<Screen>,
    sidebar_hidden: bool,
    /// Where the person has been, for the bar's back and forward.
    history: History,
    /// The project a new thread is for ("+" on a project), or none.
    home_folder: Option<crate::vm::projects::FolderKey>,
    /// The bar's "…" menu and the agents-updates list are open.
    more_open: bool,
    updates_open: bool,
    /// The updates list's rows as last shown while it is open, so an update that finished can
    /// say "Up to date" before the row goes.
    updates_shown: Vec<AgentUpdate>,
    more_at: Anchor_,
    updates_at: Anchor_,
    /// Project folders opened or closed against their default.
    folders_toggled: std::collections::HashSet<crate::vm::projects::FolderKey>,
    /// From `RENO_APPEARANCE` or "Toggle theme"; `None` follows the system.
    appearance_override: Option<Appearance>,
    /// The saved mode (Settings › Appearance), read each frame.
    mode: crate::prefs::Mode,
    palette: Option<Entity<Palette>>,
    /// Hears the open run screen ("Open Executors").
    _run_events: Vec<Subscription>,
    /// The run a trigger just started; the Triggers screen keeps showing, the toast says so.
    pending_fired: Option<RunId>,
    /// The sidebar's interaction state: menu, rename, confirm, "Show more", width.
    sidebar: sidebar::State,
    /// First-run setup while it shows, the connecting gate, and the restart of `renod`.
    setup: Option<Entity<crate::ui::setup::flow::SetupFlow>>,
    gate: Option<crate::ui::gate::Phase>,
    restart: crate::vm::daemon_restart::Restart,
    /// The panel under the restart affordance is open.
    restart_open: bool,
    restart_at: Anchor_,
    _first_run_subscriptions: Vec<Subscription>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl RootView {
    pub(crate) fn new(
        store: Entity<Store>,
        toasts: Entity<Toasts>,
        appearance_override: Option<Appearance>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let home = cx.new(|cx| HomeScreen::new(store.clone(), cx));
        let runs = cx.new(|cx| RunsScreen::new(store.clone(), cx));
        let triggers = cx.new(|cx| TriggersScreen::new(store.clone(), cx));
        let executors = cx.new(|cx| ExecutorsScreen::new(store.clone(), cx));
        let settings = cx.new(|cx| SettingsScreen::new(store.clone(), executors.clone(), cx));
        let subscriptions = vec![
            cx.subscribe_in(
                &home.read(cx).composer().clone(),
                window,
                Self::on_open_executors,
            ),
            cx.subscribe_in(&home, window, Self::on_screen_event),
            cx.subscribe_in(&runs, window, Self::on_screen_event),
            cx.subscribe_in(&triggers, window, Self::on_screen_event),
            cx.subscribe_in(&executors, window, Self::on_screen_event),
            cx.observe(&store, |this, _, cx| {
                this.remember_updates(cx);
                this.lift_gate(cx);
                if !crate::bench::off("store") {
                    cx.notify();
                }
            }),
            cx.observe(&toasts, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
            // Back from a sign-in in Terminal: look at the executor again.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.executors
                        .update(cx, |screen, cx| screen.on_window_activated(cx));
                }
            }),
            cx.subscribe_in(&store, window, |this, _, event: &StoreEvent, window, cx| {
                match event {
                    StoreEvent::Failed(message) => {
                        toast::push(
                            cx,
                            toast::Tone::Error,
                            "Something went wrong",
                            message.clone(),
                        );
                    }
                    StoreEvent::RunStarted(run) => {
                        this.open_run(run.clone(), window, cx);
                    }
                    // The composer that sent it takes its draft back.
                    StoreEvent::SendFailed(_)
                    | StoreEvent::SendRefused { .. }
                    | StoreEvent::ThreadClosed => {}
                    StoreEvent::ExecutorInstalled(id) => {
                        toast::push(
                            cx,
                            toast::Tone::Info,
                            "Executor installed",
                            format!(
                                "{} is installed. Check its sign-in in Settings › Executors.",
                                crate::summary::executor_name(id)
                            ),
                        );
                    }
                    StoreEvent::ExecutorJobDone {
                        name,
                        kind,
                        version,
                        sign_in,
                        registry,
                    } => {
                        let (label, message) = match kind {
                            // An agent from the ACP registry is "added", and says whether it
                            // can take runs now.
                            reno_proto::JobKind::Install if *registry => (
                                "Agent added",
                                crate::vm::registry::added_words(name, *sign_in),
                            ),
                            reno_proto::JobKind::Install => (
                                "Executor installed",
                                if *sign_in {
                                    format!("{name} installed. One step needs you: sign in.")
                                } else {
                                    format!("{name} installed.")
                                },
                            ),
                            reno_proto::JobKind::Update => (
                                "Executor updated",
                                match version {
                                    Some(v) => format!("{name} updated to {v}."),
                                    None => format!("{name} updated."),
                                },
                            ),
                        };
                        toast::push(cx, toast::Tone::Info, label, message);
                    }
                    StoreEvent::ExecutorJobStopped(name) => {
                        toast::push(
                            cx,
                            toast::Tone::Info,
                            "Stopped",
                            format!("{name}: the job was stopped."),
                        );
                    }
                    StoreEvent::StillSignedOut(name) => {
                        toast::push(
                            cx,
                            toast::Tone::NotYet,
                            "Still signed out",
                            format!("{name} is still signed out."),
                        );
                    }
                    StoreEvent::TriggerFired { name, outcome, run } => {
                        let what = match outcome {
                            FiringOutcome::Fired => format!("{name} started a run."),
                            FiringOutcome::Coalesced => {
                                format!("{name} is still running; this firing joined it.")
                            }
                            FiringOutcome::Duplicate => {
                                format!("{name} had already fired for this occurrence.")
                            }
                            FiringOutcome::BackedOff => {
                                format!("{name} could not start a run and is backing off.")
                            }
                        };
                        toast::push(cx, toast::Tone::Info, "Trigger fired", what);
                        if let (FiringOutcome::Fired, Some(run)) = (outcome, run) {
                            this.pending_fired = Some(run.clone());
                        }
                    }
                }
            }),
        ];
        Self {
            store,
            home,
            runs,
            triggers,
            executors,
            settings,
            pending_fired: None,
            sidebar: sidebar::State::new(crate::prefs::current(cx).sidebar_width),
            setup: None,
            gate: None,
            restart: crate::vm::daemon_restart::Restart::default(),
            restart_open: false,
            restart_at: Rc::new(Cell::new(None)),
            _first_run_subscriptions: Vec::new(),
            toasts,
            screen: Screen::Home,
            before_settings: None,
            sidebar_hidden: false,
            history: History::new(Place::NewThread(None)),
            home_folder: None,
            more_open: false,
            updates_open: false,
            updates_shown: Vec::new(),
            more_at: Rc::new(Cell::new(None)),
            updates_at: Rc::new(Cell::new(None)),
            folders_toggled: std::collections::HashSet::new(),
            appearance_override,
            mode: crate::prefs::Mode::System,
            palette: None,
            _run_events: Vec::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    // --- First run: the connecting gate and setup (ROAD-38) ----------------------------------

    /// What a window opens with: the connecting gate when the daemon has not answered yet (it is
    /// drawn on the first frame and never waits for the daemon), and first-run setup on a first
    /// run. `hook` (`RENO_SETUP`, with `--fake`) skips setup or opens it at a step; `named` says a
    /// screen was asked for by name, which is shown instead of setup.
    pub(crate) fn init_first_run(
        &mut self,
        hook: Option<crate::vm::first_run::Hook>,
        named: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::vm::first_run::{Hook, should_show};
        if self.store.read(cx).connection() == reno_client::ConnectionStatus::Connecting {
            self.gate = Some(crate::ui::gate::Phase::Up);
        }
        let forced = matches!(hook, Some(Hook::At(_)));
        let onboarded = crate::prefs::current(cx).onboarded;
        let skipped = hook == Some(Hook::Skip) || (named && !forced);
        if !should_show(onboarded, forced) || skipped {
            return;
        }
        let store = self.store.clone();
        let flow = cx.new(|cx| crate::ui::setup::flow::SetupFlow::new(store, cx));
        self._first_run_subscriptions
            .push(cx.subscribe_in(&flow, window, Self::on_setup_event));
        if let Some(Hook::At(step)) = hook {
            flow.update(cx, |flow, cx| flow.go(step, cx));
        }
        self.setup = Some(flow);
    }

    /// The daemon has answered, or failed to: the gate fades, then goes.
    fn lift_gate(&mut self, cx: &mut Context<Self>) {
        use crate::ui::gate::Phase;
        if self.gate != Some(Phase::Up)
            || !crate::vm::first_run::gate_lifts(self.store.read(cx).connection())
        {
            return;
        }
        self.gate = Some(Phase::Fading);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(crate::vm::first_run::GATE_FADE_MS))
                .await;
            this.update(cx, |this, cx| {
                this.gate = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The gate's phase, for tests.
    #[cfg(test)]
    pub(crate) fn gate_phase(&self) -> Option<crate::ui::gate::Phase> {
        self.gate
    }

    /// Setup is over: it never shows again.
    fn end_setup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.setup = None;
        crate::prefs::update(cx, |p| p.onboarded = true);
        self.focus_composer(window, cx);
        cx.notify();
    }

    fn on_setup_event(
        &mut self,
        _: &Entity<crate::ui::setup::flow::SetupFlow>,
        event: &crate::ui::setup::flow::FlowEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::ui::setup::flow::FlowEvent;
        match event {
            FlowEvent::Over { .. } => self.end_setup(window, cx),
            FlowEvent::Project(path) => {
                self.set_home_folder(Some(crate::vm::projects::FolderKey::Repo(path.clone())), cx);
            }
            FlowEvent::Review(id) => {
                // The install of this agent is reviewed where it is reviewed.
                self.end_setup(window, cx);
                self.navigate(Nav::Executors, window, cx);
                self.executors
                    .update(cx, |screen, cx| screen.review_install(id, cx));
            }
        }
    }

    /// Setup is showing, for tests.
    #[cfg(test)]
    pub(crate) fn setup(&self) -> Option<&Entity<crate::ui::setup::flow::SetupFlow>> {
        self.setup.as_ref()
    }

    // --- Skip this version (ROAD-38) ----------------------------------------------------------

    /// "Skip this version" on one agent: its update to this version is not offered again, and a
    /// newer one is. Remembered in the app-local `skippedUpdates`.
    pub(crate) fn skip_update(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(update) = self
            .agent_updates(cx)
            .into_iter()
            .find(|u| u.id == id && !u.updating)
        else {
            return;
        };
        crate::prefs::update(cx, |p| {
            p.skipped_updates
                .insert(update.id.clone(), update.to.clone());
        });
        self.updates_shown.retain(|u| u.id != id);
        if self.agent_updates(cx).is_empty() {
            self.updates_open = false;
            self.updates_shown.clear();
        }
        cx.notify();
    }

    // --- Thread links (ROAD-38) ---------------------------------------------------------------

    /// Opens a `reno://thread/<id>` link: focuses that thread's latest run. The URL scheme is not
    /// registered with the OS (that needs the app bundle), so nothing calls this yet but a test;
    /// it is the one entry a link will reach. `false` (and a toast, for a link that was meant) when it did
    /// not open anything.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn open_link(
        &mut self,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(thread) = crate::vm::thread_link::parse(url) else {
            toast::push(
                cx,
                toast::Tone::Info,
                "Not a Reno link",
                "Reno opens links to threads.",
            );
            return false;
        };
        let latest = self
            .store
            .read(cx)
            .state()
            .thread(&thread)
            .and_then(|info| info.latest_run())
            .map(|run| run.run_id.clone());
        match latest {
            Some(run) => {
                self.open_run(run, window, cx);
                true
            }
            None => {
                toast::push(
                    cx,
                    toast::Tone::Info,
                    "Thread not found",
                    "The daemon does not know this thread.",
                );
                false
            }
        }
    }

    // --- Restarting renod (ROAD-38) -----------------------------------------------------------

    /// The pill was pressed: the question, or the failure again.
    pub(crate) fn restart_press(&mut self, cx: &mut Context<Self>) {
        use crate::vm::daemon_restart::Phase;
        match self.restart.phase {
            Phase::Idle => self.restart_ask(cx),
            Phase::Asking | Phase::Failed(_) => self.restart_open = !self.restart_open,
            Phase::Restarting => {}
        }
        cx.notify();
    }

    /// "Restart renod?" is put up.
    pub(crate) fn restart_ask(&mut self, cx: &mut Context<Self>) {
        self.restart.ask();
        self.restart_open = true;
        cx.notify();
    }

    /// "Not now", Esc, a click elsewhere, or "Close" on the failure.
    pub(crate) fn restart_cancel(&mut self, cx: &mut Context<Self>) {
        self.restart_open = false;
        if self.restart.phase == crate::vm::daemon_restart::Phase::Asking {
            self.restart.cancel();
        }
        cx.notify();
    }

    /// "Try again" on the failure.
    pub(crate) fn restart_go_again(&mut self, cx: &mut Context<Self>) {
        self.restart.ask();
        self.restart_go(cx);
    }

    /// "Restart": renod restarts. Takes the time the daemon says, then reads its answer.
    pub(crate) fn restart_go(&mut self, cx: &mut Context<Self>) {
        if !self.restart.begin() {
            return;
        }
        self.restart_open = false;
        let wait = self.store.read(cx).daemon_restart_ms().unwrap_or(2_000);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(wait))
                .await;
            this.update(cx, |this, cx| {
                let result = this.store.update(cx, |store, cx| store.daemon_restart(cx));
                let failed = result.is_err();
                this.restart.finish(result);
                // A failure stays on show until it is closed.
                this.restart_open = failed;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// What the confirm says of the runs that are live now, for tests.
    #[cfg(test)]
    pub(crate) fn restart_live_words(&self, cx: &App) -> String {
        crate::vm::daemon_restart::live_words(shell::live_runs(self.store.read(cx).state()).len())
    }

    /// The restart's state, for tests.
    #[cfg(test)]
    pub(crate) fn restart_phase(&self) -> &crate::vm::daemon_restart::Phase {
        &self.restart.phase
    }

    /// The picker's Account row: look at the harness's login on the Executors screen.
    fn on_open_executors(
        &mut self,
        _: &Entity<composer::Composer>,
        _: &composer::OpenExecutors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate(Nav::Executors, window, cx);
    }

    fn on_screen_event(
        &mut self,
        _: &Entity<impl EventEmitter<ScreenEvent>>,
        event: &ScreenEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ScreenEvent::OpenRun(run) => self.open_run(run.clone(), window, cx),
            ScreenEvent::OpenExecutors => self.navigate(Nav::Executors, window, cx),
            ScreenEvent::OpenProject(key) => self.open_project(key.clone(), window, cx),
        }
    }

    /// The sidebar destination the screen belongs to.
    pub(crate) fn nav(&self) -> Nav {
        match self.screen {
            Screen::Home => Nav::Home,
            Screen::Runs | Screen::Run(_) | Screen::Project(_) => Nav::Runs,
            Screen::Triggers => Nav::Triggers,
            Screen::Settings => Nav::Settings,
        }
    }

    pub(crate) fn run_screen(&self) -> Option<&Entity<RunScreen>> {
        match &self.screen {
            Screen::Run(run) => Some(run),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn runs(&self) -> &Entity<RunsScreen> {
        &self.runs
    }

    #[cfg(test)]
    pub(crate) fn triggers(&self) -> &Entity<TriggersScreen> {
        &self.triggers
    }

    #[cfg(test)]
    pub(crate) fn executors(&self) -> &Entity<ExecutorsScreen> {
        &self.executors
    }

    #[cfg(test)]
    pub(crate) fn settings(&self) -> &Entity<SettingsScreen> {
        &self.settings
    }

    #[cfg(test)]
    pub(crate) fn appearance_override_for_test(&self) -> Option<Appearance> {
        self.appearance_override
    }

    #[cfg(test)]
    pub(crate) fn home_folder(&self) -> Option<&crate::vm::projects::FolderKey> {
        self.home_folder.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn sidebar_hidden(&self) -> bool {
        self.sidebar_hidden
    }

    #[cfg(test)]
    pub(crate) fn home(&self) -> &Entity<HomeScreen> {
        &self.home
    }

    #[cfg(test)]
    pub(crate) fn palette_open(&self) -> bool {
        self.palette.is_some()
    }

    /// The palette, while it is open.
    #[cfg(test)]
    pub(crate) fn palette(&self) -> Option<&Entity<Palette>> {
        self.palette.as_ref()
    }

    /// The appearance in force. Settings › Appearance's mode is the one source: Light or Dark
    /// wins; System is the launch preview (`RENO_APPEARANCE`) when there is one, else macOS.
    pub(crate) fn appearance(&self, window: &Window) -> Appearance {
        match self.mode {
            crate::prefs::Mode::Light => Appearance::Light,
            crate::prefs::Mode::Dark => Appearance::Dark,
            crate::prefs::Mode::System => self
                .appearance_override
                .unwrap_or_else(|| Appearance::from(window.appearance())),
        }
    }

    /// Focuses the screen's text input: Home's composer or the run's steer input. A screen
    /// without one keeps focus on the root, so the keyboard shortcuts still work.
    pub(crate) fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = match &self.screen {
            Screen::Home => self.home.read(cx).composer().focus_handle(cx),
            Screen::Run(run) => run.read(cx).focus_handle(cx),
            _ => self.focus_handle.clone(),
        };
        window.focus(&handle, cx);
    }

    /// One click or key of the sidebar's thread controls (see `ui::sidebar`).
    pub(crate) fn sidebar_act(
        &mut self,
        act: sidebar::Act,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let refocus = match act {
            sidebar::Act::ResizeEnd if !self.sidebar.resizing => false,
            act => self.sidebar.apply(act, &store, window, cx),
        };
        if refocus {
            self.focus_composer(window, cx);
        }
        cx.notify();
    }

    /// The sidebar's interaction state, for tests.
    #[cfg(test)]
    pub(crate) fn sidebar_state(&self) -> &sidebar::State {
        &self.sidebar
    }

    /// ⌘⇧P and ⌘⇧A.
    fn sidebar_key(&mut self, archive: bool, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        self.sidebar.key_act(archive, &store, window, cx);
        cx.notify();
    }

    /// `RENO_SIDEBAR_OPEN`: acts on the "fix the ACP v2 handshake" thread (or the run the hook
    /// names after a colon).
    fn sidebar_hook(&mut self, hook: &str, window: &mut Window, cx: &mut Context<Self>) {
        let (what, suffix) = hook.split_once(':').unwrap_or((hook, "plan1"));
        let Ok(thread) = reno_proto::ThreadId::from_suffix(suffix) else {
            return;
        };
        let store = self.store.clone();
        self.sidebar.hook(what, thread, &store, window, cx);
        cx.notify();
    }

    /// `RENO_DRAFT33_OPEN`: opens the run the hook names; `stream` starts the long reply, whose
    /// run opens as any new run does.
    pub(crate) fn draft33_hook(
        &mut self,
        hook: Draft33Hook,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open = |suffix: &str| RunId::from_suffix(suffix).ok();
        match hook {
            Draft33Hook::Thread | Draft33Hook::Folds => {
                // The thread's last run shows the whole thread above it.
                if let Some(run) = open(
                    reno_client::fake::transcript::HANDSHAKE_RUNS
                        [reno_client::fake::transcript::TURNS - 1],
                ) {
                    self.open_run(run, window, cx);
                }
            }
            Draft33Hook::Render | Draft33Hook::Select | Draft33Hook::CopyCode => {
                if let Some(run) = open(reno_client::fake::transcript::RENDER_RUN) {
                    self.open_run(run, window, cx);
                }
            }
            Draft33Hook::Stream | Draft33Hook::Latest => {
                let params = reno_proto::TaskRunParams {
                    prompt: "/longreply".to_owned(),
                    executor: "claude-code".parse().ok(),
                    cwd: Some(PathBuf::from("/demo/reno")),
                    ..reno_proto::TaskRunParams::default()
                };
                self.store
                    .update(cx, |store, cx| store.submit_task(params, 0, cx));
            }
        }
    }

    /// `RENO_DRAFT33_OPEN=find|enter|enter3|escape`: the handshake thread with its folds closed
    /// and the find bar on `supervisor.rs`, and then, as the word says, a hit and Escape.
    pub(crate) fn draft33_find_hook(
        &mut self,
        hook: Draft33Find,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.draft33_hook(Draft33Hook::Thread, window, cx);
        let Some(screen) = self.run_screen().cloned() else {
            return;
        };
        screen.update(cx, |screen, cx| {
            let demo = hook
                .goto()
                .map(|(goto, then_close)| crate::screens::run::FindDemo {
                    goto,
                    min_hits: reno_client::fake::transcript::NEEDLE_HITS,
                    then_close,
                });
            screen.demo_find(Draft33Find::QUERY, demo, window, cx);
        });
    }

    /// `RENO_COMPOSER_TYPE` and `RENO_COMPOSER_OPEN`: act on the composer that is showing.
    fn composer_hooks(
        &mut self,
        text: Option<String>,
        open: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let composer = match &self.screen {
            Screen::Home => self.home.read(cx).composer().clone(),
            Screen::Run(run) => run.read(cx).steer_input().clone(),
            _ => return,
        };
        composer.update(cx, |composer, cx| {
            if let Some(text) = text {
                composer.fill(&text, window, cx);
            }
            if let Some(open) = open {
                composer.open_for_capture(&open, window, cx);
            }
        });
    }

    /// The home screen is about to be replaced: its composer lives on, hidden, so it closes its
    /// menus and lets go of the overlay signal.
    fn leave_home(&mut self, cx: &mut Context<Self>) {
        if matches!(self.screen, Screen::Home) {
            let composer = self.home.read(cx).composer().clone();
            composer.update(cx, |c, cx| c.hide(cx));
        }
    }

    /// Shows a built destination; the others say they are not built yet.
    pub(crate) fn navigate(&mut self, nav: Nav, window: &mut Window, cx: &mut Context<Self>) {
        self.show_nav(nav, None, window, cx);
    }

    /// The new-thread state ("Start a task"), working in `folder`'s repository when one is given
    /// (a project's "+").
    pub(crate) fn new_thread(
        &mut self,
        folder: Option<crate::vm::projects::FolderKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_nav(Nav::Home, folder, window, cx);
    }

    fn show_nav(
        &mut self,
        nav: Nav,
        folder: Option<crate::vm::projects::FolderKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.store.update(cx, |store, cx| store.look_at(None, cx));
        self.leave_home(cx);
        self.close_menus(cx);
        let into_settings = matches!(nav, Nav::Settings | Nav::Executors);
        if !into_settings {
            self.before_settings = None;
        } else if !matches!(self.screen, Screen::Settings) {
            self.before_settings = Some(self.screen.clone());
            self.settings.update(cx, |screen, cx| screen.opened(cx));
        }
        self.screen = match nav {
            Nav::Runs => {
                self.runs.update(cx, |screen, cx| screen.shown(cx));
                Screen::Runs
            }
            Nav::Triggers => Screen::Triggers,
            Nav::Executors => {
                self.settings.update(cx, |screen, cx| {
                    screen.set_category(Category::Executors, cx);
                    screen.shown(cx);
                });
                Screen::Settings
            }
            Nav::Settings => {
                self.settings.update(cx, |screen, cx| screen.shown(cx));
                Screen::Settings
            }
            Nav::Home => {
                self.set_home_folder(folder, cx);
                Screen::Home
            }
        };
        self.focus_composer(window, cx);
        self.record(cx);
        cx.notify();
    }

    fn set_home_folder(
        &mut self,
        folder: Option<crate::vm::projects::FolderKey>,
        cx: &mut Context<Self>,
    ) {
        let root = folder
            .as_ref()
            .and_then(|k| k.root())
            .map(std::path::Path::to_path_buf);
        let project = folder.as_ref().map(|k| (k.name(), k.clone()));
        self.home_folder = folder;
        self.home.update(cx, |home, cx| {
            home.set_project(project, cx);
            home.composer()
                .update(cx, |composer, cx| composer.set_folder(root, cx));
        });
    }

    /// Where the person is, for the bar's history; Settings is not a place.
    fn place(&self, cx: &App) -> Option<Place> {
        Some(match &self.screen {
            Screen::Home => Place::NewThread(self.home_folder.clone()),
            Screen::Runs => Place::AllRuns,
            Screen::Run(run) => Place::Run(run.read(cx).run_id().clone()),
            Screen::Project(project) => Place::Project(project.read(cx).key().clone()),
            Screen::Triggers => Place::Triggers,
            Screen::Settings => return None,
        })
    }

    /// The one place the history hears of where the person went. Stepping back or forward
    /// arrives at a place the history is already on, which records nothing.
    fn record(&mut self, cx: &mut Context<Self>) {
        if let Some(place) = self.place(cx) {
            self.history.visit(place);
        }
        // Live runs are followed (for their tails) only while All runs shows.
        let lanes = matches!(self.screen, Screen::Runs);
        self.store
            .update(cx, |store, cx| store.set_follow_live(lanes, cx));
    }

    /// The bar's back and forward. Settings has its own "Back to app", so they rest there.
    pub(crate) fn step_history(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.screen, Screen::Settings) {
            return;
        }
        let place = if back {
            self.history.back()
        } else {
            self.history.forward()
        };
        match place {
            Some(Place::NewThread(folder)) => self.new_thread(folder, window, cx),
            Some(Place::AllRuns) => self.navigate(Nav::Runs, window, cx),
            Some(Place::Run(run)) => self.open_run(run, window, cx),
            Some(Place::Project(key)) => self.open_project(key, window, cx),
            Some(Place::Triggers) => self.navigate(Nav::Triggers, window, cx),
            None => {}
        }
    }

    /// The chip of a run opens that run's project page.
    pub(crate) fn open_current_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Screen::Run(run) = &self.screen else {
            return;
        };
        let id = run.read(cx).run_id().clone();
        let key = {
            let store = self.store.read(cx);
            crate::vm::projects::key_of(store.state().run(&id), |dir| store.root_of(dir))
        };
        self.open_project(key, window, cx);
    }

    pub(crate) fn toggle_more(&mut self, cx: &mut Context<Self>) {
        self.more_open = !self.more_open;
        self.updates_open = false;
        self.updates_shown.clear();
        cx.notify();
    }

    pub(crate) fn toggle_updates(&mut self, cx: &mut Context<Self>) {
        self.updates_open = !self.updates_open;
        self.more_open = false;
        self.updates_shown.clear();
        self.remember_updates(cx);
        cx.notify();
    }

    pub(crate) fn close_menus(&mut self, cx: &mut Context<Self>) {
        if self.restart_open {
            self.restart_cancel(cx);
        }
        if self.more_open || self.updates_open {
            self.more_open = false;
            self.updates_open = false;
            self.updates_shown.clear();
            cx.notify();
        }
    }

    /// The agents with an update to take, as the bar lists them.
    fn agent_updates(&self, cx: &App) -> Vec<AgentUpdate> {
        let store = self.store.read(cx);
        let all = executors::agent_updates(
            store.state(),
            store.now_ms(),
            store.registry().listing.as_ref(),
        );
        // A version the person skipped is not offered again.
        executors::without_skipped(all, &crate::prefs::current(cx).skipped_updates)
    }

    /// While the list is open, keeps its rows so one that finishes reads "Up to date".
    fn remember_updates(&mut self, cx: &App) {
        if self.updates_open {
            let rows = executors::island_rows(&self.agent_updates(cx), &self.updates_shown);
            self.updates_shown = rows.into_iter().map(|(u, _)| u).collect();
        }
    }

    /// "Update" on one agent: the daemon's update job, as on Settings › Executors.
    pub(crate) fn update_agent(&mut self, id: &str, cx: &mut Context<Self>) {
        // A job that ends before the list is drawn again still reads "Up to date".
        if let Some(shown) = self.updates_shown.iter_mut().find(|u| u.id == id) {
            shown.updating = true;
        }
        if let Ok(id) = id.parse::<reno_proto::ExecutorId>() {
            self.store.update(cx, |store, cx| {
                store.start_executor_job(id, reno_proto::JobKind::Update, false, cx);
            });
        }
    }

    pub(crate) fn update_all_agents(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .agent_updates(cx)
            .into_iter()
            .filter(|u| !u.updating)
            .map(|u| u.id)
            .collect();
        for id in ids {
            self.update_agent(&id, cx);
        }
    }

    /// Shows one page of Settings.
    pub(crate) fn open_settings(
        &mut self,
        category: Category,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate(Nav::Settings, window, cx);
        self.settings
            .update(cx, |screen, cx| screen.set_category(category, cx));
    }

    /// "Back to app": the screen Settings opened over, or Home.
    pub(crate) fn leave_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.before_settings.take() {
            Some(Screen::Run(run)) => {
                // The kept screen, not a new one: its draft and its panel stay as they were.
                let id = run.read(cx).run_id().clone();
                self.store
                    .update(cx, |store, cx| store.look_at(Some(id), cx));
                self.screen = Screen::Run(run);
                self.focus_composer(window, cx);
                self.record(cx);
                cx.notify();
            }
            Some(Screen::Project(project)) => {
                self.screen = Screen::Project(project);
                self.focus_composer(window, cx);
                self.record(cx);
                cx.notify();
            }
            Some(Screen::Runs) => self.navigate(Nav::Runs, window, cx),
            Some(Screen::Triggers) => self.navigate(Nav::Triggers, window, cx),
            // The new-thread state again, in the project it was for.
            Some(Screen::Home) => self.new_thread(self.home_folder.clone(), window, cx),
            Some(Screen::Settings) | None => self.navigate(Nav::Home, window, cx),
        }
    }

    /// Shows one run and follows its transcript.
    pub(crate) fn open_run(&mut self, run: RunId, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Run(open) = &self.screen
            && open.read(cx).run_id() == &run
        {
            return;
        }
        self.pending_fired = None;
        self.store
            .update(cx, |store, cx| store.look_at(Some(run.clone()), cx));
        let store = self.store.clone();
        let screen = cx.new(|cx| RunScreen::new(store, run, cx));
        let steer = screen.read(cx).steer_input().clone();
        self._run_events = vec![
            cx.subscribe_in(&screen, window, Self::on_screen_event),
            cx.subscribe_in(&steer, window, Self::on_open_executors),
        ];
        self.leave_home(cx);
        self.close_menus(cx);
        self.screen = Screen::Run(screen);
        self.focus_composer(window, cx);
        self.record(cx);
        cx.notify();
    }

    /// A panel key command, for the run screen when one is showing.
    fn panel_action(&mut self, action: PanelAction, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Run(run) = &self.screen {
            run.update(cx, |run, cx| run.panel_action(action, window, cx));
        }
    }

    /// Closes the run screen's find bar if it is open. Whether it was.
    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Screen::Run(run) = &self.screen else {
            return false;
        };
        run.update(cx, |run, cx| {
            let was = run.find_is_open();
            run.close_find(window, cx);
            was
        })
    }

    /// ⌘W: the run panel's tab when the panel is open, else the window.
    fn close_tab_or_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panel_open = match &self.screen {
            Screen::Run(run) => run.read(cx).panel_is_open(window, cx),
            _ => false,
        };
        if panel_open {
            self.panel_action(PanelAction::CloseTab, window, cx);
        } else {
            window.remove_window();
        }
    }

    /// Puts `tab` in front in the open run's side panel.
    pub(crate) fn show_panel_tab(
        &mut self,
        tab: Surface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Screen::Run(run) = &self.screen {
            run.update(cx, |run, cx| run.open_surface(tab, window, cx));
        }
    }

    /// Shows a project's Allowlist and History.
    pub(crate) fn open_project(
        &mut self,
        key: crate::vm::projects::FolderKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.store.update(cx, |store, cx| store.look_at(None, cx));
        let store = self.store.clone();
        let screen = cx.new(|cx| crate::screens::project::ProjectScreen::new(store, key, cx));
        self.leave_home(cx);
        self.close_menus(cx);
        self.screen = Screen::Project(screen);
        self.focus_composer(window, cx);
        self.record(cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn project_screen(&self) -> Option<&Entity<crate::screens::project::ProjectScreen>> {
        match &self.screen {
            Screen::Project(project) => Some(project),
            _ => None,
        }
    }

    /// Opens a closed project folder, or closes an open one.
    pub(crate) fn toggle_folder(
        &mut self,
        key: crate::vm::projects::FolderKey,
        cx: &mut Context<Self>,
    ) {
        if !self.folders_toggled.remove(&key) {
            self.folders_toggled.insert(key);
        }
        cx.notify();
    }

    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_hidden = !self.sidebar_hidden;
        cx.notify();
    }

    /// The Toggle theme command: writes Light or Dark into the saved mode, so Settings ›
    /// Appearance says what is in force.
    pub(crate) fn toggle_theme(&mut self, window: &Window, cx: &mut Context<Self>) {
        let flipped = self.appearance(window).toggled();
        let mode = match flipped {
            Appearance::Light => crate::prefs::Mode::Light,
            Appearance::Dark => crate::prefs::Mode::Dark,
        };
        self.mode = mode;
        self.appearance_override = Some(flipped);
        crate::prefs::update(cx, |p| p.mode = mode);
        cx.notify();
    }

    pub(crate) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            return;
        }
        let (threads, scope) = self.palette_inputs(cx);
        let store = self.store.clone();
        let palette = cx.new(|cx| Palette::new(store, threads, scope, cx));
        cx.subscribe_in(
            &palette,
            window,
            |this, _, event: &PaletteEvent, window, cx| {
                this.close_palette(window, cx);
                match event {
                    PaletteEvent::Run(action) => this.run_palette_action(*action, window, cx),
                    PaletteEvent::OpenRun(run) => this.open_run(run.clone(), window, cx),
                    PaletteEvent::OpenFile { path, line } => {
                        if let Some(screen) = this.run_screen().cloned() {
                            screen.update(cx, |screen, cx| {
                                screen.open_file_at(path, *line, window, cx);
                            });
                        }
                    }
                    PaletteEvent::Close => {}
                }
            },
        )
        .detach();
        let handle = palette.focus_handle(cx);
        window.focus(&handle, cx);
        overlay::set(cx, palette.entity_id(), true);
        self.palette = Some(palette);
        cx.notify();
    }

    /// The palette's threads (the sidebar's rows) and the focused thread whose files it searches.
    fn palette_inputs(
        &self,
        cx: &App,
    ) -> (
        Vec<crate::vm::palette::Thread>,
        Option<crate::ui::palette::Scope>,
    ) {
        use crate::vm::{palette, projects};
        let store = self.store.read(cx);
        let state = store.state();
        let runs = state.runs();
        let toggled = std::collections::HashSet::new();
        let mut folders = projects::folders(
            &runs,
            |dir| store.root_of(dir),
            &projects::Inputs {
                seen: store.seen(),
                toggled: &toggled,
                looking_at: store.looking_at(),
                now_ms: store.now_ms(),
                thread_of: &|run| store.thread_of(run),
                edit: None,
            },
        );
        // Archived threads are not places to jump to.
        if let Some(marks) = store.thread_marks() {
            palette::without_archived(&mut folders, &marks.archived);
        }
        let threads = palette::threads(&folders, |run| {
            state.run(run).map_or(0, |r| r.updated_at_ms)
        });
        let scope = store.looking_at().and_then(|looking| {
            folders
                .iter()
                .find(|f| f.rows.iter().any(|r| r.members.contains(looking)))
                .map(|f| crate::ui::palette::Scope {
                    project: f.name.clone(),
                    run: looking.clone(),
                })
        });
        (threads, scope)
    }

    pub(crate) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(palette) = self.palette.take() {
            overlay::set(cx, palette.entity_id(), false);
            self.focus_composer(window, cx);
            cx.notify();
        }
    }

    fn run_palette_action(
        &mut self,
        action: PaletteAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            PaletteAction::Go(nav) => self.navigate(nav, window, cx),
            PaletteAction::ToggleTheme => self.toggle_theme(window, cx),
            PaletteAction::ToggleSidebar => self.toggle_sidebar(cx),
        }
    }

    fn handlers(&self, cx: &mut Context<Self>) -> SidebarHandlers {
        let this = cx.entity();
        let navigate = {
            let this = this.clone();
            Rc::new(move |nav: Nav, window: &mut Window, cx: &mut App| {
                this.update(cx, |root, cx| root.navigate(nav, window, cx));
            })
        };
        SidebarHandlers {
            navigate,
            new_thread: {
                let this = this.clone();
                Rc::new(
                    move |key: crate::vm::projects::FolderKey,
                          window: &mut Window,
                          cx: &mut App| {
                        this.update(cx, |root, cx| root.new_thread(Some(key), window, cx));
                    },
                )
            },
            recent_run: {
                let this = this.clone();
                Rc::new(move |run: RunId, window: &mut Window, cx: &mut App| {
                    this.update(cx, |root, cx| root.open_run(run, window, cx));
                })
            },
            toggle_folder: {
                let this = this.clone();
                Rc::new(
                    move |key: crate::vm::projects::FolderKey, _: &mut Window, cx: &mut App| {
                        this.update(cx, |root, cx| root.toggle_folder(key, cx));
                    },
                )
            },
            open_project: {
                let this = this.clone();
                Rc::new(
                    move |key: crate::vm::projects::FolderKey,
                          window: &mut Window,
                          cx: &mut App| {
                        this.update(cx, |root, cx| root.open_project(key, window, cx));
                    },
                )
            },
        }
    }
}

impl Focusable for RootView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::bench::frame();
        // Settings › Appearance: the saved mode, theme and accent.
        let prefs = crate::prefs::current(cx);
        self.mode = prefs.mode;
        let appearance = self.appearance(window);
        let theme = Theme::from_palette(
            appearance,
            &theme::palette_for(prefs.theme, appearance, prefs.accent),
        );
        theme::set_current(theme, cx);
        let handlers = self.handlers(cx);
        let sidebar_width = self.sidebar.width;
        let act: sidebar::ActHandler = {
            let this = cx.entity();
            Rc::new(move |act, window, cx| {
                this.update(cx, |root, cx| root.sidebar_act(act, window, cx));
            })
        };
        // What the run screen's panel needs to fit itself on its first frame: the sidebar and
        // the insets around the canvas.
        cx.set_global(crate::panel::view::StageInset(if self.sidebar_hidden {
            2.0 * GUTTER
        } else {
            sidebar_width + GUTTER
        }));
        let in_settings = matches!(self.screen, Screen::Settings);
        let (needing, connection) = {
            let store = self.store.read(cx);
            (
                crate::vm::executors::needing_count(
                    store.state(),
                    store.now_ms(),
                    store.registry().listing.as_ref(),
                ),
                store.connection(),
            )
        };
        // While Settings shows, its navigation takes the sidebar's place.
        let settings_nav = (in_settings && !self.sidebar_hidden).then(|| {
            let this = cx.entity();
            let actions = NavActions {
                back: {
                    Rc::new(move |window: &mut Window, cx: &mut App| {
                        this.update(cx, |root, cx| root.leave_settings(window, cx));
                    })
                },
            };
            self.settings.update(cx, |screen, cx| {
                screen.nav_column(&theme, needing, connection, &actions, cx)
            })
        });

        let on_run = matches!(self.screen, Screen::Run(_));
        // Setup (the wizard) and the connecting gate are overlays under the status bar.
        let setup_wizard = self
            .setup
            .clone()
            .map(|flow| crate::ui::setup::wizard::overlay(&flow, &theme, cx));
        let gate_splash = self
            .gate
            .map(|phase| crate::ui::gate::splash(&theme, phase));
        let panel = div()
            .id("canvas-panel")
            .debug_selector(|| "canvas-panel".to_owned())
            .flex_1()
            .min_h_0()
            // One gutter under the bar's controls, as between and around the canvases.
            .mt(px(shell::canvas_top_margin()))
            .mb(px(GUTTER))
            .mr(px(GUTTER))
            .when(self.sidebar_hidden, |p| p.ml(px(GUTTER)))
            // A run draws its own two canvases; every other screen sits on this one.
            .when(!on_run, |p| {
                p.rounded(px(14.0))
                    .bg(theme.canvas)
                    .border_1()
                    .border_color(theme.hairline)
                    .overflow_hidden()
            })
            .child(match &self.screen {
                Screen::Home => self.home.clone().into_any_element(),
                Screen::Runs => self.runs.clone().into_any_element(),
                Screen::Run(run) => run.clone().into_any_element(),
                Screen::Project(project) => project.clone().into_any_element(),
                Screen::Triggers => self.triggers.clone().into_any_element(),
                Screen::Settings => self.settings.clone().into_any_element(),
            });

        let main = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(panel);

        let bar_state = {
            let place = self.place(cx);
            let store = self.store.read(cx);
            let chip = match &place {
                Some(place) => shell::chip(place, store.state(), |dir| store.root_of(dir)),
                None => Chip {
                    initial: "S".to_owned(),
                    title: "Settings".to_owned(),
                },
            };
            let current = self.agent_updates(cx);
            let remembered = if self.updates_open {
                self.updates_shown.as_slice()
            } else {
                &[]
            };
            (
                chip,
                shell::live_runs(store.state()).len(),
                executors::island_rows(&current, remembered),
            )
        };
        let (chip, live, updates) = bar_state;
        let bar = Bar {
            theme: &theme,
            chip,
            chip_opens_project: on_run,
            can_back: self.history.can_back() && !in_settings,
            can_forward: self.history.can_forward() && !in_settings,
            live,
            connection,
            updates: &updates,
            updates_open: self.updates_open,
            more_open: self.more_open,
            on_run,
            more_at: self.more_at.clone(),
            updates_at: self.updates_at.clone(),
            restart: {
                let (versions, live) = {
                    let store = self.store.read(cx);
                    // Nothing to restart until renod has answered.
                    (
                        store
                            .daemon_versions()
                            .filter(|_| connection == reno_client::ConnectionStatus::Connected),
                        shell::live_runs(store.state()).len(),
                    )
                };
                crate::ui::restart::RestartBar {
                    phase: self.restart.phase.clone(),
                    offered: crate::vm::daemon_restart::offered(versions.as_ref(), &self.restart),
                    open: self.restart_open,
                    versions,
                    live,
                    at: self.restart_at.clone(),
                }
            },
            draft_label: std::cell::RefCell::new(crate::ui::draft33::label(&theme, cx)),
        };
        let this = cx.entity();
        let status_bar = bar::status_bar(&this, &bar);
        let popovers = bar::popovers(&this, &bar);
        let view = sidebar::View::new(
            self.store.read(cx),
            &self.folders_toggled,
            &self.sidebar,
            Rc::clone(&act),
        );
        let sidebar_overlays = sidebar::overlays(&theme, &view);

        div()
            .id("root")
            .key_context({
                let mut context = String::from("Root");
                if self.sidebar.any_open() {
                    context = format!("Root {}", actions::SIDEBAR_OPEN);
                } else if self.restart_open {
                    context = format!("Root {}", crate::ui::restart::OPEN_CONTEXT);
                }
                // While find is open in the run screen, Escape and ⌘G are its.
                if matches!(&self.screen, Screen::Run(run) if run.read(cx).find_is_open()) {
                    context = format!("{context} {}", crate::ui::find::OPEN_CONTEXT);
                }
                gpui::KeyContext::parse(&context).unwrap_or_default()
            })
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _: &OpenPalette, window, cx| this.open_palette(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(
                cx.listener(|this, _: &crate::ui::restart::Dismiss, _, cx| this.restart_cancel(cx)),
            )
            .on_action(cx.listener(|this, _: &actions::DismissSidebar, w, cx| {
                this.sidebar_act(sidebar::Act::Dismiss, w, cx);
            }))
            // The composer lets Escape go when it has nothing to cancel: the draft takes it. And
            // when the find bar is open, Escape is the bar's (the composer lets it go).
            .on_action(cx.listener(|this, _: &composer::Cancel, w, cx| {
                if this.sidebar.any_open() {
                    this.sidebar_act(sidebar::Act::Dismiss, w, cx);
                } else if !this.close_find(w, cx) {
                    // Nothing of the screen's was open: the draft takes it.
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &actions::CloseFind, w, cx| {
                this.close_find(w, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::FindInTranscript, w, cx| {
                if let Screen::Run(run) = &this.screen {
                    run.update(cx, |run, cx| run.open_find(w, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &actions::FindNext, _, cx| {
                if let Screen::Run(run) = &this.screen {
                    run.update(cx, |run, cx| run.find_step(true, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &actions::FindPrevious, _, cx| {
                if let Screen::Run(run) = &this.screen {
                    run.update(cx, |run, cx| run.find_step(false, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &actions::PinThread, w, cx| {
                this.sidebar_key(false, w, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::ArchiveThread, w, cx| {
                this.sidebar_key(true, w, cx);
            }))
            .on_drag_move(cx.listener(
                |this, e: &gpui::DragMoveEvent<sidebar::SidebarDrag>, w, cx| {
                    this.sidebar_act(sidebar::Act::Resize(f32::from(e.event.position.x)), w, cx);
                },
            ))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, w, cx| this.sidebar_act(sidebar::Act::ResizeEnd, w, cx)),
            )
            .on_action(cx.listener(|this, _: &TogglePanel, w, cx| {
                this.panel_action(PanelAction::Toggle, w, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpToPanel, w, cx| {
                if let Screen::Run(run) = &this.screen {
                    run.update(cx, |run, cx| run.jump_to_panel(w, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &NewPanelTab, w, cx| {
                this.panel_action(PanelAction::NewTab, w, cx);
            }))
            .on_action(cx.listener(|this, _: &ClosePanelTab, w, cx| {
                this.close_tab_or_window(w, cx);
            }))
            .on_action(cx.listener(|this, _: &ReopenPanelTab, w, cx| {
                this.panel_action(PanelAction::ReopenTab, w, cx);
            }))
            .on_action(cx.listener(|this, _: &NextPanelTab, w, cx| {
                this.panel_action(PanelAction::NextTab, w, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevPanelTab, w, cx| {
                this.panel_action(PanelAction::PrevTab, w, cx);
            }))
            .on_action(
                cx.listener(|this, _: &GoRuns, window, cx| this.navigate(Nav::Runs, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleTheme, window, cx| this.toggle_theme(window, cx)),
            )
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.shell)
            .text_color(theme.text)
            .font_family(SANS)
            .text_size(px(13.5))
            .child(status_bar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .when(!self.sidebar_hidden, |body| {
                        if let Some(nav) = settings_nav {
                            body.child(nav)
                        } else {
                            body.child(sidebar::sidebar(
                                &theme,
                                self.store.read(cx),
                                self.nav(),
                                needing,
                                &handlers,
                                &view,
                                sidebar_width,
                            ))
                        }
                    })
                    .child(main),
            )
            .children(setup_wizard)
            .children(gate_splash)
            .children(popovers)
            .children(sidebar_overlays)
            .children(self.palette.clone())
            .child(self.toasts.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str], env: &[(&str, &str)]) -> Result<Options, String> {
        Options::parse(args.iter().map(|a| (*a).to_owned()), |key| {
            env.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
        })
    }

    #[test]
    fn bench_is_off_unless_reno_bench_is_one() {
        assert!(!parse(&[], &[]).unwrap().bench);
        assert!(!parse(&[], &[("RENO_BENCH", "0")]).unwrap().bench);
        assert!(parse(&[], &[("RENO_BENCH", "1")]).unwrap().bench);
    }

    #[test]
    fn open_folds_is_a_demo_switch_for_the_run_screen() {
        assert!(!parse(&["--fake"], &[]).unwrap().open_folds);
        let o = parse(&["--fake"], &[("RENO_OPEN_FOLDS", "1")]).unwrap();
        assert!(o.open_folds);
        assert!(
            !parse(&["--fake"], &[("RENO_OPEN_FOLDS", "0")])
                .unwrap()
                .open_folds
        );
    }

    #[test]
    fn the_composer_hooks_exist_only_with_the_fake_daemon() {
        let hooks = [
            ("RENO_COMPOSER_TYPE", "@src/"),
            ("RENO_COMPOSER_OPEN", "branch"),
            ("RENO_COMPOSER_FAIL", "commands"),
        ];
        let off = parse(&[], &hooks).unwrap();
        assert_eq!(
            (off.composer_type, off.composer_open, off.composer_fail),
            (None, None, None)
        );
        let on = parse(&["--fake"], &hooks).unwrap();
        assert_eq!(on.composer_type.as_deref(), Some("@src/"));
        assert_eq!(on.composer_open.as_deref(), Some("branch"));
        assert_eq!(on.composer_fail.as_deref(), Some("commands"));
    }

    #[test]
    fn the_sidebar_start_hook_exists_only_with_the_fake_daemon_and_is_checked() {
        let hook = |h: &'static str| ("RENO_SIDEBAR_OPEN", h);
        assert_eq!(parse(&[], &[hook("menu")]).unwrap().sidebar_open, None);
        let on = parse(&["--fake"], &[hook("close:atlas01")]).unwrap();
        assert_eq!(on.sidebar_open.as_deref(), Some("close:atlas01"));
        assert!(parse(&["--fake"], &[hook("dance")]).is_err());
        for h in [
            "menu", "copy", "rename", "archived", "more", "archive", "resize", "close",
        ] {
            assert!(parse(&["--fake"], &[hook(h)]).is_ok(), "{h}");
        }
    }

    #[test]
    fn the_setup_hook_exists_only_with_the_fake_daemon_and_is_checked() {
        use crate::vm::first_run::{Hook, Step};
        let hook = |h: &'static str| ("RENO_SETUP", h);
        assert_eq!(parse(&[], &[hook("skip")]).unwrap().setup, None);
        assert_eq!(
            parse(&["--fake"], &[hook("skip")]).unwrap().setup,
            Some(Hook::Skip)
        );
        for (word, step) in [
            ("welcome", Step::Welcome),
            ("look", Step::Look),
            ("agents", Step::Agents),
            ("project", Step::Project),
            ("done", Step::Done),
        ] {
            assert_eq!(
                parse(&["--fake"], &[hook(word)]).unwrap().setup,
                Some(Hook::At(step))
            );
        }
        assert!(parse(&["--fake"], &[hook("sideways")]).is_err());
    }

    #[test]
    fn no_flags_means_the_socket() {
        assert_eq!(parse(&[], &[]), Ok(Options::default()));
        assert!(!Options::default().fake);
    }

    #[test]
    fn fake_flag_and_environment() {
        let o = parse(
            &["--fake"],
            &[
                ("RENO_APPEARANCE", "light"),
                ("RENO_QUIT_AFTER_MS", "250"),
                ("RENO_REDUCE_MOTION", "1"),
                ("RENO_OPEN", "executors"),
            ],
        );
        assert_eq!(
            o,
            Ok(Options {
                fake: true,
                appearance: Some(Appearance::Light),
                quit_after: Some(Duration::from_millis(250)),
                reduce_motion: true,
                split: false,
                open: Some(Nav::Executors),
                open_run: None,
                open_project: None,
                open_new_thread: None,
                panel: None,
                files_open: None,
                files_search: None,
                terminal_input: None,
                follow_up: None,
                open_folds: false,
                demo_menu: None,
                activity_open: None,
                registry_demo: None,
                open_settings: None,
                data_dir: None,
                ..Options::default()
            })
        );
        let thread = parse(&[], &[("RENO_OPEN", "newthread:/work/atlas-api")]).unwrap();
        assert_eq!(
            thread.open_new_thread,
            Some(PathBuf::from("/work/atlas-api"))
        );
        let policy = parse(&[], &[("RENO_OPEN", "settings:policy")]).unwrap();
        assert_eq!(policy.open, Some(Nav::Settings));
        assert_eq!(policy.open_settings, Some(Category::Policy));
        assert!(parse(&[], &[("RENO_OPEN", "settings:nope")]).is_err());
        let run = parse(&[], &[("RENO_OPEN", "run_keeper1")]).unwrap();
        assert_eq!(run.open, None);
        assert_eq!(
            run.open_run.map(|r| r.to_string()).as_deref(),
            Some("run_keeper1")
        );
    }

    #[test]
    fn a_fake_run_keeps_the_panel_state_only_in_a_directory_it_was_given() {
        let none = parse(&["--fake"], &[]).unwrap();
        assert_eq!(none.panel_state_path(), None);
        let given = parse(&["--fake"], &[("RENO_DATA_DIR", "/tmp/x")]).unwrap();
        assert_eq!(
            given.panel_state_path(),
            Some(PathBuf::from("/tmp/x/app/panel-state.json"))
        );
    }

    #[test]
    fn the_panel_tab_to_show_comes_from_the_environment() {
        let tab = |name| parse(&[], &[("RENO_PANEL", name)]).map(|o| o.panel);
        assert_eq!(tab("home"), Ok(Some(Surface::Home)));
        assert_eq!(tab("session"), Ok(Some(Surface::Session)));
        assert_eq!(tab("review"), Ok(Some(Surface::Review)));
        assert_eq!(
            tab("file:src/a.rs"),
            Ok(Some(Surface::File("src/a.rs".into())))
        );
        assert_eq!(tab(""), Ok(None));
        assert_eq!(tab("files"), Ok(Some(Surface::Files)));
        assert_eq!(tab("history"), Ok(Some(Surface::History)));
        assert_eq!(
            tab("commit:e6e6e6e"),
            Ok(Some(Surface::Commit("e6e6e6e".into())))
        );
        assert!(tab("commit:").is_err());
        assert!(tab("filez").is_err());
        assert!(tab("file:").is_err());
    }

    #[test]
    fn the_transcript_draft_needs_the_fake_daemon_and_is_checked() {
        let draft = |args: &[&str], env: &[(&str, &str)]| parse(args, env);
        // Without `--fake` it is refused, whatever else is given.
        assert_eq!(
            draft(&["--draft33"], &[]).unwrap_err(),
            "--draft33 needs --fake"
        );
        // With it, variant A unless `RENO_DRAFT33` says otherwise.
        let a = draft(&["--fake", "--draft33"], &[]).unwrap();
        assert_eq!(a.draft33, Some(Draft33Variant::A));
        assert_eq!(a.draft33_open, None);
        for (key, want) in [("b", Draft33Variant::B), ("C", Draft33Variant::C)] {
            let o = draft(&["--fake", "--draft33"], &[("RENO_DRAFT33", key)]).unwrap();
            assert_eq!(o.draft33, Some(want));
        }
        assert!(draft(&["--fake", "--draft33"], &[("RENO_DRAFT33", "d")]).is_err());
        // The start hook is a word; an unknown one is ignored, not refused.
        for (word, want) in [
            ("thread", Some(Draft33Hook::Thread)),
            ("folds", Some(Draft33Hook::Folds)),
            ("render", Some(Draft33Hook::Render)),
            ("stream", Some(Draft33Hook::Stream)),
            ("nonsense", None),
        ] {
            let o = draft(&["--fake", "--draft33"], &[("RENO_DRAFT33_OPEN", word)]).unwrap();
            assert_eq!(o.draft33_open, want, "{word}");
        }
        // Without `--draft33` the app is as it was: the variables mean nothing.
        let plain = draft(
            &["--fake"],
            &[("RENO_DRAFT33", "c"), ("RENO_DRAFT33_OPEN", "folds")],
        )
        .unwrap();
        assert_eq!(plain.draft33, None);
        assert_eq!(plain.draft33_open, None);
    }

    #[test]
    fn bad_input_is_ignored_or_rejected() {
        assert!(parse(&["--wat"], &[]).is_err());
        let o = parse(
            &[],
            &[("RENO_APPEARANCE", "plaid"), ("RENO_QUIT_AFTER_MS", "soon")],
        );
        assert_eq!(o, Ok(Options::default()));
        assert!(parse(&[], &[("RENO_OPEN", "flows")]).is_err());
        assert_eq!(parse(&[], &[("RENO_OPEN", "home")]), Ok(Options::default()));
    }

    #[test]
    fn the_window_opens_large_on_a_laptop_and_capped_on_a_big_display() {
        let at = |w: f32, h: f32| window_size(size(px(w), px(h)));
        // A 14-inch laptop's visible area: most of it, not the whole.
        assert_eq!(
            at(1512.0, 944.0),
            size(px(1239.84), px(811.84)).max(&size(px(1100.0), px(700.0)))
        );
        // A small screen: never larger than the screen.
        assert_eq!(at(1024.0, 640.0), size(px(1024.0), px(640.0)));
        // A big display: capped, so lines stay readable.
        assert_eq!(at(3440.0, 1440.0), size(px(1480.0), px(960.0)));
        // Never below the minimum the layout needs when the screen allows it.
        let s = at(1280.0, 800.0);
        assert!(s.width >= px(1100.0) && s.height >= px(700.0));
    }
}
