//! The saved panel (D8) on the run screen: tabs and layout come back, writes wait for one
//! deadline, a quit writes at once. Time is the test clock; the files are in a temp dir, and
//! without `Saved::install` nothing here (or in any other test) reaches the real data dir.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Duration;

use gpui::{Entity, TestAppContext, VisualTestContext};

use super::RunScreen;
use crate::app::RootView;
use crate::panel::PanelKey;
use crate::panel::persist::{Loaded, PanelStore, SAVE_DELAY};
use crate::panel::saved::Saved;
use crate::panel::surface::Surface;
use crate::panel::view::{Cmd, Prefs};
use crate::tests::{draw, fake_store, painted, run_id, wait_drawn, wait_until, window};
use crate::ui::theme::Appearance;

struct Fixture<'a> {
    dir: tempfile::TempDir,
    root: Entity<RootView>,
    vcx: &'a mut VisualTestContext,
}

fn path(dir: &tempfile::TempDir) -> PathBuf {
    crate::panel::persist::path_in(dir.path())
}

/// A window on the fake daemon with the panel store installed over a temp dir; `before` can put
/// a file there first.
fn fixture(cx: &mut TestAppContext, before: impl FnOnce(&PathBuf)) -> Fixture<'_> {
    let dir = tempfile::tempdir().unwrap();
    before(&path(&dir));
    let at = path(&dir);
    cx.update(|cx| Saved::install(at, cx));
    let store = fake_store(cx);
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.state().approvals().len() == 2)
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    Fixture { dir, root, vcx }
}

fn open(f: &mut Fixture<'_>, run: &str) -> Entity<RunScreen> {
    let id = run_id(run);
    f.root
        .update_in(f.vcx, |r, window, cx| r.open_run(id, window, cx));
    draw(f.vcx);
    f.root
        .read_with(f.vcx, |r, _| r.run_screen().cloned())
        .expect("a run screen")
}

fn tabs(f: &Fixture<'_>, screen: &Entity<RunScreen>) -> Vec<Surface> {
    screen.read_with(f.vcx, |s, _| s.panel.tabs().to_vec())
}

fn active(f: &Fixture<'_>, screen: &Entity<RunScreen>) -> Surface {
    screen.read_with(f.vcx, |s, _| s.panel.active().clone())
}

fn writes(f: &mut Fixture<'_>) -> usize {
    f.vcx.update(|_, cx| Saved::writes(cx))
}

fn advance(f: &mut Fixture<'_>, by: Duration) {
    f.vcx.executor().advance_clock(by);
    f.vcx.run_until_parked();
}

/// builder1 changed files, so it opens with Review; then Home is opened and Review closed.
fn work_on_builder(f: &mut Fixture<'_>) -> Entity<RunScreen> {
    let screen = open(f, "builder1");
    wait_drawn(f.vcx, |vcx| painted(vcx, "panel-tab-review"));
    screen.update_in(f.vcx, |s, window, cx| {
        s.open_surface(Surface::Home, window, cx);
        s.panel_cmd(Cmd::Close(Surface::Review), window, cx);
    });
    draw(f.vcx);
    screen
}

#[gpui::test]
fn the_same_run_comes_back_with_its_tabs_and_a_different_run_has_its_own(cx: &mut TestAppContext) {
    let mut f = fixture(cx, |_| {});
    let screen = work_on_builder(&mut f);
    assert_eq!(tabs(&f, &screen), [Surface::Session, Surface::Home]);
    assert_eq!(active(&f, &screen), Surface::Home);

    // Another run has its own tabs, not the first one's.
    let other = open(&mut f, "keeper1");
    wait_drawn(f.vcx, |vcx| painted(vcx, "panel-tab-session"));
    assert!(!tabs(&f, &other).contains(&Surface::Home));
    assert_ne!(active(&f, &other), Surface::Home);

    // Back to the first: a new screen, the same tabs and the same one in front.
    let again = open(&mut f, "builder1");
    assert_ne!(again.entity_id(), screen.entity_id());
    assert_eq!(tabs(&f, &again), [Surface::Session, Surface::Home]);
    assert_eq!(active(&f, &again), Surface::Home);
    // Review was closed, and the closed stack came back too.
    again.update_in(f.vcx, |s, window, cx| {
        s.panel_action(crate::screens::run::PanelAction::ReopenTab, window, cx);
    });
    assert_eq!(active(&f, &again), Surface::Review);
}

