//! The saved panel (D8) on the run screen: tabs and layout come back, writes wait for one
//! deadline, a quit writes at once. Time is the test clock; the files are in a temp dir, and
//! without `Saved::install` nothing here (or in any other test) reaches the real data dir.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    AppContext as _, Entity, ListOffset, Modifiers, ScrollDelta, TestAppContext, TouchPhase,
    VisualTestContext, point, px,
};
use reno_client::Update;
use reno_client::fake::FakeConfig;
use reno_proto::{ActionClass, ExecEvent, SessionRef, ToolCall};

use super::RunScreen;
use crate::app::RootView;
use crate::panel::persist::{Loaded, PanelStore, SAVE_DELAY};
use crate::panel::saved::Saved;
use crate::panel::surface::Surface;
use crate::panel::view::{Cmd, Prefs};
use crate::panel::{Descriptors, PanelKey, PanelState, Place};
use crate::store::Store;
use crate::tests::{
    draw, fake_store, one_window_test_at_a_time, painted, run_id, wait_drawn, wait_until, window,
};
use crate::transcript::{Key, Row};
use crate::ui::theme::Appearance;
use crate::vm::items::project;

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
    let saved = store.get(&PanelKey("thr_builder1".to_owned())).unwrap();
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
    assert!(store.get(&PanelKey("thr_builder1".to_owned())).is_some());
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
                width: Some(512.0),
                split: false,
            }
        );
    });
}

#[gpui::test]
fn a_file_tab_is_saved_and_comes_back_showing_its_diff(cx: &mut TestAppContext) {
    let mut f = fixture(cx, |_| {});
    let screen = open(&mut f, "builder1");
    wait_drawn(f.vcx, |vcx| painted(vcx, "panel-tab-review"));
    let file = Surface::File("Cargo.toml".to_owned());
    screen.update_in(f.vcx, |s, window, cx| {
        s.open_surface(file.clone(), window, cx);
    });
    advance(&mut f, SAVE_DELAY);
    let (store, _) = PanelStore::load(&path(&f.dir));
    let saved = store.get(&PanelKey("thr_builder1".to_owned())).unwrap();
    let restored = crate::panel::PanelState::restore(saved);
    assert!(restored.tabs().contains(&file));
    assert_eq!(*restored.active(), file);

    // A new screen restores it, shows "reading" until the changes arrive, then the diff.
    let other = open(&mut f, "keeper1");
    wait_drawn(f.vcx, |vcx| painted(vcx, "panel-tab-session"));
    assert!(!tabs(&f, &other).contains(&file));
    let again = open(&mut f, "builder1");
    assert_eq!(active(&f, &again), file);
    wait_drawn(f.vcx, |vcx| painted(vcx, "change-patch-Cargo.toml"));
    assert!(painted(f.vcx, "file-tab-Cargo.toml"));
}

// --- the reading place -----------------------------------------------------------------------

/// A window on the fake daemon's long "handshake" thread, with the panel store over a temp dir.
fn long_thread(cx: &mut TestAppContext) -> Fixture<'_> {
    let dir = tempfile::tempdir().unwrap();
    let at = path(&dir);
    cx.update(|cx| Saved::install(at, cx));
    one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    let store = cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(
            FakeConfig {
                step: Duration::ZERO,
                transcript_threads: true,
                ..FakeConfig::default()
            },
            cx,
        );
        store
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.thread_of(&run_id("acp8")).is_some())
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    Fixture { dir, root, vcx }
}

/// The handshake thread's last run, once every turn's fold is in.
fn open_long(f: &mut Fixture<'_>) -> Entity<RunScreen> {
    let screen = open(f, "acp8");
    wait_drawn(f.vcx, |vcx| {
        screen.read_with(vcx, |s, _| {
            s.rows()
                .iter()
                .filter(|r| matches!(r, Row::Worked { .. }))
                .count()
                == 8
        })
    });
    screen
}

/// The owner turns the wheel over the transcript; positive moves toward the start.
fn wheel(f: &mut Fixture<'_>, toward_start: f32) {
    let at = f.vcx.debug_bounds("transcript").unwrap().center();
    f.vcx.simulate_event(gpui::ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(0.0), px(toward_start))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
        source: gpui::ScrollSource::Pointer,
    });
    settle(f);
}

