//! The History tab, the Commit tab and the Review upgrade (scopes, split, syntax colour,
//! staleness), against the fake daemon and its six-commit history: a merge, two tags and
//! `origin/main`. The fake's commits are named by their repeated digits.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext, px, size};
use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_proto::{ErrorCode, ErrorObject};

use super::RunScreen;
use crate::app::RootView;
use crate::panel::persist::{PanelStore, SAVE_DELAY};
use crate::panel::saved::Saved;
use crate::panel::surface::Surface;
use crate::panel::view::Prefs;
use crate::store::Store;
use crate::tests::{click, draw, fake_store, painted, run_id, wait_drawn, wait_until, window};
use crate::ui::theme::Appearance;
use crate::vm::changes::{Loaded, PatchPane, ReviewScope};
use crate::vm::git_graph::{HistoryModel, Openable, Status};

/// The merge (HEAD), the thread's two commits, its base, then the older two.
const MERGE: &str = "e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6";
const FEATURE: &str = "f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5";
const SIDE: &str = "5454545454545454545454545454545454545454";
const BASE: &str = "b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3";
const OLDER: &str = "c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2";

type Fixture<'a> = (
    Entity<RootView>,
    &'a mut VisualTestContext,
    Entity<RunScreen>,
    Arc<FakeDaemon>,
);

fn store_with(cx: &mut TestAppContext, mut config: FakeConfig) -> Entity<Store> {
    crate::tests::one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    config.step = Duration::ZERO;
    cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(config, cx);
        store
    })
}

fn open_with<'a>(
    cx: &'a mut TestAppContext,
    store: Entity<Store>,
    name: &str,
    width: f32,
) -> Fixture<'a> {
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| s.state().approvals().len() == 2)
    });
    let fake = store.read_with(cx, |s, _| s.fake().cloned()).expect("fake");
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    vcx.simulate_resize(size(px(1700.0), px(960.0)));
    vcx.update(|_, cx| {
        Prefs {
            open: Some(true),
            width: Some(width),
            ..Prefs::default()
        }
        .set_without_saving(cx);
    });
    draw(vcx);
    let id = run_id(name);
    root.update_in(vcx, |r, window, cx| r.open_run(id, window, cx));
    draw(vcx);
    let screen = root
        .read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("a run screen");
    (root, vcx, screen, fake)
}

fn open_run<'a>(cx: &'a mut TestAppContext, name: &str, width: f32) -> Fixture<'a> {
    let store = fake_store(cx);
    open_with(cx, store, name, width)
}

fn show(vcx: &mut VisualTestContext, screen: &Entity<RunScreen>, surface: Surface) {
    screen.update_in(vcx, |s, window, cx| s.open_surface(surface, window, cx));
    draw(vcx);
}

fn show_history(vcx: &mut VisualTestContext, screen: &Entity<RunScreen>) {
    show(vcx, screen, Surface::History);
    wait_drawn(vcx, |vcx| painted(vcx, "view-history-log"));
}

fn row_id(sha: &str) -> String {
    format!("history-row-{}", &sha[..7])
}

fn model<T>(
    vcx: &VisualTestContext,
    screen: &Entity<RunScreen>,
    f: impl FnOnce(&HistoryModel) -> T,
) -> T {
    screen.read_with(vcx, |s, _| f(s.history_model().expect("a history tab")))
}

fn active(vcx: &VisualTestContext, screen: &Entity<RunScreen>) -> Surface {
    screen.read_with(vcx, |s, _| s.panel.active().clone())
}

fn clipboard(vcx: &mut VisualTestContext) -> String {
    vcx.read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default()
}

#[gpui::test]
fn the_log_draws_every_commit_with_its_badges_and_the_merge_in_two_lanes(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 800.0);
    show_history(vcx, &screen);
    let root_commit = "a1".repeat(20);
    for sha in [MERGE, FEATURE, SIDE, BASE, OLDER, root_commit.as_str()] {
        assert!(painted(vcx, &row_id(sha)), "{sha}");
        assert!(painted(vcx, &format!("history-graph-{}", &sha[..7])));
    }
    // Badges: HEAD and the branch on the merge, main and origin/main on the base, both tags.
    for badge in ["HEAD", "origin/main", "main", "v0.1", "v0.2"] {
        assert!(painted(vcx, &format!("history-badge-{badge}")), "{badge}");
    }
    // The merge forks into two lanes; the feature commit stays on lane 0, the side one is on 1.
    model(vcx, &screen, |m| {
        assert_eq!(m.status, Status::Ready);
        assert_eq!(m.graph().max_lane_count, 2);
        let lanes: Vec<usize> = m.graph().rows.iter().map(|r| r.node_lane).collect();
        assert_eq!(lanes, [0, 0, 1, 0, 0, 0]);
        assert!(m.graph().rows[0].is_head);
        // The thread's own commits open a diff; the base and older ones do not.
        assert_eq!(m.openable(MERGE), Openable::Yes);
        assert_eq!(m.openable(SIDE), Openable::Yes);
        assert_eq!(m.openable(BASE), Openable::BeforeBase);
        assert_eq!(m.openable(OLDER), Openable::BeforeBase);
    });
    // Author, age and short name are in the wide row.
    assert!(painted(vcx, "history-end"), "six commits are all there is");
}

