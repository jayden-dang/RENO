//! The app's own preferences: what the desktop notifications do. Kept on this machine in
//! `app-settings.json` beside the daemon's socket (like `app-seen.json`), read at start and written
//! on every change. With `--fake` they live in memory only.
//!
//! The file is camelCase JSON. A missing or unreadable file means the defaults; a key this build
//! does not know is kept as it was when the file is written back.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// The four switches of Settings → Notifications. All default to on.
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
        prefs.rest = map
            .into_iter()
            .filter(|(key, _)| !Pref::ALL.iter().any(|p| p.key() == key))
            .collect();
        prefs
    }

    pub(crate) fn to_json(&self) -> Vec<u8> {
        let mut map = self.rest.clone();
        for pref in Pref::ALL {
            map.insert(pref.key().to_owned(), Value::Bool(self.get(pref)));
        }
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

    pub(crate) fn prefs(&self) -> &Prefs {
        &self.prefs
    }

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
    fn in_memory_prefs_touch_no_file() {
        let mut store = PrefStore::in_memory();
        store.set(Pref::Notifications, false);
        assert!(!store.prefs().notifications);
    }
}
