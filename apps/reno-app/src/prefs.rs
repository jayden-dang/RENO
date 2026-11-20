//! The app's own preferences: what the desktop notifications do, how Reno looks (Settings ›
//! Appearance) and how the composer and the Files tab behave (Settings › General). Kept on this machine in
//! `app-settings.json` beside the daemon's socket (like `app-seen.json`), read at start and written
//! on every change. With `--fake` they live in memory only.
//!
//! The file is camelCase JSON. A missing or unreadable file means the defaults; a key this build
//! does not know is kept as it was when the file is written back.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// System, Light or Dark: the one source of the app's appearance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    #[default]
    System,
    Light,
    Dark,
}

/// A curated theme (its neutrals).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ThemeName {
    /// Vercel's Geist palette: neutral greys on black or white. The default.
    #[default]
    Vercel,
    Reno,
    Slate,
}

/// The accent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum AccentName {
    Teal,
    #[default]
    Blue,
    Violet,
}

/// A choice written as one word in the file.
trait Word: Copy + Default + PartialEq + Sized + 'static {
    const ALL: &'static [(Self, &'static str)];

    fn word(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(v, _)| *v == self)
            .map_or("", |(_, w)| w)
    }

    fn from_word(word: Option<&str>) -> Self {
        Self::ALL
            .iter()
            .find(|(_, w)| Some(*w) == word)
            .map_or_else(Self::default, |(v, _)| *v)
    }
}

impl Word for Mode {
    const ALL: &'static [(Self, &'static str)] = &[
        (Self::System, "system"),
        (Self::Light, "light"),
        (Self::Dark, "dark"),
    ];
}

impl Word for ThemeName {
    const ALL: &'static [(Self, &'static str)] = &[
        (Self::Vercel, "vercel"),
        (Self::Reno, "reno"),
        (Self::Slate, "slate"),
    ];
}

impl Word for AccentName {
    const ALL: &'static [(Self, &'static str)] = &[
        (Self::Teal, "teal"),
        (Self::Blue, "blue"),
        (Self::Violet, "violet"),
    ];
}

/// Reduce motion: follow macOS, or pinned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Motion {
    #[default]
    System,
    On,
    Off,
}

impl Word for Motion {
    const ALL: &'static [(Self, &'static str)] = &[
        (Self::System, "system"),
        (Self::On, "on"),
        (Self::Off, "off"),
    ];
}

/// What sends a message from the composer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SendWith {
    /// Enter sends; ⇧ Enter starts a new line.
    #[default]
    Enter,
    /// ⌘ Enter sends; Enter starts a new line.
    CommandEnter,
}

impl Word for SendWith {
    const ALL: &'static [(Self, &'static str)] =
        &[(Self::Enter, "enter"), (Self::CommandEnter, "cmdEnter")];
}

/// The keys of the file this build reads besides the notification switches.
const KEYS: [&str; 8] = [
    "mode",
    "theme",
    "accent",
    "reduceMotion",
    "sendWith",
    "escapeStops",
    "hiddenFiles",
    "sidebarWidth",
];

/// The switches of Settings → Notifications (all default to on), and the choices of Settings ›
/// Appearance and General (today's look and behaviour by default).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Prefs {
    /// Banners at all.
    pub(crate) notifications: bool,
    /// Hold banners while a Reno window is the active window.
    pub(crate) background_only: bool,
    /// Also banner runs that finished (held actions, questions and problems always ring).
    pub(crate) finished: bool,
    /// The Dock badge.
    pub(crate) dock_badge: bool,
    pub(crate) mode: Mode,
    pub(crate) theme: ThemeName,
    pub(crate) accent: AccentName,
    pub(crate) reduce_motion: Motion,
    pub(crate) send_with: SendWith,
    /// Escape in a running thread stops it.
    pub(crate) escape_stops: bool,
    /// The Files tab lists dotfiles.
    pub(crate) hidden_files: bool,
    /// How wide the sidebar was dragged to (ROAD-32): its default until it is.
    pub(crate) sidebar_width: f32,
    /// Keys of the file this build does not know, written back unchanged.
    rest: Map<String, Value>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            notifications: true,
            background_only: true,
            finished: true,
            dock_badge: true,
            mode: Mode::System,
            theme: ThemeName::Vercel,
            accent: AccentName::Blue,
            reduce_motion: Motion::System,
            send_with: SendWith::Enter,
            escape_stops: false,
            hidden_files: true,
            sidebar_width: crate::ui::sidebar::WIDTH,
            rest: Map::new(),
        }
    }
}

