//! Panel state on disk (D8, option D): one small versioned JSON file of tab descriptors per
//! panel key, plus the window layout. Pure file I/O with an injected path; no GPUI, no daemon.
//!
//! Loading never fails the UI: a missing, unreadable, corrupt or newer-versioned file gives an
//! empty store and a [`Loaded`] note the caller may log. Unknown tab kinds are dropped.
//!
//! Saving is atomic (temp file in the same directory, then rename, mode 0600). Writes are
//! coalesced by [`Saver`], a pure deadline tracker: the GPUI layer calls [`Saver::changed`] on
//! each edit, arms ONE one-shot timer for the returned deadline, and calls
//! [`Saver::flush_if_due`] when it fires. There is no polling timer (ARCH-10), and the clock is
//! an `Instant` argument, so the logic is tested without sleeping.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::surface::{Descriptor, Kind};
use super::{CLOSED_CAP, Descriptors, PanelKey};

/// The file format this build reads and writes.
pub(crate) const VERSION: u32 = 1;
/// Panel keys kept; the least recently used go first.
pub(crate) const MAX_KEYS: usize = 100;
/// Tabs kept per key.
pub(crate) const MAX_TABS: usize = 20;
/// How long after the last change a write waits.
pub(crate) const SAVE_DELAY: Duration = Duration::from_millis(750);

/// `<Reno data dir>/app/panel-state.json`, the data dir being the one the daemon and CLI use
/// (`RENO_DATA_DIR` overrides). `None` when no home directory is known.
pub(crate) fn default_path() -> Option<PathBuf> {
    reno_client::default_data_dir().map(|dir| dir.join("app").join("panel-state.json"))
}

/// The global panel window layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct WindowState {
    pub(crate) open: bool,
    #[serde(default)]
    pub(crate) width: Option<f32>,
}

impl WindowState {
    fn sane(self) -> Self {
        Self {
            open: self.open,
            width: self.width.filter(|w| w.is_finite() && *w > 0.0),
        }
    }
}

/// What `load` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Loaded {
    /// The file was read.
    Read,
    /// There is no file yet.
    Missing,
    /// The file was unusable and the store starts empty. The reason is for a log line.
    Reset(String),
}

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    #[serde(default)]
    window: WindowState,
    #[serde(default)]
    keys: BTreeMap<PanelKey, Descriptors>,
    /// Most recent last.
    #[serde(default)]
    order: Vec<PanelKey>,
}

/// Descriptors per panel key, bounded (see [`MAX_KEYS`], [`MAX_TABS`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PanelStore {
    window: WindowState,
    keys: BTreeMap<PanelKey, Descriptors>,
    /// Most recently used last. Holds exactly the keys of `keys`.
    order: Vec<PanelKey>,
}

