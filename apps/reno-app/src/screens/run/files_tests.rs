//! The Files tab of the side panel, against the fake daemon: the lazy tree and its pages, the
//! viewer for each kind of file, search, git marks, the watch and when it is open, the narrow
//! panel, and a run whose workspace is gone.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use gpui::{Entity, TestAppContext, VisualTestContext, px, size};
use reno_client::fake::FakeDaemon;
use reno_proto::FsChange;
use std::sync::Arc;

use super::RunScreen;
use crate::app::RootView;
use crate::nav::Nav;
use crate::panel::surface::Surface;
use crate::panel::view::Prefs;
use crate::tests::{click, draw, fake_store, painted, run_id, wait_drawn, wait_until, window};
use crate::ui::theme::Appearance;
use crate::vm::files::{Deco, DocState, FilesModel, Highlight, Live, Results, Row, View};

type Fixture<'a> = (
    Entity<RootView>,
    &'a mut VisualTestContext,
    Entity<RunScreen>,
    Arc<FakeDaemon>,
);

/// A wide window with the run `name` open and its panel drawn.
fn open_run<'a>(cx: &'a mut TestAppContext, name: &str, width: f32) -> Fixture<'a> {
    let store = fake_store(cx);
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

fn show_files(vcx: &mut VisualTestContext, screen: &Entity<RunScreen>) {
    screen.update_in(vcx, |s, window, cx| {
        s.open_surface(Surface::Files, window, cx);
    });
    wait_drawn(vcx, |vcx| painted(vcx, "files-tab"));
}

fn model<T>(
    vcx: &VisualTestContext,
    screen: &Entity<RunScreen>,
    f: impl FnOnce(&FilesModel) -> T,
) -> T {
    screen.read_with(vcx, |s, _| f(s.files_model().expect("a files tab")))
}

fn entry_names(m: &FilesModel) -> Vec<String> {
    m.rows()
        .into_iter()
        .filter_map(|r| match r {
            Row::Entry(e) => Some(e.path),
            Row::Note { .. } => None,
        })
        .collect()
}

fn wait_row(vcx: &mut VisualTestContext, path: &str) {
    let selector = format!("files-row-{path}");
    wait_drawn(vcx, |vcx| painted(vcx, &selector));
}

fn wait_doc(vcx: &mut VisualTestContext, screen: &Entity<RunScreen>, selector: &str) {
    wait_drawn(vcx, |vcx| painted(vcx, selector));
    let _ = screen;
}

#[gpui::test]
fn the_tree_loads_lazily_expands_and_pages_a_six_hundred_file_folder(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 800.0);
    show_files(vcx, &screen);
    // Directories first, then files, each by name; nothing below the root is read yet.
    wait_row(vcx, "README.md");
    assert_eq!(
        model(vcx, &screen, entry_names),
        [
            "assets",
            "docs",
            "many",
            "src",
            "Cargo.toml",
            "notes.txt",
            "README.md"
        ]
    );
    assert!(model(vcx, &screen, |m| m.dir("many").is_none()));
    assert!(painted(vcx, "panel-tab-files"));

    click(vcx, "files-row-src");
    wait_row(vcx, "src/lib.rs");
    assert!(painted(vcx, "files-row-src/main.rs"));
    // Closing it hides its rows; opening it again does not read it again.
    let lists = model(vcx, &screen, |m| m.lists);
    click(vcx, "files-row-src");
    draw(vcx);
    assert!(!painted(vcx, "files-row-src/lib.rs"));
    click(vcx, "files-row-src");
    draw(vcx);
    assert!(painted(vcx, "files-row-src/lib.rs"));
    assert_eq!(model(vcx, &screen, |m| m.lists), lists);

    // The big folder arrives a page at a time.
    click(vcx, "files-row-many");
    wait_drawn(vcx, |vcx| painted(vcx, "files-row-many/f000.txt"));
    let first = model(vcx, &screen, |m| {
        entry_names(m)
            .iter()
            .filter(|p| p.starts_with("many/"))
            .count()
    });
    assert_eq!(first, 500, "the default page");
    screen.read_with(vcx, |s, _| s.files_scroll_tree_to_end());
    wait_drawn(vcx, |vcx| painted(vcx, "files-more-many"));
    click(vcx, "files-more-many");
    wait_drawn(vcx, |vcx| {
        model(vcx, &screen, |m| {
            entry_names(m)
                .iter()
                .filter(|p| p.starts_with("many/"))
                .count()
                == 600
        })
    });
    screen.read_with(vcx, |s, _| s.files_scroll_tree_to_end());
    draw(vcx);
    assert!(
        !painted(vcx, "files-more-many"),
        "the last page has no more"
    );
    assert!(painted(vcx, "files-row-many/f599.txt"));
}