fn top(f: &Fixture<'_>, screen: &Entity<RunScreen>) -> ListOffset {
    screen.read_with(f.vcx, |s, _| s.list.logical_scroll_top())
}

fn following(f: &Fixture<'_>, screen: &Entity<RunScreen>) -> bool {
    screen.read_with(f.vcx, |s, _| s.is_following_tail())
}

fn thread_key(f: &Fixture<'_>, screen: &Entity<RunScreen>) -> PanelKey {
    screen.read_with(f.vcx, |s, _| s.panel_key().clone())
}

/// The place the store holds now (in memory, not yet necessarily on disk).
fn held_place(f: &mut Fixture<'_>, key: &PanelKey) -> Option<Place> {
    f.vcx
        .update(|_, cx| Saved::get(key, cx))
        .and_then(|saved| saved.place)
}

fn file_place(f: &Fixture<'_>, key: &PanelKey) -> Option<Place> {
    PanelStore::load(&path(&f.dir))
        .0
        .get(key)
        .and_then(|saved| saved.place)
}

fn same_place(a: ListOffset, b: ListOffset) -> bool {
    a.item_ix == b.item_ix
        && (f32::from(a.offset_in_item) - f32::from(b.offset_in_item)).abs() <= 0.5
}

/// Draws, delivers the frame the screen asked for (tests have no frame loop), and draws again.
fn settle(f: &mut Fixture<'_>) {
    draw(f.vcx);
    f.vcx.update(|window, cx| window.simulate_next_frame(cx));
    draw(f.vcx);
}

fn leave_and_come_back(f: &mut Fixture<'_>, run: &str) -> Entity<RunScreen> {
    open(f, "keeper1");
    let screen = open(f, run);
    settle(f);
    screen
}

#[gpui::test]
fn returning_to_a_thread_shows_the_saved_place_after_a_restart(cx: &mut TestAppContext) {
    let mut f = long_thread(cx);
    let screen = open_long(&mut f);
    assert!(following(&f, &screen), "a thread opens at its tail");
    wheel(&mut f, 900.0);
    assert!(!following(&f, &screen));
    assert!(top(&f, &screen).item_ix > 0, "the view moved up");
    let key = thread_key(&f, &screen);
    assert!(held_place(&mut f, &key).is_some(), "the scroll was noted");

    // Several more turns of the wheel are one write, 750 ms after the last.
    wheel(&mut f, 40.0);
    wheel(&mut f, -40.0);
    let seen = top(&f, &screen);
    assert_eq!(writes(&mut f), 0);
    advance(&mut f, SAVE_DELAY);
    assert_eq!(writes(&mut f), 1);
    assert!(file_place(&f, &key).is_some());

    // Same session: a new screen of the thread shows where it was left.
    let again = leave_and_come_back(&mut f, "acp8");
    assert_ne!(again.entity_id(), screen.entity_id());
    draw(f.vcx);
    assert!(!following(&f, &again));
    assert!(same_place(top(&f, &again), seen), "{:?}", top(&f, &again));

    // After a restart: the store is read from the file again.
    let at = path(&f.dir);
    f.vcx.update(|_, cx| Saved::install(at, cx));
    let restarted = leave_and_come_back(&mut f, "acp8");
    draw(f.vcx);
    assert!(!following(&f, &restarted));
    assert!(same_place(top(&f, &restarted), seen));
}

#[gpui::test]
fn a_view_at_the_tail_saves_no_place_and_reopens_following(cx: &mut TestAppContext) {
    let mut f = long_thread(cx);
    let screen = open_long(&mut f);
    let key = thread_key(&f, &screen);
    assert_eq!(held_place(&mut f, &key), None, "opening saves nothing");

    wheel(&mut f, 900.0);
    assert!(held_place(&mut f, &key).is_some());
    // Back down to the end: the view follows the tail again, so there is nothing to keep.
    wheel(&mut f, -100_000.0);
    assert!(following(&f, &screen), "the end of the thread is the tail");
    assert_eq!(held_place(&mut f, &key), None);
    advance(&mut f, SAVE_DELAY);
    assert_eq!(file_place(&f, &key), None);

    let again = leave_and_come_back(&mut f, "acp8");
    draw(f.vcx);
    assert!(following(&f, &again));
}