/// One of the switches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pref {
    Notifications,
    BackgroundOnly,
    Finished,
    DockBadge,
}

impl Pref {
    pub(crate) const ALL: [Self; 4] = [
        Self::Notifications,
        Self::BackgroundOnly,
        Self::Finished,
        Self::DockBadge,
    ];

    /// The key in the file.
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Notifications => "notifications",
            Self::BackgroundOnly => "backgroundOnly",
            Self::Finished => "finished",
            Self::DockBadge => "dockBadge",
        }
    }
}

impl Prefs {
    pub(crate) fn get(&self, pref: Pref) -> bool {
        match pref {
            Pref::Notifications => self.notifications,
            Pref::BackgroundOnly => self.background_only,
            Pref::Finished => self.finished,
            Pref::DockBadge => self.dock_badge,
        }
    }

    pub(crate) fn set(&mut self, pref: Pref, on: bool) {
        match pref {
            Pref::Notifications => self.notifications = on,
            Pref::BackgroundOnly => self.background_only = on,
            Pref::Finished => self.finished = on,
            Pref::DockBadge => self.dock_badge = on,
        }
    }

    /// Reads the file's JSON. Anything that is not an object, and any switch that is not a
    /// boolean, is the default.
    pub(crate) fn from_json(bytes: &[u8]) -> Self {
        let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(bytes) else {
            return Self::default();
        };
        let mut prefs = Self::default();
        for pref in Pref::ALL {
            if let Some(on) = map.get(pref.key()).and_then(Value::as_bool) {
                prefs.set(pref, on);
            }
        }
        let word = |key: &str| map.get(key).and_then(Value::as_str);
        let flag =
            |key: &str, default: bool| map.get(key).and_then(Value::as_bool).unwrap_or(default);
        prefs.mode = Mode::from_word(word("mode"));
        prefs.theme = ThemeName::from_word(word("theme"));
        prefs.accent = AccentName::from_word(word("accent"));
        prefs.reduce_motion = Motion::from_word(word("reduceMotion"));
        prefs.send_with = SendWith::from_word(word("sendWith"));
        prefs.escape_stops = flag("escapeStops", prefs.escape_stops);
        prefs.hidden_files = flag("hiddenFiles", prefs.hidden_files);
        prefs.sidebar_width = map
            .get("sidebarWidth")
            .and_then(Value::as_f64)
            .map_or(prefs.sidebar_width, |w| {
                (w as f32).clamp(crate::ui::sidebar::MIN_WIDTH, crate::ui::sidebar::MAX_WIDTH)
            });
        prefs.rest = map
            .into_iter()
            .filter(|(key, _)| {
                !Pref::ALL.iter().any(|p| p.key() == key) && !KEYS.contains(&key.as_str())
            })
            .collect();
        prefs
    }

    pub(crate) fn to_json(&self) -> Vec<u8> {
        let mut map = self.rest.clone();
        for pref in Pref::ALL {
            map.insert(pref.key().to_owned(), Value::Bool(self.get(pref)));
        }
        let mut put = |key: &str, value: Value| {
            map.insert(key.to_owned(), value);
        };
        put("mode", self.mode.word().into());
        put("theme", self.theme.word().into());
        put("accent", self.accent.word().into());
        put("reduceMotion", self.reduce_motion.word().into());
        put("sendWith", self.send_with.word().into());
        put("escapeStops", self.escape_stops.into());
        put("hiddenFiles", self.hidden_files.into());
        put("sidebarWidth", f64::from(self.sidebar_width).into());
        serde_json::to_vec_pretty(&Value::Object(map)).unwrap_or_default()
    }
}