impl PanelStore {
    /// Read the store at `path`. Never errors: anything wrong gives an empty store.
    pub(crate) fn load(path: &Path) -> (Self, Loaded) {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return (Self::default(), Loaded::Missing);
            }
            Err(e) => return (Self::default(), Loaded::Reset(format!("unreadable: {e}"))),
        };
        let value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => return (Self::default(), Loaded::Reset(format!("corrupt: {e}"))),
        };
        let version = value.get("version").and_then(serde_json::Value::as_u64);
        if version != Some(u64::from(VERSION)) {
            return (
                Self::default(),
                Loaded::Reset(format!("unsupported version {version:?}")),
            );
        }
        match serde_json::from_value::<File>(value) {
            Ok(file) => (Self::from_file(file), Loaded::Read),
            Err(e) => (Self::default(), Loaded::Reset(format!("corrupt: {e}"))),
        }
    }

    fn from_file(file: File) -> Self {
        let mut store = Self {
            window: file.window.sane(),
            ..Self::default()
        };
        // Keys the order does not name are the oldest.
        let mut keys = file.keys;
        let mut order: Vec<PanelKey> = Vec::new();
        for key in file.order {
            if keys.contains_key(&key) && !order.contains(&key) {
                order.push(key);
            }
        }
        let unnamed: Vec<PanelKey> = keys
            .keys()
            .filter(|k| !order.contains(k))
            .cloned()
            .collect();
        for key in unnamed.into_iter().chain(order) {
            if let Some(saved) = keys.remove(&key) {
                store.put(key, saved);
            }
        }
        store
    }

    pub(crate) fn get(&self, key: &PanelKey) -> Option<&Descriptors> {
        self.keys.get(key)
    }

    /// Number of keys held.
    pub(crate) fn len(&self) -> usize {
        self.keys.len()
    }

    /// Keys from least to most recently used.
    pub(crate) fn order(&self) -> &[PanelKey] {
        &self.order
    }

    /// Store `saved` for `key` as the most recent, cleaned and capped. A panel with nothing
    /// left after cleaning is not stored.
    pub(crate) fn put(&mut self, key: PanelKey, saved: Descriptors) {
        self.order.retain(|k| *k != key);
        let saved = clean(saved);
        if saved.tabs.is_empty() && saved.active.is_none() && saved.closed.is_empty() {
            self.keys.remove(&key);
            return;
        }
        self.order.push(key.clone());
        self.keys.insert(key, saved);
        while self.order.len() > MAX_KEYS {
            let oldest = self.order.remove(0);
            self.keys.remove(&oldest);
        }
    }

    pub(crate) fn window(&self) -> WindowState {
        self.window
    }

    pub(crate) fn set_window(&mut self, window: WindowState) {
        self.window = window.sane();
    }

    /// Write the store to `path` atomically: a 0600 temp file in the same directory, flushed,
    /// then renamed over the target. On any failure the temp file is removed and the old target
    /// is untouched.
    pub(crate) fn save(&self, path: &Path) -> io::Result<()> {
        let file = File {
            version: VERSION,
            window: self.window,
            keys: self.keys.clone(),
            order: self.order.clone(),
        };
        let json = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(dir)?;
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "panel state path has no file name",
            )
        })?;
        let mut tmp_name = name.to_os_string();
        tmp_name.push(format!(".{}.tmp", std::process::id()));
        let tmp = dir.join(tmp_name);
        let written = write_file(&tmp, &json).and_then(|()| fs::rename(&tmp, path));
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written
    }
}

fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Drop descriptors this build cannot read, cap the closed stack and the tabs. When over
/// [`MAX_TABS`], keep Session first, then the active tab, then the most recently used. The saved
/// form has no use times, so later tabs count as more recent (a new tab is appended).
fn clean(mut saved: Descriptors) -> Descriptors {
    let readable = |d: &Descriptor| d.surface().is_some();
    saved.tabs.retain(readable);
    saved.closed.retain(readable);
    if saved.active.as_ref().is_some_and(|d| !readable(d)) {
        saved.active = None;
    }
    let excess = saved.closed.len().saturating_sub(CLOSED_CAP);
    saved.closed.drain(..excess);
    if saved.tabs.len() > MAX_TABS {
        let mut keep = vec![false; saved.tabs.len()];
        let mut left = MAX_TABS;
        let session = saved.tabs.iter().position(|d| d.kind == Kind::Session);
        let active = saved
            .tabs
            .iter()
            .position(|d| saved.active.as_ref() == Some(d));
        for i in session.into_iter().chain(active) {
            if !keep[i] {
                keep[i] = true;
                left -= 1;
            }
        }
        for flag in keep.iter_mut().rev() {
            if left == 0 {
                break;
            }
            if !*flag {
                *flag = true;
                left -= 1;
            }
        }
        let mut flags = keep.into_iter();
        saved.tabs.retain(|_| flags.next().unwrap_or(false));
    }
    saved
}

/// Coalesces writes: every change pushes one deadline `delay` past the last change, and a flush
/// writes once however many changes came before it. Pure state; the caller owns the timer.
#[derive(Clone, Debug)]
pub(crate) struct Saver {
    delay: Duration,
    deadline: Option<Instant>,
}

impl Default for Saver {
    fn default() -> Self {
        Self::new(SAVE_DELAY)
    }
}

impl Saver {
    pub(crate) fn new(delay: Duration) -> Self {
        Self {
            delay,
            deadline: None,
        }
    }

    /// Note a change at `now`. Returns the new deadline; the caller (re)arms its single timer.
    pub(crate) fn changed(&mut self, now: Instant) -> Instant {
        let deadline = now + self.delay;
        self.deadline = Some(deadline);
        deadline
    }

    /// Unsaved changes exist.
    pub(crate) fn dirty(&self) -> bool {
        self.deadline.is_some()
    }