#[gpui::test]
fn the_tabs_survive_a_restart_through_the_file(cx: &mut TestAppContext) {
    let mut f = fixture(cx, |_| {});
    work_on_builder(&mut f);
    advance(&mut f, SAVE_DELAY);
    let (store, _) = PanelStore::load(&path(&f.dir));
    let saved = store
        .get(&PanelKey(run_id("builder1").to_string()))
        .unwrap();
    let restored = crate::panel::PanelState::restore(saved);
    assert_eq!(restored.tabs(), [Surface::Session, Surface::Home]);
    assert_eq!(*restored.active(), Surface::Home);
}

#[gpui::test]
fn a_corrupt_file_gives_defaults_and_no_crash(cx: &mut TestAppContext) {
    let mut f = fixture(cx, |path| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"{ not json at all").unwrap();
    });
    let screen = open(&mut f, "builder1");
    wait_drawn(f.vcx, |vcx| painted(vcx, "panel-tab-review"));
    // The defaults a run with changes gets.
    assert_eq!(tabs(&f, &screen), [Surface::Session, Surface::Review]);
    assert_eq!(active(&f, &screen), Surface::Review);
    // And the next change still saves over the bad file.
    screen.update_in(f.vcx, |s, window, cx| {
        s.panel_cmd(Cmd::Close(Surface::Review), window, cx);
    });
    advance(&mut f, SAVE_DELAY);
    assert_eq!(PanelStore::load(&path(&f.dir)).1, Loaded::Read);
}

#[gpui::test]
fn a_burst_of_changes_writes_once_after_the_delay(cx: &mut TestAppContext) {
    let mut f = fixture(cx, |_| {});
    let screen = open(&mut f, "builder1");
    wait_drawn(f.vcx, |vcx| painted(vcx, "panel-tab-review"));
    assert_eq!(writes(&mut f), 0, "opening a run writes nothing");
    let step = Duration::from_millis(100);
    for i in 0..5 {
        screen.update_in(f.vcx, |s, window, cx| {
            let tab = if i % 2 == 0 {
                Surface::Session
            } else {
                Surface::Review
            };
            s.panel_cmd(Cmd::Activate(tab), window, cx);
        });
        advance(&mut f, step);
    }
    assert_eq!(
        writes(&mut f),
        0,
        "nothing is written while changes keep coming"
    );
    assert!(!path(&f.dir).exists());
    advance(&mut f, SAVE_DELAY - step - Duration::from_millis(1));
    assert_eq!(
        writes(&mut f),
        0,
        "not before the delay after the last change"
    );
    advance(&mut f, Duration::from_millis(2));
    assert_eq!(writes(&mut f), 1, "one write for the whole burst");
    assert!(path(&f.dir).exists());
    advance(&mut f, SAVE_DELAY * 4);
    assert_eq!(writes(&mut f), 1, "and no timer is left running");
}

#[gpui::test]
fn quitting_writes_what_is_pending(cx: &mut TestAppContext) {
    let app = cx.clone();
    let mut f = fixture(cx, |_| {});
    work_on_builder(&mut f);
    assert_eq!(writes(&mut f), 0);
    assert!(!path(&f.dir).exists());
    // What the platform runs on quit: the app's quit handlers.
    app.update(|cx| cx.shutdown());
    let (store, loaded) = PanelStore::load(&path(&f.dir));
    assert_eq!(loaded, Loaded::Read);
    assert!(
        store
            .get(&PanelKey(run_id("builder1").to_string()))
            .is_some()
    );
}

#[gpui::test]
fn the_panel_width_and_visibility_persist(cx: &mut TestAppContext) {
    let mut f = fixture(cx, |_| {});
    let screen = open(&mut f, "builder1");
    wait_drawn(f.vcx, |vcx| painted(vcx, "panel-canvas"));
    screen.update_in(f.vcx, |s, window, cx| {
        s.toggle_panel(window, cx);
    });
    f.vcx.update(|_, cx| {
        Prefs {
            width: Some(512.0),
            ..Prefs::get(cx)
        }
        .set(cx);
    });
    advance(&mut f, SAVE_DELAY);
    let (store, _) = PanelStore::load(&path(&f.dir));
    assert_eq!(store.window().open, Some(false));
    assert_eq!(store.window().width, Some(512.0));

    // A fresh start from that file hands the same layout to `Prefs`.
    let at = path(&f.dir);
    f.vcx.update(|_, cx| {
        Prefs::default().set_without_saving(cx);
        Saved::install(at, cx);
        assert_eq!(
            Prefs::get(cx),
            Prefs {
                open: Some(false),
                width: Some(512.0)
            }
        );
    });
}