/// Where the preferences are kept: beside the daemon's socket, in their own file.
pub(crate) fn settings_path(socket: &Path) -> Option<PathBuf> {
    socket.parent().map(|dir| dir.join("app-settings.json"))
}

/// The preferences and the file they are written to (`None`: memory only).
#[derive(Debug, Default)]
pub(crate) struct PrefStore {
    prefs: Prefs,
    path: Option<PathBuf>,
}

impl PrefStore {
    /// Defaults, never written anywhere (`--fake`, tests).
    pub(crate) fn in_memory() -> Self {
        Self::default()
    }

    /// Reads `path`; a missing or unreadable file is the defaults. Every later change is written
    /// back to it.
    pub(crate) fn at(path: PathBuf) -> Self {
        let prefs = std::fs::read(&path)
            .map(|bytes| Prefs::from_json(&bytes))
            .unwrap_or_default();
        Self {
            prefs,
            path: Some(path),
        }
    }

    #[cfg(test)]
    pub(crate) fn prefs(&self) -> &Prefs {
        &self.prefs
    }

    /// Applies `change`; writes the file when something changed.
    pub(crate) fn update(&mut self, change: impl FnOnce(&mut Prefs)) {
        let before = self.prefs.clone();
        change(&mut self.prefs);
        if self.prefs != before
            && let Some(path) = &self.path
        {
            save(path, &self.prefs);
        }
    }

    #[cfg(test)]
    pub(crate) fn set(&mut self, pref: Pref, on: bool) {
        if self.prefs.get(pref) == on {
            return;
        }
        self.prefs.set(pref, on);
        if let Some(path) = &self.path {
            save(path, &self.prefs);
        }
    }
}

impl gpui::Global for PrefStore {}

/// The preferences in force: the installed store's, else the defaults (a test window).
pub(crate) fn current(cx: &gpui::App) -> Prefs {
    cx.try_global::<PrefStore>()
        .map_or_else(Prefs::default, |store| store.prefs.clone())
}

/// Changes the preferences in force and redraws; installs an in-memory store when none is.
pub(crate) fn update(cx: &mut gpui::App, change: impl FnOnce(&mut Prefs)) {
    cx.default_global::<PrefStore>().update(change);
    cx.refresh_windows();
}