#[gpui::test]
fn the_log_pages_by_the_cursor_and_says_when_it_ends(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 800.0);
    screen.update(vcx, |s, _| s.history_page = 2);
    show(vcx, &screen, Surface::History);
    wait_drawn(vcx, |vcx| painted(vcx, "history-more"));
    assert_eq!(model(vcx, &screen, |m| m.rows().len()), 2);
    assert!(painted(vcx, &row_id(MERGE)));
    assert!(!painted(vcx, &row_id(SIDE)));
    click(vcx, "history-more");
    wait_drawn(vcx, |vcx| {
        model(vcx, &screen, |m| m.rows().len()) == 4 && painted(vcx, "history-more")
    });
    click(vcx, "history-more");
    wait_drawn(vcx, |vcx| painted(vcx, "history-end"));
    assert_eq!(model(vcx, &screen, |m| m.rows().len()), 6);
    // The lanes are laid out over every page, not page by page.
    assert_eq!(model(vcx, &screen, |m| m.graph().max_lane_count), 2);
}

#[gpui::test]
fn search_replaces_the_log_by_matches_and_clearing_brings_it_back(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 800.0);
    show_history(vcx, &screen);
    let field = screen.read_with(vcx, |s, _| s.history_search_field().cloned().unwrap());
    field.update_in(vcx, |f, window, cx| f.fill("lease", window, cx));
    // The debounce is the screen's: nothing is asked before it ends.
    assert!(!model(vcx, &screen, |m| m.searching()));
    vcx.executor().advance_clock(Duration::from_millis(400));
    wait_drawn(vcx, |vcx| {
        model(vcx, &screen, |m| {
            m.searching() && m.rows().len() == 3 && m.search_state().is_some()
        }) && !painted(vcx, &row_id(MERGE))
    });
    assert!(painted(vcx, &row_id(FEATURE)));
    assert!(painted(vcx, &row_id(SIDE)));
    assert!(painted(vcx, &row_id(BASE)));

    // Nothing matches: it says so.
    field.update_in(vcx, |f, window, cx| f.fill("zzzzqqq", window, cx));
    vcx.executor().advance_clock(Duration::from_millis(400));
    wait_drawn(vcx, |vcx| painted(vcx, "view-history-nomatch"));

    // Clearing the field is the log again, with no request.
    field.update_in(vcx, |f, window, cx| f.fill("", window, cx));
    vcx.executor().advance_clock(Duration::from_millis(400));
    wait_drawn(vcx, |vcx| painted(vcx, &row_id(MERGE)));
    assert!(!model(vcx, &screen, |m| m.searching()));
    assert_eq!(model(vcx, &screen, |m| m.rows().len()), 6);
}

#[gpui::test]
fn a_failed_log_says_why_and_can_be_read_again(cx: &mut TestAppContext) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1", 800.0);
    fake.fail(
        "git.log",
        ErrorObject::new(ErrorCode::Internal, "git is not on the PATH"),
    );
    show(vcx, &screen, Surface::History);
    wait_drawn(vcx, |vcx| painted(vcx, "view-history-failed"));
    fake.clear_failures();
    click(vcx, "history-retry");
    wait_drawn(vcx, |vcx| painted(vcx, "view-history-log"));
}

#[gpui::test]
fn a_run_with_no_worktree_has_no_history_to_show(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "reviewer1", 800.0);
    show(vcx, &screen, Surface::History);
    wait_drawn(vcx, |vcx| painted(vcx, "view-history-norepo"));
}

