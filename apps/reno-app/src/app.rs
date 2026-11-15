//! The shell: window setup and the root view.
//!
//! The window is opaque and two-tone (UI brief amendment 6): the window, sidebar and rail sit on
//! `shell`; every page sits in one inset rounded `canvas` panel with a hairline. The titlebar is
//! transparent so the traffic lights float over `shell`. Light and dark follow the system
//! appearance unless `RENO_APPEARANCE` or the palette's "Toggle theme" says otherwise.

use std::borrow::Cow;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Subscription,
    TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds, WindowControlArea,
    WindowOptions, div, point, prelude::*, px, size,
};
use gpui_platform::application;
use reno_proto::RunId;

use crate::actions::{self, OpenPalette, Quit, ToggleSidebar, ToggleTheme};
use crate::nav::Nav;
use crate::screens::home::HomeScreen;
use crate::screens::run::RunScreen;
use crate::store::{Store, StoreEvent};
use crate::ui::composer;
use crate::ui::icon::{Assets, Icon};
use crate::ui::kit::{self, SANS};
use crate::ui::motion::{self, ReduceMotion};
use crate::ui::palette::{Palette, PaletteAction, PaletteEvent};
use crate::ui::sidebar::{self, SidebarHandlers};
use crate::ui::theme::{self, Appearance, Theme};
use crate::ui::toast::{self, Toasts};

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
}

impl Options {
    pub(crate) fn parse(
        args: impl IntoIterator<Item = String>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let mut options = Self::default();
        for arg in args {
            match arg.as_str() {
                "--fake" => options.fake = true,
                other => return Err(format!("unknown argument: {other}")),
            }
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
        Ok(options)
    }
}

/// Registers Geist and Geist Mono (SIL OFL, see `assets/fonts/Geist-OFL.txt`).
fn register_fonts(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/Geist.ttf").as_slice()),
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

pub(crate) fn run(options: Options) {
    application().with_assets(Assets).run(move |cx: &mut App| {
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
        cx.on_action(|_: &Quit, cx| cx.quit());

        let toasts = toast::install(cx);
        let store = cx.new(|cx| {
            let mut store = Store::connecting();
            if options.fake {
                store.start_fake(cx);
            } else {
                store.start_socket(cx);
            }
            store
        });

        let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
        let appearance = options.appearance;
        let opened = cx.open_window(
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
                let root = cx.new(|cx| RootView::new(store, toasts, appearance, window, cx));
                root.update(cx, |root, cx| root.focus_composer(window, cx));
                root
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

/// What the canvas panel shows.
#[derive(Clone)]
enum Screen {
    Home,
    Run(Entity<RunScreen>),
}

pub(crate) struct RootView {
    store: Entity<Store>,
    home: Entity<HomeScreen>,
    toasts: Entity<Toasts>,
    screen: Screen,
    sidebar_hidden: bool,
    /// From `RENO_APPEARANCE` or "Toggle theme"; `None` follows the system.
    appearance_override: Option<Appearance>,
    palette: Option<Entity<Palette>>,
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
        let subscriptions =
            vec![
                cx.observe(&store, |_, _, cx| cx.notify()),
                cx.observe(&toasts, |_, _, cx| cx.notify()),
                cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
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
                        StoreEvent::RunStarted(run) => this.open_run(run.clone(), window, cx),
                    }
                }),
            ];
        Self {
            store,
            home,
            toasts,
            screen: Screen::Home,
            sidebar_hidden: false,
            appearance_override,
            palette: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    /// The sidebar destination the screen belongs to.
    pub(crate) fn nav(&self) -> Nav {
        match self.screen {
            Screen::Home => Nav::Home,
            Screen::Run(_) => Nav::Runs,
        }
    }

    #[cfg(test)]
    pub(crate) fn run_screen(&self) -> Option<&Entity<RunScreen>> {
        match &self.screen {
            Screen::Run(run) => Some(run),
            Screen::Home => None,
        }
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

    pub(crate) fn appearance(&self, window: &Window) -> Appearance {
        self.appearance_override
            .unwrap_or_else(|| Appearance::from(window.appearance()))
    }

    /// Focuses the screen's text input: Home's composer or the run's steer input.
    pub(crate) fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = match &self.screen {
            Screen::Home => self.home.read(cx).composer().focus_handle(cx),
            Screen::Run(run) => run.read(cx).focus_handle(cx),
        };
        window.focus(&handle, cx);
    }

    /// Home is the only destination with a screen of its own (runs open from the sidebar or
    /// after a send); every other destination is a toast.
    pub(crate) fn navigate(&mut self, nav: Nav, window: &mut Window, cx: &mut Context<Self>) {
        if nav.is_built() {
            self.screen = Screen::Home;
            self.focus_composer(window, cx);
            cx.notify();
        } else {
            toast::not_yet(cx, nav.label());
        }
    }

    /// Shows one run and follows its transcript.
    pub(crate) fn open_run(&mut self, run: RunId, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Run(open) = &self.screen
            && open.read(cx).run_id() == &run
        {
            return;
        }
        let store = self.store.clone();
        let screen = cx.new(|cx| RunScreen::new(store, run, cx));
        self.screen = Screen::Run(screen);
        self.focus_composer(window, cx);
        cx.notify();
    }

    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_hidden = !self.sidebar_hidden;
        cx.notify();
    }

    pub(crate) fn toggle_theme(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.appearance_override = Some(self.appearance(window).toggled());
        cx.notify();
    }

    pub(crate) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            return;
        }
        let palette = cx.new(Palette::new);
        cx.subscribe_in(
            &palette,
            window,
            |this, _, event: &PaletteEvent, window, cx| {
                this.close_palette(window, cx);
                if let PaletteEvent::Run(action) = event {
                    this.run_palette_action(*action, window, cx);
                }
            },
        )
        .detach();
        let handle = palette.focus_handle(cx);
        window.focus(&handle, cx);
        self.palette = Some(palette);
        cx.notify();
    }

    pub(crate) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
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
        let hide = {
            let this = this.clone();
            Rc::new(move |_: &mut Window, cx: &mut App| {
                this.update(cx, |root, cx| root.toggle_sidebar(cx));
            })
        };
        SidebarHandlers {
            navigate,
            hide,
            recent_run: {
                let this = this.clone();
                Rc::new(move |run: RunId, window: &mut Window, cx: &mut App| {
                    this.update(cx, |root, cx| root.open_run(run, window, cx));
                })
            },
            settings: Rc::new(|_, cx| toast::not_yet(cx, "Settings")),
        }
    }