    /// When the pending write is due, if any.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// A timer fired at `now`: write when the deadline has passed. A timer that a later change
    /// superseded finds the deadline in the future and does nothing. `None` means nothing was
    /// due. A failed write is reported, not retried by itself; the next change schedules a new
    /// one.
    pub(crate) fn flush_if_due(
        &mut self,
        now: Instant,
        store: &PanelStore,
        path: &Path,
    ) -> Option<io::Result<()>> {
        (self.deadline? <= now).then(|| self.flush(store, path))
    }

    /// Write now if dirty (quit, window close); a no-op `Ok` when clean.
    pub(crate) fn flush(&mut self, store: &PanelStore, path: &Path) -> io::Result<()> {
        if self.deadline.take().is_none() {
            return Ok(());
        }
        store.save(path)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::super::surface::Surface;
    use super::*;

    fn key(n: usize) -> PanelKey {
        PanelKey(format!("run_{n}"))
    }

    fn file(p: &str) -> Descriptor {
        Surface::File(p.into()).descriptor()
    }

    fn panel(files: &[&str]) -> Descriptors {
        let mut tabs = vec![Surface::Session.descriptor()];
        tabs.extend(files.iter().map(|f| file(f)));
        Descriptors {
            active: tabs.last().cloned(),
            tabs,
            closed: vec![],
        }
    }

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .expect("read_dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn round_trips_window_keys_and_order() {
        let d = dir();
        let path = d.path().join("app/panel-state.json");
        let mut s = PanelStore::default();
        s.set_window(WindowState {
            open: true,
            width: Some(420.5),
        });
        s.put(key(1), panel(&["a.rs"]));
        s.put(key(2), panel(&["b.rs", "c.rs"]));
        s.put(key(1), panel(&["a.rs", "z.rs"]));
        s.save(&path).expect("save");
        let (back, loaded) = PanelStore::load(&path);
        assert_eq!(loaded, Loaded::Read);
        assert_eq!(back, s);
        assert_eq!(back.order(), [key(2), key(1)]);
        assert_eq!(back.get(&key(1)), Some(&panel(&["a.rs", "z.rs"])));
    }

    #[test]
    fn file_shape_is_the_documented_one() {
        let d = dir();
        let path = d.path().join("p.json");
        let mut s = PanelStore::default();
        s.put(key(1), panel(&[]));
        s.save(&path).expect("save");
        let v: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read")).expect("json");
        assert_eq!(v["version"], 1);
        assert_eq!(v["window"]["open"], false);
        assert!(v["keys"]["run_1"]["tabs"].is_array());
        assert_eq!(v["order"], serde_json::json!(["run_1"]));
    }

    #[test]
    fn missing_file_is_an_empty_store() {
        let d = dir();
        let (s, loaded) = PanelStore::load(&d.path().join("nope.json"));
        assert_eq!(loaded, Loaded::Missing);
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn corrupt_and_unreadable_files_reset() {
        let d = dir();
        for (name, body) in [
            ("a", "{not json"),
            ("b", ""),
            ("c", "[1,2]"),
            ("d", "{\"version\":1,\"keys\":5}"),
        ] {
            let p = d.path().join(name);
            fs::write(&p, body).expect("write");
            let (s, loaded) = PanelStore::load(&p);
            assert!(matches!(loaded, Loaded::Reset(_)), "{name}: {loaded:?}");
            assert_eq!(s, PanelStore::default());
        }
        // A directory where the file should be cannot be read.
        let (s, loaded) = PanelStore::load(d.path());
        assert!(matches!(loaded, Loaded::Reset(_)));
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn unknown_or_missing_version_resets() {
        let d = dir();
        for (name, body) in [
            (
                "v2",
                r#"{"version":2,"keys":{"r":{"tabs":[]}},"order":["r"]}"#,
            ),
            ("v0", r#"{"version":0}"#),
            ("none", r#"{"keys":{}}"#),
            ("str", r#"{"version":"1"}"#),
        ] {
            let p = d.path().join(name);
            fs::write(&p, body).expect("write");
            let (s, loaded) = PanelStore::load(&p);
            assert!(matches!(loaded, Loaded::Reset(_)), "{name}");
            assert_eq!(s.len(), 0);
        }
    }

    #[test]
    fn unknown_kinds_are_dropped_on_load() {
        let d = dir();
        let p = d.path().join("p.json");
        fs::write(
            &p,
            r#"{"version":1,"window":{"open":true},"keys":{"r":{
                "tabs":[{"kind":"session"},{"kind":"browser","key":"u"},{"kind":"file","key":"a.rs"}],
                "active":{"kind":"browser","key":"u"},
                "closed":[{"kind":"hologram"},{"kind":"file","key":"b.rs"}]}},
                "order":["r"]}"#,
        )
        .expect("write");
        let (s, loaded) = PanelStore::load(&p);
        assert_eq!(loaded, Loaded::Read);
        let r = s.get(&PanelKey("r".into())).expect("key");
        assert_eq!(r.tabs, [Surface::Session.descriptor(), file("a.rs")]);
        assert_eq!(r.active, None);
        assert_eq!(r.closed, [file("b.rs")]);
        assert!(s.window().open);
    }

    #[test]
    fn key_cap_evicts_the_least_recently_used() {
        let mut s = PanelStore::default();
        for n in 0..=MAX_KEYS {
            s.put(key(n), panel(&[]));
        }
        assert_eq!(s.len(), MAX_KEYS);
        assert!(s.get(&key(0)).is_none());
        assert!(s.get(&key(1)).is_some());
        assert!(s.get(&key(MAX_KEYS)).is_some());
    }

    #[test]
    fn put_marks_most_recent_so_a_touched_key_survives_eviction() {
        let mut s = PanelStore::default();
        for n in 0..MAX_KEYS {
            s.put(key(n), panel(&[]));
        }
        s.put(key(0), panel(&["x.rs"]));
        s.put(key(1000), panel(&[]));
        assert!(s.get(&key(0)).is_some(), "touched key kept");
        assert!(s.get(&key(1)).is_none(), "oldest untouched key evicted");
        assert_eq!(s.order().last(), Some(&key(1000)));
        assert_eq!(s.order().len(), s.len());
    }

    #[test]
    fn tab_cap_keeps_session_active_then_the_most_recent() {
        let names: Vec<String> = (0..24).map(|i| format!("f{i}.rs")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut saved = panel(&refs); // 25 tabs
        saved.active = Some(file("f2.rs"));
        let mut s = PanelStore::default();
        s.put(key(1), saved);
        let got = s.get(&key(1)).expect("key");
        assert_eq!(got.tabs.len(), MAX_TABS);
        assert_eq!(got.tabs[0], Surface::Session.descriptor());
        assert!(got.tabs.contains(&file("f2.rs")), "active kept");
        assert!(got.tabs.contains(&file("f23.rs")), "newest kept");
        assert!(!got.tabs.contains(&file("f0.rs")), "oldest dropped");
        assert_eq!(got.active, Some(file("f2.rs")));
    }

    #[test]
    fn closed_stack_is_capped_and_load_enforces_caps_too() {
        let d = dir();
        let p = d.path().join("p.json");
        let tabs: Vec<Descriptor> = (0..25).map(|i| file(&format!("f{i}"))).collect();
        let body = serde_json::json!({
            "version": 1,
            "keys": {"r": {"tabs": tabs, "closed": tabs}},
            "order": ["r"],
        });
        fs::write(&p, body.to_string()).expect("write");
        let (s, _) = PanelStore::load(&p);
        let r = s.get(&PanelKey("r".into())).expect("key");
        assert_eq!(r.tabs.len(), MAX_TABS);
        assert_eq!(r.closed.len(), CLOSED_CAP);
        assert_eq!(r.closed.last(), Some(&file("f24")));
    }

    #[test]
    fn load_reconciles_order_with_keys() {
        let d = dir();
        let p = d.path().join("p.json");
        let t = serde_json::json!({"tabs": [{"kind": "session"}]});
        let body = serde_json::json!({
            "version": 1,
            "keys": {"a": t, "b": t, "c": t},
            "order": ["ghost", "c", "c", "a"],
        });
        fs::write(&p, body.to_string()).expect("write");
        let (s, _) = PanelStore::load(&p);
        let order: Vec<&str> = s.order().iter().map(PanelKey::as_str).collect();
        assert_eq!(order, ["b", "c", "a"], "unlisted is oldest; ghosts go");
    }

    #[test]
    fn an_empty_panel_is_not_stored() {
        let mut s = PanelStore::default();
        s.put(key(1), panel(&[]));
        s.put(key(1), Descriptors::default());
        assert!(s.get(&key(1)).is_none());
        assert!(s.order().is_empty());
    }

    #[test]
    fn bad_window_width_is_dropped() {
        let mut s = PanelStore::default();
        s.set_window(WindowState {
            open: true,
            width: Some(f32::NAN),
        });
        assert_eq!(s.window().width, None);
        s.set_window(WindowState {
            open: true,
            width: Some(-3.0),
        });
        assert_eq!(s.window().width, None);
    }

    #[test]
    fn saved_file_is_0600_and_creates_the_directory() {
        let d = dir();
        let path = d.path().join("deep/app/panel-state.json");
        PanelStore::default().save(&path).expect("save");
        let mode = fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn save_replaces_and_leaves_no_temp_file() {
        let d = dir();
        let path = d.path().join("p.json");
        let mut s = PanelStore::default();
        s.put(key(1), panel(&[]));
        s.save(&path).expect("save");
        s.put(key(2), panel(&[]));
        s.save(&path).expect("save again");
        assert_eq!(leftovers(d.path()), ["p.json"]);
        assert_eq!(PanelStore::load(&path).0.len(), 2);
    }

    #[test]
    fn read_only_directory_fails_without_a_partial_file() {
        let d = dir();
        let path = d.path().join("p.json");
        let mut s = PanelStore::default();
        s.put(key(1), panel(&[]));
        s.save(&path).expect("save");
        let before = fs::read(&path).expect("read");
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o555)).expect("chmod");
        s.put(key(2), panel(&[]));
        let result = s.save(&path);
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o755)).expect("chmod back");
        assert!(result.is_err());
        assert_eq!(fs::read(&path).expect("read"), before, "old file intact");
        assert_eq!(leftovers(d.path()), ["p.json"], "no temp file");
    }

    #[test]
    fn failed_rename_removes_the_temp_file() {
        let d = dir();
        let path = d.path().join("p.json");
        fs::create_dir(&path).expect("mkdir"); // a file cannot be renamed onto a directory
        let result = PanelStore::default().save(&path);
        assert!(result.is_err());
        assert_eq!(leftovers(d.path()), ["p.json"]);
    }

    #[test]
    fn default_path_is_under_the_app_dir() {
        if let Some(p) = default_path() {
            assert!(p.ends_with("app/panel-state.json"), "{p:?}");
        }
    }

    #[test]
    fn saver_is_clean_until_a_change() {
        let mut saver = Saver::new(Duration::from_secs(1));
        assert!(!saver.dirty());
        assert_eq!(saver.deadline(), None);
        let d = dir();
        let path = d.path().join("p.json");
        let none = saver.flush_if_due(Instant::now(), &PanelStore::default(), &path);
        assert!(none.is_none());
        assert!(saver.flush(&PanelStore::default(), &path).is_ok());
        assert!(!path.exists(), "clean flush writes nothing");
    }

    #[test]
    fn saver_coalesces_changes_into_one_write_after_the_last() {
        let d = dir();
        let path = d.path().join("p.json");
        let t0 = Instant::now();
        let delay = Duration::from_secs(1);
        let mut saver = Saver::new(delay);
        let mut s = PanelStore::default();
        s.put(key(1), panel(&[]));
        saver.changed(t0);
        let second = saver.changed(t0 + Duration::from_millis(600));
        assert_eq!(second, t0 + Duration::from_millis(1600));
        // The first change's timer fires: superseded, nothing is written.
        assert!(saver.flush_if_due(t0 + delay, &s, &path).is_none());
        assert!(!path.exists());
        assert!(saver.dirty());
        // The deadline after the last change: exactly one write.
        let r = saver.flush_if_due(second, &s, &path);
        assert!(matches!(r, Some(Ok(()))));
        assert!(path.exists());
        assert!(!saver.dirty());
        assert!(saver.flush_if_due(second + delay, &s, &path).is_none());
    }

    #[test]
    fn saver_flush_writes_early_and_clears() {
        let d = dir();
        let path = d.path().join("p.json");
        let mut saver = Saver::default();
        saver.changed(Instant::now());
        assert!(saver.flush(&PanelStore::default(), &path).is_ok());
        assert!(path.exists());
        assert!(!saver.dirty());
    }

    #[test]
    fn saver_reports_a_failed_write_and_goes_clean() {
        let d = dir();
        let path = d.path().join("p.json");
        fs::create_dir(&path).expect("mkdir");
        let t0 = Instant::now();
        let mut saver = Saver::new(Duration::ZERO);
        saver.changed(t0);
        let r = saver.flush_if_due(t0, &PanelStore::default(), &path);
        assert!(matches!(r, Some(Err(_))));
        assert!(!saver.dirty(), "the next change schedules a retry");
    }
}