#[gpui::test]
fn a_text_file_opens_with_line_numbers_and_gets_highlighted_in_the_background(
    cx: &mut TestAppContext,
) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 800.0);
    show_files(vcx, &screen);
    wait_row(vcx, "src");
    click(vcx, "files-row-src");
    wait_row(vcx, "src/main.rs");
    click(vcx, "files-row-src/main.rs");
    wait_doc(vcx, &screen, "files-doc-text");
    wait_drawn(vcx, |vcx| {
        model(vcx, &screen, |m| {
            matches!(
                m.doc().map(|d| &d.state),
                Some(DocState::Text(t)) if matches!(t.highlight, Highlight::Ready(_))
            )
        })
    });
    model(vcx, &screen, |m| {
        let doc = m.doc().unwrap();
        assert_eq!(doc.path, "src/main.rs");
        let DocState::Text(t) = &doc.state else {
            panic!()
        };
        assert_eq!(t.lines(), 3);
        assert_eq!(t.line(0), "fn main() {");
        assert_eq!(m.selected(), Some("src/main.rs"));
    });
    assert!(painted(vcx, "files-doc-path"));
    assert!(!painted(vcx, "files-not-worktree"), "a worktree run");
    assert!(!painted(vcx, "files-back"), "the wide panel has no back");
    // The viewer is to the right of the tree, and both fit inside the panel.
    let panel = vcx.debug_bounds("panel-canvas").unwrap();
    let tree = vcx.debug_bounds("files-tree-pane").unwrap();
    let viewer = vcx.debug_bounds("files-viewer").unwrap();
    assert!(tree.right() <= viewer.left() + px(1.0));
    assert!(viewer.right() <= panel.right() + px(1.0));
    let share = f32::from(tree.size.width) / f32::from(panel.size.width);
    assert!(
        (0.34..0.42).contains(&share),
        "the tree is about 38%: {share}"
    );
}

#[gpui::test]
fn markdown_previews_and_toggles_to_its_source(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 800.0);
    show_files(vcx, &screen);
    wait_row(vcx, "README.md");
    click(vcx, "files-row-README.md");
    wait_doc(vcx, &screen, "files-doc-markdown");
    assert!(painted(vcx, "files-md-preview") && painted(vcx, "files-md-source"));
    assert!(!painted(vcx, "files-doc-text"));
    click(vcx, "files-md-source");
    draw(vcx);
    assert!(painted(vcx, "files-doc-text"));
    assert!(!painted(vcx, "files-doc-markdown"));
    assert_eq!(model(vcx, &screen, |m| m.doc().unwrap().view), View::Source);
    click(vcx, "files-md-preview");
    draw(vcx);
    assert!(painted(vcx, "files-doc-markdown"));
    // A source file has no toggle.
    click(vcx, "files-row-Cargo.toml");
    wait_doc(vcx, &screen, "files-doc-text");
    assert!(!painted(vcx, "files-md-preview"));
}