#[gpui::test]
fn jumping_to_the_latest_forgets_the_place(cx: &mut TestAppContext) {
    let mut f = long_thread(cx);
    let screen = open_long(&mut f);
    let key = thread_key(&f, &screen);
    wheel(&mut f, 900.0);
    assert!(held_place(&mut f, &key).is_some());
    screen.update(f.vcx, |s, cx| s.jump_to_latest(cx));
    draw(f.vcx);
    assert!(following(&f, &screen));
    assert_eq!(held_place(&mut f, &key), None);
}

#[gpui::test]
fn the_place_moves_with_the_run_to_thread_key(cx: &mut TestAppContext) {
    let mut f = long_thread(cx);
    let screen = open_long(&mut f);
    let thread = thread_key(&f, &screen);
    let run = PanelKey("run_acp8".to_owned());

    // A screen the daemon has not yet told the thread of.
    screen.update(f.vcx, |s, _| s.panel_key = run.clone());
    wheel(&mut f, 900.0);
    let seen = top(&f, &screen);
    let place = held_place(&mut f, &run).expect("saved under the run");
    assert_eq!(held_place(&mut f, &thread), None);

    // The daemon names the thread: the place goes with the tabs.
    screen.update(f.vcx, |s, cx| s.resolve_panel_key(cx));
    assert_eq!(thread_key(&f, &screen), thread);
    assert_eq!(held_place(&mut f, &thread), Some(place));
    assert!(same_place(top(&f, &screen), seen), "the view did not move");
}

#[gpui::test]
fn a_thread_saved_place_is_shown_when_the_thread_is_named_late(cx: &mut TestAppContext) {
    let mut f = long_thread(cx);
    let screen = open_long(&mut f);
    let thread = thread_key(&f, &screen);
    wheel(&mut f, 900.0);
    let seen = top(&f, &screen);
    let place = held_place(&mut f, &thread).unwrap();
    let saved = Descriptors {
        place: Some(place),
        ..Descriptors::default()
    };

    // A new screen that does not know its thread yet shows the tail; then it learns the thread.
    let again = leave_and_come_back(&mut f, "acp8");
    again.update(f.vcx, |s, cx| {
        s.list.set_follow_mode(gpui::FollowMode::Tail);
        s.panel_key = PanelKey("run_acp8".to_owned());
        s.panel = PanelState::default();
        s.place_pending = None;
        s.place_moved = false;
        Saved::put(thread.clone(), saved.clone(), cx);
        s.refresh(cx);
    });
    draw(f.vcx);
    assert!(same_place(top(&f, &again), seen));
}

#[gpui::test]
fn a_saved_place_waits_until_the_thread_has_reached_it(cx: &mut TestAppContext) {
    let mut f = long_thread(cx);
    let screen = open_long(&mut f);
    let run = run_id("acp8");
    let store = screen.read_with(f.vcx, |s, _| s.store.clone());
    // The row the next text event will make: nothing in the thread so far is as late.
    let highest = store.read_with(f.vcx, |s, _| {
        s.state()
            .transcript(&run)
            .and_then(|t| t.highest)
            .unwrap()
            .0
    });
    let segment = screen.read_with(f.vcx, |s, _| s.runs.len() - 1);
    let ahead = Key(highest + 3).in_segment(segment);
    screen.update(f.vcx, |s, cx| {
        s.place_pending = Some(Place {
            key: ahead.0,
            offset_pt: 0,
        });
        s.refresh(cx);
    });
    draw(f.vcx);
    assert!(following(&f, &screen), "not there yet: the view stays put");

    // The turn goes on for a good while, so what it opened with is far from the end.
    let mut events = vec![
        ExecEvent::Resumed {
            from_attempt: 0,
            session: SessionRef("s".into()),
        },
        ExecEvent::Text {
            text: "again".into(),
        },
    ];
    for n in 0..40 {
        events.push(ExecEvent::Tool {
            id: format!("step{n}"),
            call: ToolCall::new(
                "bash",
                serde_json::json!({ "command": "cargo test" }),
                ActionClass::Command,
            ),
        });
        events.push(ExecEvent::Text {
            text: format!("more {n}"),
        });
    }
    for (n, event) in events.into_iter().enumerate() {
        store.update(f.vcx, |s, cx| {
            s.apply(
                Update::RunEvent {
                    run: run.clone(),
                    seq: reno_proto::Seq(highest + 1 + n as u64),
                    event,
                },
                cx,
            );
        });
    }
    draw(f.vcx);
    let at = screen
        .read_with(f.vcx, |s, _| s.items().item_of(ahead))
        .expect("the reply is a row now");
    assert!(!following(&f, &screen));
    assert_eq!(top(&f, &screen).item_ix, at);
}

