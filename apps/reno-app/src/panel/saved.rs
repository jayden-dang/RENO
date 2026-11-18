//! The panel store in the running app (D8): one [`PanelStore`] and one [`Saver`] as a GPUI
//! global, loaded once at startup from an injected path.
//!
//! Every change pushes the saver's deadline out and re-arms ONE one-shot timer (the old timer is
//! dropped, which cancels it); when it fires the saver writes if the deadline has passed. There
//! is no polling timer (ARCH-10). Quit flushes what is pending. With no global installed (the
//! path is unknown, or a test that does not care) every call here does nothing, so a test never
//! reaches the real data dir.

use std::path::PathBuf;
use std::time::Instant;

use gpui::{App, Global, Task};

use super::persist::{Loaded, PanelStore, SAVE_DELAY, Saver, WindowState};
use super::view::Prefs;
use super::{Descriptors, PanelKey};

pub(crate) struct Saved {
    store: PanelStore,
    path: PathBuf,
    saver: Saver,
    /// The one pending timer; replacing it cancels the old one.
    timer: Option<Task<()>>,
    /// Writes that reached the disk (tests count them).
    #[cfg(test)]
    writes: usize,
}

impl Global for Saved {}

impl Saved {
    /// Loads the store at `path` and makes it the app's. The window's `Prefs` start from what
    /// was saved, and a quit writes what is pending. A bad file starts empty and says so once.
    pub(crate) fn install(path: PathBuf, cx: &mut App) {
        let (store, loaded) = PanelStore::load(&path);
        if let Loaded::Reset(reason) = &loaded {
            note(&format!(
                "panel state at {} reset: {reason}",
                path.display()
            ));
        }
        let window = store.window();
        Prefs {
            open: window.open,
            width: window.width,
        }
        .set_without_saving(cx);
        cx.set_global(Self {
            store,
            path,
            saver: Saver::default(),
            timer: None,
            #[cfg(test)]
            writes: 0,
        });
        cx.on_app_quit(|cx| {
            Self::flush(cx);
            std::future::ready(())
        })
        .detach();
    }

    /// What was saved for `key`.
    pub(crate) fn get(key: &PanelKey, cx: &App) -> Option<Descriptors> {
        cx.try_global::<Self>()?.store.get(key).cloned()
    }

    /// Remembers `panel` for `key` and schedules the write.
    pub(crate) fn put(key: PanelKey, panel: Descriptors, cx: &mut App) {
        Self::edit(cx, |store| store.put(key, panel));
    }

    /// Remembers the window's layout and schedules the write.
    pub(crate) fn set_window(window: WindowState, cx: &mut App) {
        Self::edit(cx, |store| store.set_window(window));
    }

    /// Writes now when something is pending (quit, window close).
    pub(crate) fn flush(cx: &mut App) {
        if cx.has_global::<Self>() {
            let saved = cx.global_mut::<Self>();
            saved.timer = None;
            let result = saved.saver.flush(&saved.store, &saved.path);
            saved.written(result);
        }
    }

    fn edit(cx: &mut App, change: impl FnOnce(&mut PanelStore)) {
        if !cx.has_global::<Self>() {
            return;
        }
        let saved = cx.global_mut::<Self>();
        change(&mut saved.store);
        let deadline = saved.saver.changed(Instant::now());
        // The one timer: assigning it drops (and so cancels) the one before.
        let timer = cx.spawn(async move |cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            cx.update(|cx| Self::fire(deadline, cx));
        });
        cx.global_mut::<Self>().timer = Some(timer);
    }

    /// The timer armed for `deadline` fired. A later change moved the deadline on, and its own
    /// timer is the one that writes.
    fn fire(deadline: Instant, cx: &mut App) {
        if cx.has_global::<Self>() {
            let saved = cx.global_mut::<Self>();
            if let Some(result) = saved
                .saver
                .flush_if_due(deadline, &saved.store, &saved.path)
            {
                saved.written(result);
            }
        }
    }

    fn written(&mut self, result: std::io::Result<()>) {
        match result {
            Ok(()) => {
                #[cfg(test)]
                {
                    self.writes += 1;
                }
            }
            Err(error) => note(&format!("could not save {}: {error}", self.path.display())),
        }
    }

    #[cfg(test)]
    pub(crate) fn writes(cx: &App) -> usize {
        cx.try_global::<Self>().map_or(0, |s| s.writes)
    }
}

#[allow(clippy::print_stderr)]
fn note(message: &str) {
    eprintln!("reno-app: {message}");
}