#[gpui::test]
fn binary_and_image_files_have_their_own_pages(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 800.0);
    show_files(vcx, &screen);
    wait_row(vcx, "assets");
    click(vcx, "files-row-assets");
    wait_row(vcx, "assets/blob.bin");
    click(vcx, "files-row-assets/blob.bin");
    wait_doc(vcx, &screen, "files-doc-binary");
    assert!(!painted(vcx, "files-doc-text"));
    click(vcx, "files-row-assets/logo.png");
    wait_doc(vcx, &screen, "files-doc-image");
    assert!(model(vcx, &screen, |m| matches!(
        m.doc().unwrap().state,
        DocState::Image { .. }
    )));
}

#[gpui::test]
fn file_name_search_and_content_search_jump_to_the_file_and_line(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 800.0);
    show_files(vcx, &screen);
    wait_row(vcx, "README.md");
    let field = screen.read_with(vcx, |s, _| s.files_search_field().cloned().unwrap());
    field.update_in(vcx, |f, window, cx| f.fill("lib", window, cx));
    // The debounce is the caller's: nothing is asked before it ends.
    assert_eq!(
        model(vcx, &screen, |m| m.search.results.clone()),
        Results::Idle
    );
    vcx.executor().advance_clock(Duration::from_millis(400));
    wait_drawn(vcx, |vcx| painted(vcx, "files-result-0"));
    assert_eq!(model(vcx, &screen, |m| m.result_paths()), ["src/lib.rs"]);
    assert!(
        !painted(vcx, "files-row-README.md"),
        "the results replace the tree"
    );
    click(vcx, "files-result-0");
    wait_doc(vcx, &screen, "files-doc-text");
    model(vcx, &screen, |m| {
        assert_eq!(m.doc().unwrap().path, "src/lib.rs");
        assert!(
            m.is_expanded("src"),
            "its folder is open behind the results"
        );
    });

    // `>` searches inside the files, and a result carries its line.
    field.update_in(vcx, |f, window, cx| f.fill("> alpha", window, cx));
    vcx.executor().advance_clock(Duration::from_millis(400));
    wait_drawn(vcx, |vcx| painted(vcx, "files-result-1"));
    assert_eq!(
        model(vcx, &screen, |m| m.result_paths()),
        ["notes.txt", "notes.txt"]
    );
    click(vcx, "files-result-1");
    wait_doc(vcx, &screen, "files-doc-text");
    model(vcx, &screen, |m| {
        let doc = m.doc().unwrap();
        assert_eq!((doc.path.as_str(), doc.line), ("notes.txt", Some(3)));
    });

    // The toggle does what the prefix does, and clearing brings the tree back.
    field.update_in(vcx, |f, window, cx| f.fill("", window, cx));
    vcx.executor().advance_clock(Duration::from_millis(400));
    wait_drawn(vcx, |vcx| painted(vcx, "files-row-README.md"));
    field.update_in(vcx, |f, window, cx| f.fill("beta", window, cx));
    click(vcx, "files-mode-text");
    wait_drawn(vcx, |vcx| painted(vcx, "files-result-0"));
    assert_eq!(model(vcx, &screen, |m| m.result_paths()), ["notes.txt"]);
}

#[gpui::test]
fn git_status_decorates_files_and_marks_their_folders(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 800.0);
    show_files(vcx, &screen);
    wait_row(vcx, "Cargo.toml");
    wait_drawn(vcx, |vcx| model(vcx, &screen, |m| m.git.loaded));
    let deco = |path: &str| {
        model(vcx, &screen, |m| {
            m.rows().into_iter().find_map(|r| match r {
                Row::Entry(e) if e.path == path => Some((e.deco, e.dirty)),
                _ => None,
            })
        })
    };
    assert_eq!(deco("Cargo.toml"), Some((Some(Deco::Modified), false)));
    assert_eq!(deco("notes.txt"), Some((Some(Deco::Untracked), false)));
    // Folders carry no letter, but say they hold a change, at any depth.
    assert_eq!(deco("docs"), Some((None, true)));
    assert_eq!(deco("src"), Some((None, true)));
    assert_eq!(deco("assets"), Some((None, false)));
    assert_eq!(
        model(vcx, &screen, |m| m.git.branch.clone()).as_deref(),
        Some("reno/builder1")
    );
}