/// Writes through a temporary file, so a crash never leaves half a file. A failure only costs the
/// change its memory; it is logged, not shown.
#[allow(clippy::print_stderr)]
fn save(path: &Path, prefs: &Prefs) {
    let write = || -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, prefs.to_json())?;
        std::fs::rename(&tmp, path)
    };
    if let Err(error) = write() {
        eprintln!("reno-app: could not save {}: {error}", path.display());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_broken_file_is_the_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("app-settings.json");
        assert_eq!(*PrefStore::at(path.clone()).prefs(), Prefs::default());
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(*PrefStore::at(path.clone()).prefs(), Prefs::default());
        std::fs::write(&path, b"[1,2]").unwrap();
        assert_eq!(*PrefStore::at(path).prefs(), Prefs::default());
        let all_on = Prefs::default();
        assert!(
            all_on.notifications && all_on.background_only && all_on.finished && all_on.dock_badge
        );
    }

    #[test]
    fn a_change_is_written_and_read_back_and_unknown_keys_survive() {
        let tmp = tempfile::tempdir().unwrap();
        let path = settings_path(&tmp.path().join("reno.sock")).unwrap();
        std::fs::write(
            &path,
            br#"{"finished": false, "someFutureKey": {"a": 1}, "dockBadge": "yes"}"#,
        )
        .unwrap();
        let mut store = PrefStore::at(path.clone());
        assert!(!store.prefs().finished);
        assert!(
            store.prefs().dock_badge,
            "a value that is not a boolean is the default"
        );

        store.set(Pref::BackgroundOnly, false);
        let again = PrefStore::at(path.clone());
        assert!(!again.prefs().background_only);
        assert!(!again.prefs().finished);
        let json: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["someFutureKey"]["a"], 1);
        assert_eq!(json["backgroundOnly"], false);
        assert_eq!(json["notifications"], true, "the file is camelCase");
    }

    #[test]
    fn the_new_choices_default_to_todays_look_and_behaviour() {
        let p = Prefs::default();
        assert_eq!(p.mode, Mode::System);
        assert_eq!(p.theme, ThemeName::Vercel, "the default theme is Vercel's");
        assert_eq!(p.accent, AccentName::Blue, "with its blue");
        assert_eq!(p.reduce_motion, Motion::System);
        assert_eq!(p.send_with, SendWith::Enter);
        assert!(!p.escape_stops, "Escape does not stop a run today");
        assert!(p.hidden_files, "dotfiles are listed today");
    }

    #[test]
    fn the_sidebar_width_defaults_to_the_old_width_and_is_kept_within_bounds() {
        assert!((Prefs::default().sidebar_width - 248.0).abs() < 0.01);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("app-settings.json");
        let mut store = PrefStore::at(path.clone());
        store.update(|p| p.sidebar_width = 300.0);
        assert!((PrefStore::at(path.clone()).prefs().sidebar_width - 300.0).abs() < 0.01);
        std::fs::write(&path, br#"{"sidebarWidth": 9000}"#).unwrap();
        assert!((PrefStore::at(path.clone()).prefs().sidebar_width - 380.0).abs() < 0.01);
        std::fs::write(&path, br#"{"sidebarWidth": "wide"}"#).unwrap();
        assert!((PrefStore::at(path).prefs().sidebar_width - 248.0).abs() < 0.01);
    }

    #[test]
    fn the_new_choices_are_written_and_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("app-settings.json");
        let mut store = PrefStore::at(path.clone());
        store.update(|p| {
            p.mode = Mode::Dark;
            p.theme = ThemeName::Slate;
            p.accent = AccentName::Violet;
            p.reduce_motion = Motion::On;
            p.send_with = SendWith::CommandEnter;
            p.escape_stops = true;
            p.hidden_files = false;
        });
        let json: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["mode"], "dark");
        assert_eq!(json["theme"], "slate");
        assert_eq!(json["accent"], "violet");
        assert_eq!(json["reduceMotion"], "on");
        assert_eq!(json["sendWith"], "cmdEnter");
        assert_eq!(json["escapeStops"], true);
        let again = PrefStore::at(path).prefs().clone();
        assert_eq!(again.mode, Mode::Dark);
        assert_eq!(again.theme, ThemeName::Slate);
        assert_eq!(again.accent, AccentName::Violet);
        assert_eq!(again.reduce_motion, Motion::On);
        assert_eq!(again.send_with, SendWith::CommandEnter);
        assert!(again.escape_stops && !again.hidden_files);
    }

    #[test]
    fn an_unknown_value_is_the_default_and_kept_out_of_the_rest() {
        let p = Prefs::from_json(br#"{"theme": "graphite", "accent": 3, "mode": "light"}"#);
        assert_eq!(p.theme, ThemeName::Vercel);
        assert_eq!(p.accent, AccentName::Blue);
        assert_eq!(p.mode, Mode::Light);
        let json: Value = serde_json::from_slice(&p.to_json()).unwrap();
        assert_eq!(
            json["theme"], "vercel",
            "a known key is rewritten, not carried as unknown"
        );
        let saved = Prefs::from_json(br#"{"theme": "reno", "accent": "teal"}"#);
        assert_eq!(
            (saved.theme, saved.accent),
            (ThemeName::Reno, AccentName::Teal),
            "a saved choice stays"
        );
    }

    #[gpui::test]
    fn the_app_reads_one_store_and_a_change_reaches_it(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            assert_eq!(current(cx), Prefs::default(), "no store yet: the defaults");
            update(cx, |p| p.theme = ThemeName::Slate);
            assert_eq!(current(cx).theme, ThemeName::Slate);
        });
    }

    #[test]
    fn in_memory_prefs_touch_no_file() {
        let mut store = PrefStore::in_memory();
        store.set(Pref::Notifications, false);
        assert!(!store.prefs().notifications);
    }
}
