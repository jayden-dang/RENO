//! The app outliving its window: what is held at app level (the store, the toasts, the notifier),
//! the Dock click that builds a window again, banner clicks, and the menu bar.
//!
//! Closing the last window does not quit; ⌘Q (or `RENO_QUIT_AFTER_MS`) does.

use std::rc::Rc;

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{App, Entity, Global, Menu, MenuItem, SystemMenuType};

use crate::actions::{About, CloseWindow, Minimize, OpenSettings, Quit};
use crate::app::{self, Options, RootView, Start};
use crate::banners::Banners;
use crate::desktop::{Desktop, Native, Silent};
use crate::nav::Nav;
use crate::notify::Target;
use crate::prefs::{self, PrefStore};
use crate::store::Store;
use crate::ui::theme::Appearance;
use crate::ui::toast::{self, Toasts};

/// What lives as long as the app.
pub(crate) struct Shell {
    store: Entity<Store>,
    toasts: Entity<Toasts>,
    appearance: Option<Appearance>,
    banners: Entity<Banners>,
    /// Banner clicks, as their targets: the click handler fires mid-AppKit-event and only sends.
    clicks: UnboundedSender<String>,
}

impl Global for Shell {}

impl Shell {
    /// Sends what a banner click would.
    #[cfg(test)]
    pub(crate) fn click(&self, target: &str) {
        let _ = self.clicks.unbounded_send(target.to_owned());
    }
}

/// The notifier of the app, when one is installed (a test window without the shell has none).
pub(crate) fn banners(cx: &App) -> Option<Entity<Banners>> {
    cx.try_global::<Shell>().map(|shell| shell.banners.clone())
}

/// Installs the app-level state for the real app. The real desktop (banners, Dock badge) is built
/// here and only here, and only when connecting to the real socket: `--fake` is silent, and tests
/// call [`install_with`] with a recorder.
pub(crate) fn install(
    cx: &mut App,
    store: Entity<Store>,
    toasts: Entity<Toasts>,
    options: &Options,
) {
    if options.fake {
        install_with(
            cx,
            store,
            toasts,
            options.appearance,
            Rc::new(Silent),
            PrefStore::in_memory(),
        );
        return;
    }
    let prefs = reno_client::default_socket_path()
        .and_then(|socket| prefs::settings_path(&socket))
        .map_or_else(PrefStore::in_memory, PrefStore::at);
    install_with(
        cx,
        store,
        toasts,
        options.appearance,
        Rc::new(Native::new()),
        prefs,
    );
    if let Some(shell) = cx.try_global::<Shell>() {
        let clicks = shell.clicks.clone();
        Native::on_click(move |target| {
            let _ = clicks.unbounded_send(target);
        });
    }
}

/// Installs the app-level state over any [`Desktop`].
pub(crate) fn install_with(
    cx: &mut App,
    store: Entity<Store>,
    toasts: Entity<Toasts>,
    appearance: Option<Appearance>,
    desktop: Rc<dyn Desktop>,
    prefs: PrefStore,
) {
    let banners = Banners::install(cx, store.clone(), desktop, prefs);
    let (clicks, mut received) = unbounded::<String>();
    cx.set_global(Shell {
        store,
        toasts,
        appearance,
        banners,
        clicks,
    });
    cx.spawn(async move |cx| {
        while let Some(target) = received.next().await {
            cx.update(|cx| open_target(&target, cx));
        }
    })
    .detach();

    // A key press runs these with the focused window taken out of the app, so each does its work
    // once the dispatch is over.
    cx.on_action(|_: &OpenSettings, cx| {
        cx.defer(|cx| {
            show(cx, |root, window, cx| {
                root.navigate(Nav::Settings, window, cx);
            });
        });
    });
    cx.on_action(|_: &About, cx| {
        cx.defer(|cx| {
            activate_main_window(cx);
            toast::push(
                cx,
                toast::Tone::Info,
                "Reno",
                format!("Reno {}", env!("CARGO_PKG_VERSION")),
            );
        });
    });
    cx.on_action(|_: &CloseWindow, cx| {
        cx.defer(|cx| {
            if let Some(window) = cx.active_window() {
                window
                    .update(cx, |_, window, _| window.remove_window())
                    .ok();
            }
        });
    });
    cx.on_action(|_: &Minimize, cx| {
        cx.defer(|cx| {
            if let Some(window) = cx.active_window() {
                window
                    .update(cx, |_, window, _| window.minimize_window())
                    .ok();
            }
        });
    });
    cx.set_menus(menus());
}

/// The menu bar: so ⌘Q and ⌘, work with no window open.
fn menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "Reno".into(),
            items: vec![
                MenuItem::action("About Reno", About),
                MenuItem::separator(),
                MenuItem::action("Settings\u{2026}", OpenSettings),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Quit Reno", Quit),
            ],
            disabled: false,
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Close Window", CloseWindow),
                MenuItem::action("Minimize", Minimize),
            ],
            disabled: false,
        },
    ]
}

/// A Dock click: with no window open, builds one around the running store.
pub(crate) fn reopen(cx: &mut App) {
    if cx.windows().is_empty() {
        open_main_window(cx);
    }
}

fn open_main_window(cx: &mut App) {
    let Some(shell) = cx.try_global::<Shell>() else {
        return;
    };
    let (store, toasts, appearance) = (shell.store.clone(), shell.toasts.clone(), shell.appearance);
    app::open_window(store, toasts, appearance, Start::default(), cx);
}

/// Brings Reno forward, opening the window first if it was closed.
pub(crate) fn activate_main_window(cx: &mut App) {
    cx.activate(true);
    reopen(cx);
}

/// Runs `f` on the main window's root, after bringing Reno forward.
fn show(
    cx: &mut App,
    f: impl FnOnce(&mut RootView, &mut gpui::Window, &mut gpui::Context<RootView>),
) {
    activate_main_window(cx);
    let root = cx
        .windows()
        .into_iter()
        .find_map(|window| window.downcast::<RootView>());
    if let Some(root) = root {
        root.update(cx, |root, window, cx| {
            window.activate_window();
            f(root, window, cx);
        })
        .ok();
    }
}

/// A clicked banner: Reno comes forward on the run it announced, or on Home (the daemon) or the
/// Runs screen (a burst). A target this build does not know only brings Reno forward.
fn open_target(raw: &str, cx: &mut App) {
    let Some(target) = Target::parse(raw) else {
        activate_main_window(cx);
        return;
    };
    if let Target::Run(run) = &target
        && let Some(banners) = banners(cx)
    {
        banners.update(cx, |banners, _| banners.run_opened(run));
    }
    show(cx, move |root, window, cx| match target {
        Target::Run(run) => root.open_run(run, window, cx),
        Target::Home => root.navigate(Nav::Home, window, cx),
        Target::Runs => root.navigate(Nav::Runs, window, cx),
    });
}