#[gpui::test]
fn a_narrow_panel_keeps_the_graph_and_the_subject_only(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 440.0);
    show_history(vcx, &screen);
    assert!(painted(vcx, &row_id(MERGE)));
    assert!(painted(vcx, &format!("history-graph-{}", &MERGE[..7])));
    assert!(!painted(vcx, "history-badge-HEAD"), "no badges when narrow");
}

#[gpui::test]
fn a_click_opens_the_commit_in_its_own_tab_with_its_diff(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 800.0);
    show_history(vcx, &screen);
    click(vcx, &row_id(FEATURE));
    let tab = Surface::Commit(FEATURE.to_owned());
    assert_eq!(active(vcx, &screen), tab);
    wait_drawn(vcx, |vcx| painted(vcx, "view-commit-files"));
    assert!(painted(
        vcx,
        "panel-tab-commit-f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5"
    ));
    // The commit's own line, from the log, and the diff in the same rows Review draws.
    assert!(painted(vcx, "commit-head"));
    wait_drawn(vcx, |vcx| painted(vcx, "change-patch-Cargo.toml"));
    screen.read_with(vcx, |s, _| {
        let Some(Loaded::Ready(diff)) = s.commit_diff(FEATURE) else {
            panic!("the commit's files");
        };
        assert_eq!(diff.files.len(), 1);
        assert!(matches!(
            s.commit_pane(FEATURE, "Cargo.toml"),
            Some(PatchPane::Rows { .. })
        ));
    });
    // The history stays a tab of its own, with the commit selected.
    assert!(screen.read_with(vcx, |s, _| s.panel.tabs().contains(&Surface::History)));
    assert_eq!(
        model(vcx, &screen, |m| m.selected().map(str::to_owned)),
        Some(FEATURE.to_owned())
    );
    // Its files fold like Review's, and copy their path.
    click(vcx, "change-file-Cargo.toml");
    assert!(!painted(vcx, "change-patch-Cargo.toml"));
    click(vcx, "change-file-Cargo.toml");
    wait_drawn(vcx, |vcx| painted(vcx, "change-patch-Cargo.toml"));
    click(vcx, "change-copy-Cargo.toml");
    assert_eq!(clipboard(vcx), "Cargo.toml");
}

#[gpui::test]
fn a_commit_before_the_base_offers_no_diff_and_a_refusal_shows_inline(cx: &mut TestAppContext) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1", 800.0);
    show_history(vcx, &screen);
    // The base and older commits are dimmed and do not open.
    click(vcx, &row_id(BASE));
    assert_eq!(active(vcx, &screen), Surface::History);
    assert_eq!(
        model(vcx, &screen, |m| m.selected().map(str::to_owned)),
        Some(BASE.to_owned())
    );
    assert!(!screen.read_with(vcx, |s, _| {
        s.panel.tabs().contains(&Surface::Commit(BASE.to_owned()))
    }));

    // A restored tab can still name one: the daemon's refusal is the page, not a toast.
    fake.fail(
        "workspace.diff",
        ErrorObject::new(
            ErrorCode::InvalidParams,
            "that commit is before the thread's base",
        ),
    );
    show(vcx, &screen, Surface::Commit(OLDER.to_owned()));
    wait_drawn(vcx, |vcx| painted(vcx, "view-commit-failed"));
    screen.read_with(vcx, |s, _| {
        let Some(Loaded::Failed(why)) = s.commit_diff(OLDER) else {
            panic!("a refusal");
        };
        assert!(why.contains("before the thread's base"), "{why}");
    });
    // It can be tried again once the cause is gone.
    fake.clear_failures();
    click(vcx, "commit-retry");
    wait_drawn(vcx, |vcx| painted(vcx, "view-commit-files"));
}

#[gpui::test]
fn the_scope_switch_has_uncommitted_and_it_compares_the_worktree_with_head(
    cx: &mut TestAppContext,
) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 900.0);
    wait_drawn(vcx, |vcx| painted(vcx, "scope-working-tree"));
    assert!(painted(vcx, "scope-run") && painted(vcx, "scope-thread"));
    click(vcx, "scope-working-tree");
    assert_eq!(
        screen.read_with(vcx, |s, _| s.scope()),
        ReviewScope::WorkingTree
    );
    let run = run_id("builder1");
    let files = |vcx: &mut VisualTestContext| {
        screen.read_with(vcx, |s, cx| {
            match s.store.read(cx).changes_in(&run, ReviewScope::WorkingTree) {
                Some(c) => match &c.diff {
                    Loaded::Ready(d) => Some(d.files.len()),
                    _ => None,
                },
                None => None,
            }
        })
    };
    wait_drawn(vcx, |vcx| files(vcx).is_some());
    assert_eq!(files(vcx), Some(2), "what is not committed: two files");
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| s.review_row_count() > 2)
    });
    assert!(painted(
        vcx,
        "change-file-crates/reno-kernel/src/lease_tests.rs"
    ));
    assert!(!painted(vcx, "change-file-Cargo.toml"));
    click(vcx, "scope-thread");
    wait_drawn(vcx, |vcx| painted(vcx, "change-file-Cargo.toml"));
}