    fn titlebar_toggle(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        // Shown only while the sidebar is hidden: a band for the traffic lights and the toggle.
        let this = cx.entity();
        div()
            .h(px(sidebar::TITLEBAR))
            .flex_none()
            .flex()
            .items_center()
            .pl(px(84.0))
            .window_control_area(WindowControlArea::Drag)
            .child(
                kit::icon_button(theme, "show-sidebar", Icon::Sidebar, 28.0).on_click(
                    move |_, _, cx| {
                        this.update(cx, |root, cx| root.toggle_sidebar(cx));
                    },
                ),
            )
    }
}

impl Focusable for RootView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(self.appearance(window));
        theme::set_current(theme, cx);
        let handlers = self.handlers(cx);

        let panel = div()
            .id("canvas-panel")
            .flex_1()
            .min_h_0()
            .mb(px(8.0))
            .mr(px(8.0))
            .when(self.sidebar_hidden, |p| p.ml(px(8.0)))
            .when(!self.sidebar_hidden, |p| p.mt(px(8.0)))
            .rounded(px(14.0))
            .bg(theme.canvas)
            .border_1()
            .border_color(theme.hairline)
            .overflow_hidden()
            .child(match &self.screen {
                Screen::Home => self.home.clone().into_any_element(),
                Screen::Run(run) => run.clone().into_any_element(),
            });

        let main = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .when(self.sidebar_hidden, |m| {
                m.child(self.titlebar_toggle(&theme, cx))
            })
            .child(panel);

        div()
            .id("root")
            .key_context("Root")
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _: &OpenPalette, window, cx| this.open_palette(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(
                cx.listener(|this, _: &ToggleTheme, window, cx| this.toggle_theme(window, cx)),
            )
            .relative()
            .size_full()
            .flex()
            .flex_row()
            .bg(theme.shell)
            .text_color(theme.text)
            .font_family(SANS)
            .text_size(px(13.5))
            .when(!self.sidebar_hidden, |root| {
                root.child(sidebar::sidebar(
                    &theme,
                    self.store.read(cx),
                    self.nav(),
                    &handlers,
                ))
            })
            .child(main)
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
            ],
        );
        assert_eq!(
            o,
            Ok(Options {
                fake: true,
                appearance: Some(Appearance::Light),
                quit_after: Some(Duration::from_millis(250)),
                reduce_motion: true,
            })
        );
    }

    #[test]
    fn bad_input_is_ignored_or_rejected() {
        assert!(parse(&["--wat"], &[]).is_err());
        let o = parse(
            &[],
            &[("RENO_APPEARANCE", "plaid"), ("RENO_QUIT_AFTER_MS", "soon")],
        );
        assert_eq!(o, Ok(Options::default()));
    }
}