#[gpui::test]
fn scrolling_before_the_first_tabs_are_chosen_saves_only_the_place(cx: &mut TestAppContext) {
    let mut f = long_thread(cx);
    let screen = open_long(&mut f);
    let key = thread_key(&f, &screen);
    screen.update(f.vcx, |s, _| {
        s.panel_seeded = false;
        s.panel_touched = false;
    });
    wheel(&mut f, 900.0);
    let saved = f.vcx.update(|_, cx| Saved::get(&key, cx)).unwrap();
    assert!(saved.place.is_some());
    assert!(saved.tabs.is_empty() && saved.active.is_none(), "{saved:?}");

    // Once the owner uses the tabs, they are saved with the place.
    let again = leave_and_come_back(&mut f, "acp8");
    again.update_in(f.vcx, |s, window, cx| {
        s.open_surface(Surface::Home, window, cx);
    });
    let saved = f.vcx.update(|_, cx| Saved::get(&key, cx)).unwrap();
    assert!(saved.tabs.contains(&Surface::Home.descriptor()));
    assert!(saved.place.is_some());
}

fn prose(n: u64) -> Row {
    Row::Prose {
        key: Key(n),
        text: format!("reply {n}"),
    }
}

fn folded(first: u64, inner: &[u64]) -> Row {
    Row::Worked {
        key: Key::fold(Key(first)),
        secs: Some(5),
        rows: inner.iter().map(|n| prose(*n)).collect(),
    }
}

#[test]
fn a_gone_or_folded_anchor_restores_to_its_fold_head_else_the_next_entry() {
    let rows = vec![prose(0), prose(2), folded(4, &[4, 6]), prose(10), prose(12)];
    let closed = project(&rows, |_| false);
    let at = |key: u64, offset_pt: i32| {
        let landed = Place { key, offset_pt }.resolve(&closed, &rows);
        (landed.item_ix, f32::from(landed.offset_in_item))
    };
    // An entry that is there keeps its offset.
    assert_eq!(at(2, 9), (1, 9.0));
    // One folded away goes to the fold's head, at its top.
    assert_eq!(at(6, 9), (2, 0.0));
    assert_eq!(at(4, 9), (2, 0.0));
    // One that is gone goes to the fold that spans it, else to the entry after it.
    assert_eq!(at(5, 9), (2, 0.0));
    assert_eq!(at(8, 9), (3, 0.0));
    assert_eq!(at(11, 9), (4, 0.0));
    // With nothing after it: the end of the list, which is the tail.
    assert_eq!(at(99, 9), (closed.len(), 0.0));

    // The fold open, its rows are entries of their own again.
    let open = project(&rows, |_| true);
    let landed = Place {
        key: 6,
        offset_pt: 9,
    }
    .resolve(&open, &rows);
    assert_eq!(landed.item_ix, open.item_of(Key(6)).unwrap());
    assert_eq!(f32::from(landed.offset_in_item), 9.0);
}

#[test]
fn a_gone_anchor_is_reached_only_when_its_own_run_has_entries_after_it() {
    let rows = vec![prose(0), prose(2), prose(10)];
    let items = project(&rows, |_| false);
    let place = |key: u64| Place { key, offset_pt: 0 };
    assert!(place(2).is_reached(&items, &rows), "there");
    assert!(
        place(8).is_reached(&items, &rows),
        "gone, and 10 is after it"
    );
    assert!(!place(11).is_reached(&items, &rows), "nothing after it yet");
    // An entry of a later run says nothing about the earlier run's events.
    let later = vec![prose(0), prose(2), prose((1 << 40) | 3)];
    let items = project(&later, |_| false);
    assert!(!place(8).is_reached(&items, &later));
    assert!(!place((1 << 40) | 9).is_reached(&items, &later));
}