#[gpui::test]
fn split_mode_pairs_removed_and_added_lines_and_is_remembered(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let at = crate::panel::persist::path_in(dir.path());
    cx.update(|cx| Saved::install(at.clone(), cx));
    let store = fake_store(cx);
    let (_root, vcx, screen, _fake) = open_with(cx, store, "builder1", 900.0);
    let lease = "crates/reno-kernel/src/lease.rs";
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| {
            matches!(s.patch_pane_of(lease), Some(PatchPane::Rows { .. }))
        })
    });
    assert_eq!(screen.read_with(vcx, |s, _| s.review_pair_rows()), 0);
    let unified = screen.read_with(vcx, |s, _| s.review_row_count());
    click(vcx, "mode-split");
    assert!(vcx.update(|_, cx| Prefs::get(cx).split));
    draw(vcx);
    let pairs = screen.read_with(vcx, |s, _| s.review_pair_rows());
    let split = screen.read_with(vcx, |s, _| s.review_row_count());
    assert!(pairs > 0, "side-by-side rows");
    assert!(
        split < unified,
        "{split} pair rows are fewer than {unified} lines"
    );
    // Painted through the same rows as the unified list.
    assert!(painted(vcx, &format!("change-patch-{lease}")));

    // The choice goes with the panel's layout, and a fresh start reads it back.
    vcx.executor().advance_clock(SAVE_DELAY);
    vcx.run_until_parked();
    let (saved, _) = PanelStore::load(&at);
    assert!(saved.window().split);
    vcx.update(|_, cx| {
        Prefs::default().set_without_saving(cx);
        Saved::install(at.clone(), cx);
        assert!(Prefs::get(cx).split);
    });
    click(vcx, "mode-unified");
    draw(vcx);
    assert_eq!(screen.read_with(vcx, |s, _| s.review_pair_rows()), 0);
}

#[gpui::test]
fn a_changed_file_has_syntax_colour_once_its_sources_arrive(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 900.0);
    let main = "apps/renod/src/main.rs";
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| {
            matches!(s.patch_pane_of(main), Some(PatchPane::Rows { .. }))
        })
    });
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, cx| s.coloured_lines(main, cx)) > 0
    });
    // A file whose lines are not the sources' (the fake's stubs) stays plain, not mis-coloured.
    screen.read_with(vcx, |s, cx| {
        assert_eq!(s.coloured_lines("Cargo.toml", cx), 0);
    });
}

#[gpui::test]
fn without_history_the_upgrade_degrades_and_says_why(cx: &mut TestAppContext) {
    let store = store_with(
        cx,
        FakeConfig {
            without_features: vec![reno_proto::FEATURE_HISTORY.to_owned()],
            ..FakeConfig::default()
        },
    );
    let (_root, vcx, screen, _fake) = open_with(cx, store, "builder1", 900.0);
    let main = "apps/renod/src/main.rs";
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| {
            matches!(s.patch_pane_of(main), Some(PatchPane::Rows { .. }))
        })
    });
    // Plain mono lines: no sources are asked for.
    assert_eq!(screen.read_with(vcx, |s, cx| s.coloured_lines(main, cx)), 0);
    assert_eq!(screen.read_with(vcx, |s, _| s.syntax_ready()), 0);
    // Uncommitted stays on the switch, dimmed, and does nothing.
    wait_drawn(vcx, |vcx| painted(vcx, "scope-working-tree"));
    click(vcx, "scope-working-tree");
    assert_eq!(screen.read_with(vcx, |s, _| s.scope()), ReviewScope::Thread);
    // Split is the app's own, so it still works.
    click(vcx, "mode-split");
    assert!(screen.read_with(vcx, |s, _| s.review_pair_rows()) > 0);

    // The home dims its History row, and the tab itself says the daemon is too old.
    click(vcx, "panel-new-tab");
    wait_drawn(vcx, |vcx| painted(vcx, "home-row-history"));
    let home = screen.read_with(vcx, |s, cx| s.home(cx).unwrap());
    let row = home.row(crate::vm::panel_home::HomeKind::History).unwrap();
    assert!(!row.enabled);
    assert_eq!(row.detail, "needs a newer daemon");
    show(vcx, &screen, Surface::History);
    wait_drawn(vcx, |vcx| painted(vcx, "view-history-unsupported"));
    show(vcx, &screen, Surface::Commit(FEATURE.to_owned()));
    wait_drawn(vcx, |vcx| painted(vcx, "view-commit-unsupported"));
}