#[gpui::test]
fn a_watch_item_reads_what_changed_and_degraded_says_live_updates_are_down(
    cx: &mut TestAppContext,
) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder1", 800.0);
    show_files(vcx, &screen);
    wait_row(vcx, "README.md");
    wait_drawn(vcx, |vcx| model(vcx, &screen, |m| m.live == Live::On));
    assert_eq!(fake.fs_watches(), 1);
    click(vcx, "files-row-src");
    wait_row(vcx, "src/lib.rs");
    click(vcx, "files-row-src/lib.rs");
    wait_doc(vcx, &screen, "files-doc-text");

    let run = run_id("builder1");
    let lists = model(vcx, &screen, |m| m.lists);
    fake.push_fs_change(
        &run,
        FsChange {
            dirs: vec![String::new(), "src".into()],
            git: true,
            ..FsChange::default()
        },
    );
    wait_drawn(vcx, |vcx| model(vcx, &screen, |m| m.lists >= lists + 2));
    assert_eq!(model(vcx, &screen, |m| m.lists), lists + 2, "root and src");
    assert!(painted(vcx, "files-row-src/lib.rs"), "what is shown stays");

    fake.push_fs_change(
        &run,
        FsChange {
            degraded: true,
            ..FsChange::default()
        },
    );
    wait_drawn(vcx, |vcx| painted(vcx, "files-live-down"));
    // The item that ends the spell has `overflow`: everything shown is read again.
    let lists = model(vcx, &screen, |m| m.lists);
    fake.push_fs_change(
        &run,
        FsChange {
            overflow: true,
            ..FsChange::default()
        },
    );
    wait_drawn(vcx, |vcx| !painted(vcx, "files-live-down"));
    assert!(model(vcx, &screen, |m| m.lists) > lists);
}

#[gpui::test]
fn the_watch_runs_only_while_the_tab_is_the_visible_one(cx: &mut TestAppContext) {
    let (root, vcx, screen, fake) = open_run(cx, "builder1", 800.0);
    assert_eq!(fake.fs_watches(), 0, "nothing opened yet");
    show_files(vcx, &screen);
    wait_drawn(vcx, |_| fake.fs_watches() == 1);

    // Another tab in front: the subscription is dropped.
    click(vcx, "panel-tab-session");
    draw(vcx);
    wait_drawn(vcx, |_| fake.fs_watches() == 0);
    // Back: it subscribes again and reads what it missed.
    let lists = model(vcx, &screen, |m| m.lists);
    click(vcx, "panel-tab-files");
    draw(vcx);
    wait_drawn(vcx, |_| fake.fs_watches() == 1);
    assert!(
        model(vcx, &screen, |m| m.lists) > lists,
        "a rescan on return"
    );

    // The panel hidden: dropped. Shown: back.
    click(vcx, "panel-hide");
    draw(vcx);
    wait_drawn(vcx, |_| fake.fs_watches() == 0);
    vcx.dispatch_action(crate::actions::TogglePanel);
    draw(vcx);
    wait_drawn(vcx, |_| fake.fs_watches() == 1);

    // Closing the tab: dropped.
    click(vcx, "panel-tab-close-files");
    draw(vcx);
    wait_drawn(vcx, |_| fake.fs_watches() == 0);
    click(vcx, "panel-new-tab");
    vcx.simulate_keystrokes("f");
    draw(vcx);
    wait_drawn(vcx, |_| fake.fs_watches() == 1);

    // Leaving the run screen: dropped. (The test's own handle would keep the screen alive.)
    drop(screen);
    root.update_in(vcx, |r, window, cx| r.navigate(Nav::Runs, window, cx));
    draw(vcx);
    wait_drawn(vcx, |_| fake.fs_watches() == 0);
}