#[gpui::test]
fn a_diff_that_changes_under_review_is_read_again_without_losing_the_place(
    cx: &mut TestAppContext,
) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 900.0);
    let path = "Cargo.toml";
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| {
            matches!(s.patch_pane_of(path), Some(PatchPane::Rows { .. }))
        })
    });
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| s.review_row_count() > 30)
    });
    screen.read_with(vcx, |s, _| s.review_scroll_by(px(120.0)));
    draw(vcx);
    let top = screen.read_with(vcx, |s, _| s.review_top());
    assert!(top > 0, "scrolled into the diff");

    // The daemon says the diff is another now: same files, another checksum.
    let run = run_id("builder1");
    let store = screen.read_with(vcx, |s, _| s.store.clone());
    let reads = store.read_with(vcx, |s, _| s.patch_reads());
    store.update(vcx, |s, cx| {
        let mut changed = s.changes_in(&run, ReviewScope::Thread).unwrap().clone();
        if let Loaded::Ready(d) = &mut changed.diff {
            d.checksum = "a-newer-checksum".to_owned();
        }
        s.accept_changes((run.clone(), ReviewScope::Thread), changed, cx);
    });
    // What was read stays on screen while the patches are read again quietly.
    screen.read_with(vcx, |s, _| {
        assert!(matches!(
            s.patch_pane_of(path),
            Some(PatchPane::Rows { .. })
        ));
    });
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| {
            s.patch_reads() > reads && !s.has_stale_patches(&run, ReviewScope::Thread)
        })
    });
    assert!(matches!(
        screen.read_with(vcx, |s, _| s.patch_pane_of(path).cloned()),
        Some(PatchPane::Rows { .. })
    ));
    draw(vcx);
    assert_eq!(screen.read_with(vcx, |s, _| s.review_top()), top);
}

#[gpui::test]
fn the_history_and_commit_tabs_are_saved_and_come_back_reading_again(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let at = crate::panel::persist::path_in(dir.path());
    cx.update(|cx| Saved::install(at.clone(), cx));
    let store = fake_store(cx);
    let (root, vcx, screen, _fake) = open_with(cx, store, "builder1", 900.0);
    show_history(vcx, &screen);
    show(vcx, &screen, Surface::Commit(SIDE.to_owned()));
    wait_drawn(vcx, |vcx| painted(vcx, "view-commit-files"));
    vcx.executor().advance_clock(SAVE_DELAY);
    vcx.run_until_parked();

    // The file holds a descriptor for each: a kind, and the commit's name as its key.
    let (saved, _) = PanelStore::load(&at);
    let key = crate::panel::PanelKey("thr_builder1".to_owned());
    let descriptors = saved.get(&key).expect("this thread's panel");
    let json = serde_json::to_string(descriptors).unwrap();
    assert!(json.contains("\"history\""), "{json}");
    assert!(
        json.contains(&format!("\"commit\",\"key\":\"{SIDE}\"")),
        "{json}"
    );

    // A fresh screen restores both tabs; the commit is read again when it shows.
    root.update_in(vcx, |r, window, cx| {
        r.open_run(run_id("keeper1"), window, cx)
    });
    draw(vcx);
    root.update_in(vcx, |r, window, cx| {
        r.open_run(run_id("builder1"), window, cx)
    });
    draw(vcx);
    let again = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    assert_ne!(again.entity_id(), screen.entity_id());
    again.read_with(vcx, |s, _| {
        let tabs = s.panel.tabs();
        assert!(tabs.contains(&Surface::History));
        assert!(tabs.contains(&Surface::Commit(SIDE.to_owned())));
        assert_eq!(*s.panel.active(), Surface::Commit(SIDE.to_owned()));
    });
    wait_drawn(vcx, |vcx| painted(vcx, "view-commit-files"));
    assert!(matches!(
        again.read_with(vcx, |s, _| s.commit_diff(SIDE).cloned()),
        Some(Loaded::Ready(_))
    ));
}