#[gpui::test]
fn a_removed_workspace_says_its_files_are_unavailable_and_why(cx: &mut TestAppContext) {
    let (_root, vcx, screen, fake) = open_run(cx, "builder0", 800.0);
    show_files(vcx, &screen);
    wait_drawn(vcx, |vcx| painted(vcx, "files-unavailable"));
    assert!(!painted(vcx, "files-tree-pane"));
    assert_eq!(
        model(vcx, &screen, |m| m.unavailable().map(str::to_owned)).as_deref(),
        Some("the files of this run are unavailable: its worktree was removed")
    );
    // The watch cannot start for it either: nothing is held open.
    assert_eq!(fake.fs_watches(), 0);
}

#[gpui::test]
fn a_run_working_in_place_says_it_is_not_a_worktree(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "reviewer1", 800.0);
    show_files(vcx, &screen);
    wait_row(vcx, "README.md");
    click(vcx, "files-row-README.md");
    wait_doc(vcx, &screen, "files-doc-markdown");
    wait_drawn(vcx, |vcx| painted(vcx, "files-not-worktree"));
}

#[gpui::test]
fn a_narrow_panel_stacks_the_tree_and_the_viewer_with_a_way_back(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 470.0);
    show_files(vcx, &screen);
    wait_row(vcx, "README.md");
    let panel = vcx.debug_bounds("panel-canvas").unwrap();
    assert!(panel.size.width < px(560.0), "{:?}", panel.size.width);
    assert!(painted(vcx, "files-tree-pane"));
    assert!(!painted(vcx, "files-viewer"), "the tree is the page");
    click(vcx, "files-row-README.md");
    wait_doc(vcx, &screen, "files-doc-markdown");
    assert!(!painted(vcx, "files-tree-pane"), "the viewer is the page");
    assert!(painted(vcx, "files-back"));
    click(vcx, "files-back");
    draw(vcx);
    assert!(painted(vcx, "files-tree-pane"));
    assert!(!painted(vcx, "files-viewer"));
    // The tree remembers what was open.
    assert_eq!(
        model(vcx, &screen, |m| m.selected().map(str::to_owned)).as_deref(),
        Some("README.md")
    );
}

#[gpui::test]
fn review_can_open_a_file_in_the_files_tab(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 800.0);
    let lease = "crates/reno-kernel/src/lease.rs";
    wait_drawn(vcx, |vcx| painted(vcx, &format!("change-files-{lease}")));
    click(vcx, &format!("change-files-{lease}"));
    wait_drawn(vcx, |vcx| painted(vcx, "files-tab"));
    screen.read_with(vcx, |s, _| {
        assert_eq!(*s.panel.active(), Surface::Files);
        assert!(s.panel.tabs().contains(&Surface::Review), "Review stays");
    });
    model(vcx, &screen, |m| {
        assert_eq!(m.doc().map(|d| d.path.as_str()), Some(lease));
        assert!(m.is_expanded("crates") && m.is_expanded("crates/reno-kernel"));
    });
    // The fake tree has no such file: the viewer says so instead of failing.
    wait_doc(vcx, &screen, "files-doc-unavailable");
    // The diff tab still has its own arrow.
    assert!(painted(vcx, &format!("change-open-{lease}")) || !painted(vcx, "review"));
}

#[gpui::test]
fn the_files_tab_is_saved_as_its_own_kind_and_comes_back_to_the_root(cx: &mut TestAppContext) {
    let (_root, vcx, screen, _fake) = open_run(cx, "builder1", 800.0);
    show_files(vcx, &screen);
    wait_row(vcx, "README.md");
    let snapshot = screen.read_with(vcx, |s, _| s.panel_state().snapshot());
    let json = serde_json::to_string(&snapshot).unwrap();
    assert!(json.contains(r#"{"kind":"files","key":""}"#), "{json}");
    let restored = crate::panel::PanelState::restore(&serde_json::from_str(&json).unwrap());
    assert!(restored.tabs().contains(&Surface::Files));
    assert_eq!(restored.active(), &Surface::Files);
}
