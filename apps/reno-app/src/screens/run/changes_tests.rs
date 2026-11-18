//! The Run screen's tool output and Changes view, against the fake daemon.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_client::fake::FakeConfig;
use reno_client::{ConnectionStatus, Feed, FeedState};
use reno_proto::{ErrorCode, ErrorObject, RunStatus};

use super::RunScreen;
use crate::app::RootView;
use crate::panel::surface::Surface;
use crate::store::Store;
use crate::tests::{click, draw, fake_store, painted, run_id, wait_drawn, wait_until, window};
use crate::transcript::Row;
use crate::ui::theme::Appearance;
use crate::vm::changes::{PatchPane, RowKind};

fn store_with(cx: &mut TestAppContext, mut config: FakeConfig) -> Entity<Store> {
    crate::tests::one_window_test_at_a_time();
    cx.update(gpui_tokio::init);
    config.step = std::time::Duration::ZERO;
    cx.new(|cx| {
        let mut store = Store::connecting();
        store.start_fake_with(config, cx);
        store
    })
}

fn connected(cx: &mut TestAppContext, store: &Entity<Store>) {
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| {
            s.connection() == ConnectionStatus::Connected
                && s.state().feed(Feed::Runs) == FeedState::Live
        })
    });
}

/// Opens `run` and waits for its screen to draw.
fn open(vcx: &mut VisualTestContext, root: &Entity<RootView>, run: &str) -> Entity<RunScreen> {
    let id = run_id(run);
    root.update_in(vcx, |r, window, cx| r.open_run(id, window, cx));
    draw(vcx);
    root.read_with(vcx, |r, _| r.run_screen().cloned())
        .expect("a run screen")
}

fn tools_key(vcx: &mut VisualTestContext, screen: &Entity<RunScreen>) -> u64 {
    screen.read_with(vcx, |s, _| {
        s.rows()
            .iter()
            .find_map(|r| match r {
                Row::Tools { key, .. } => Some(key.0),
                _ => None,
            })
            .expect("a tool group")
    })
}

fn clipboard(vcx: &mut VisualTestContext) -> String {
    vcx.read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default()
}

#[gpui::test]
fn a_failed_tools_output_opens_folded_and_unfolds(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let screen = open(vcx, &root, "builder0");
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| !s.rows().is_empty())
    });
    let key = tools_key(vcx, &screen);
    wait_drawn(vcx, |vcx| painted(vcx, &format!("tools-{key}")));

    // Collapsed: no block, and no noise.
    assert!(!painted(vcx, "tool-output-t2"));
    click(vcx, &format!("tools-{key}"));
    assert!(
        painted(vcx, "tool-output-t2"),
        "a group of one shows its output"
    );
    assert!(
        painted(vcx, "output-more-t2"),
        "the first lines, then the rest"
    );
    assert!(painted(vcx, "output-cut-t2"), "the daemon cut it: say so");

    // Copy puts the whole kept text on the clipboard.
    click(vcx, "output-copy-t2");
    assert!(clipboard(vcx).contains("FAILED"));

    click(vcx, "output-more-t2");
    assert!(!painted(vcx, "output-more-t2"), "all of it shows now");
    assert!(painted(vcx, "output-cut-t2"));
}

#[gpui::test]
fn a_call_without_output_adds_no_block(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    // keeper1's `ls` result has a summary and no output.
    let screen = open(vcx, &root, "keeper1");
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| !s.rows().is_empty())
    });
    let key = tools_key(vcx, &screen);
    wait_drawn(vcx, |vcx| painted(vcx, &format!("tools-{key}")));
    click(vcx, &format!("tools-{key}"));
    assert!(painted(vcx, "tool-line-t1"));
    assert!(!painted(vcx, "tool-output-t1"));
}

#[gpui::test]
fn review_shows_in_the_panel_beside_the_transcript_and_each_file_folds(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let screen = open(vcx, &root, "builder1");
    // The run changed files, so Review is in front; the transcript and the composer stay.
    wait_drawn(vcx, |vcx| painted(vcx, "change-patch-Cargo.toml"));
    assert!(painted(vcx, "panel-tab-review"));
    assert!(painted(vcx, "panel-tab-session"));
    assert!(painted(vcx, "review-finding"));
    assert!(painted(vcx, "chat-canvas"));
    assert!(painted(vcx, "composer"), "the chat is not replaced");
    // The header's Changes pill is gone.
    assert!(!painted(vcx, "header-changes"));

    // Every file is a section with its patch.
    let lease = "crates/reno-kernel/src/lease.rs";
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| {
            matches!(s.patch_pane_of(lease), Some(PatchPane::Rows { rows, .. })
                if rows.iter().any(|r| r.text.contains("saturating_add")))
        })
    });
    screen.read_with(vcx, |s, _| {
        let Some(PatchPane::Rows { rows, truncated }) = s.patch_pane_of(lease) else {
            panic!("rows");
        };
        assert!(!truncated);
        assert_eq!(rows.iter().filter(|r| r.kind == RowKind::Hunk).count(), 2);
    });
    // A binary file says so and asks for no patch.
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| {
            s.patch_pane_of("assets/logo.png") == Some(&PatchPane::Binary)
        })
    });

    // Folding a file hides its patch, and opening it brings it back.
    assert!(painted(vcx, &format!("change-patch-{lease}")));
    click(vcx, &format!("change-file-{lease}"));
    assert!(!painted(vcx, &format!("change-patch-{lease}")));
    click(vcx, &format!("change-file-{lease}"));
    assert!(painted(vcx, &format!("change-patch-{lease}")));

    // The branch can be copied from the bar that stays above the files.
    click(vcx, "copy-branch");
    assert_eq!(clipboard(vcx), "reno/builder1");
}

#[gpui::test]
fn a_finished_run_says_what_its_branch_holds(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    store.update(cx, |s, cx| {
        s.run_task("tidy the lease".into(), "claude-code", cx)
    });
    wait_until(cx, |cx| {
        store.read_with(cx, |s, _| {
            s.state()
                .runs()
                .iter()
                .any(|r| r.title.contains("tidy the lease") && r.status == RunStatus::Succeeded)
        })
    });
    let id = store.read_with(cx, |s, _| {
        s.state()
            .runs()
            .iter()
            .find(|r| r.title.contains("tidy the lease"))
            .map(|r| r.run_id.clone())
            .unwrap()
    });
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    root.update_in(vcx, |r, window, cx| r.open_run(id.clone(), window, cx));
    wait_drawn(vcx, |vcx| painted(vcx, "changes-result"));
    let screen = root.read_with(vcx, |r, _| r.run_screen().cloned()).unwrap();
    let line = screen.read_with(vcx, |s, _| s.result.clone().unwrap());
    assert!(
        line.contains("has 3 commits, 6 files changed (+15 \u{2212}8)"),
        "{line}"
    );

    // The panel may be showing Session; the result line brings Review in front.
    click(vcx, "panel-tab-session");
    wait_drawn(vcx, |vcx| painted(vcx, "details"));
    click(vcx, "changes-result");
    wait_drawn(vcx, |vcx| painted(vcx, "review"));
}

#[gpui::test]
fn a_workspace_with_no_changes_says_so(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    open(vcx, &root, "scout1");
    // Nothing changed: Session stays in front, and Review is a tab that says so.
    wait_drawn(vcx, |vcx| painted(vcx, "panel-tab-review"));
    assert!(painted(vcx, "details"));
    click(vcx, "panel-tab-review");
    wait_drawn(vcx, |vcx| painted(vcx, "view-changes-empty"));
}

#[gpui::test]
fn a_removed_workspace_says_there_is_nothing_to_show(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    open(vcx, &root, "builder0");
    wait_drawn(vcx, |vcx| painted(vcx, "panel-tab-review"));
    click(vcx, "panel-tab-review");
    wait_drawn(vcx, |vcx| painted(vcx, "view-changes-removed"));
}

#[gpui::test]
fn a_run_that_never_had_a_workspace_has_no_review_tab(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let screen = open(vcx, &root, "reviewer1");
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, cx| {
            s.store.read(cx).changes(&run_id("reviewer1")).is_some()
        })
    });
    draw(vcx);
    assert!(painted(vcx, "details"));
    assert!(painted(vcx, "panel-tab-session"));
    assert!(!painted(vcx, "panel-tab-review"));
}

#[gpui::test]
fn a_run_with_no_workspace_record_is_in_place_on_the_home(cx: &mut TestAppContext) {
    use crate::vm::panel_home::HomeKind;
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let screen = open(vcx, &root, "reviewer1");
    // "reading..." is not forever: once `workspace.get` answers there is no record.
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, cx| {
            s.store.read(cx).changes(&run_id("reviewer1")).is_some()
        })
    });
    let home = screen.read_with(vcx, |s, cx| s.home(cx).unwrap());
    let path = screen.read_with(vcx, |s, cx| {
        s.summary(cx).and_then(|r| r.cwd.clone()).expect("a cwd")
    });
    assert_eq!(
        home.row(HomeKind::Terminal).unwrap().detail,
        format!("in {}, not a worktree", path.display())
    );
    let changes = home.row(HomeKind::Changes).unwrap();
    assert_eq!(changes.reason.as_deref(), Some("not a worktree"));
    assert!(!changes.enabled);
}

#[gpui::test]
fn a_failed_read_says_why_and_can_be_tried_again(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let daemon = store.read_with(cx, |s, _| s.fake().cloned().unwrap());
    daemon.fail(
        "workspace.diff",
        ErrorObject::new(ErrorCode::Internal, "git is not on the PATH"),
    );
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    open(vcx, &root, "builder1");
    wait_drawn(vcx, |vcx| painted(vcx, "panel-tab-review"));
    click(vcx, "panel-tab-review");
    wait_drawn(vcx, |vcx| painted(vcx, "view-changes-failed"));
    daemon.clear_failures();
    click(vcx, "changes-retry");
    wait_drawn(vcx, |vcx| painted(vcx, "view-changes-files"));
}

#[gpui::test]
fn an_older_daemon_has_no_review_tab_and_the_home_says_why(cx: &mut TestAppContext) {
    let store = store_with(
        cx,
        FakeConfig {
            without_features: vec![reno_proto::FEATURE_RUN_CHANGES.to_owned()],
            ..FakeConfig::default()
        },
    );
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let screen = open(vcx, &root, "builder1");
    // No Review tab; the home dims its Changes row.
    wait_drawn(vcx, |vcx| painted(vcx, "details"));
    assert!(!painted(vcx, "panel-tab-review"));
    click(vcx, "panel-new-tab");
    wait_drawn(vcx, |vcx| painted(vcx, "home-row-changes"));
    let home = screen.read_with(vcx, |s, cx| s.home(cx).unwrap());
    let row = home.row(crate::vm::panel_home::HomeKind::Changes).unwrap();
    assert!(!row.enabled);
    // An old daemon cannot say whether the folder is a repository (no `workspace.get`): that is
    // unknown, not "no repository", so the row says the daemon is too old.
    assert_eq!(row.detail, "daemon too old");
}

#[gpui::test]
fn a_five_thousand_line_patch_draws_only_the_rows_in_view(cx: &mut TestAppContext) {
    use crate::vm::changes::DiffRow;
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let screen = open(vcx, &root, "builder1");
    let path = "Cargo.toml";
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| {
            matches!(s.patch_pane_of(path), Some(PatchPane::Rows { .. }))
        })
    });
    let rows: Vec<DiffRow> = (0..5_000u32)
        .map(|i| DiffRow {
            kind: RowKind::Add,
            old: None,
            new: Some(i + 1),
            text: format!("line {i}"),
        })
        .collect();
    super::changes::ROWS_BUILT.with(|n| n.set(0));
    screen.update(vcx, |s, cx| s.inject_patch(path, rows, cx));
    draw(vcx);
    draw(vcx);
    let built = super::changes::ROWS_BUILT.with(std::cell::Cell::get);
    let total = screen.read_with(vcx, |s, _| s.review_row_count());
    assert!(
        total >= 4_001,
        "a row per line (capped) and the bars: {total}"
    );
    assert!(built > 0, "the patch is on screen");
    assert!(
        built < 400,
        "{built} rows were built for a patch of 5,000 lines: only the ones in view and the overdraw"
    );
    assert!(painted(vcx, &format!("change-patch-{path}")));
    assert!(!painted(vcx, "patch-cut"), "the end is far below");
    // The cap's message is the last row of that file.
    screen.read_with(vcx, |s, _| s.review_scroll_to_end());
    draw(vcx);
    draw(vcx);
    assert!(
        painted(vcx, "patch-cut") || painted(vcx, "change-file-docs/old-notes.md"),
        "the end of the list is in view"
    );
}

#[gpui::test]
fn the_arrow_on_a_file_opens_it_in_its_own_tab_with_its_diff(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let screen = open(vcx, &root, "builder1");
    let lease = "crates/reno-kernel/src/lease.rs";
    wait_drawn(vcx, |vcx| painted(vcx, &format!("change-open-{lease}")));
    click(vcx, &format!("change-open-{lease}"));
    let file = Surface::File(lease.to_owned());
    wait_drawn(vcx, |vcx| painted(vcx, &format!("file-tab-{lease}")));
    screen.read_with(vcx, |s, _| {
        assert_eq!(*s.panel.active(), file);
        assert!(s.panel.tabs().contains(&Surface::Review), "Review stays");
        assert!(
            !s.review_folded.contains(lease),
            "the arrow did not fold it"
        );
    });
    assert!(painted(vcx, &format!("panel-tab-{}", file.slug())));
    // The file's bar and its diff, from the same rows Review draws.
    wait_drawn(vcx, |vcx| painted(vcx, &format!("change-patch-{lease}")));
    assert!(painted(vcx, &format!("change-file-{lease}")));
    assert!(!painted(vcx, "review"), "Review is not the page now");
    // Another file, not in the changes, says so.
    screen.update_in(vcx, |s, window, cx| {
        s.open_surface(Surface::File("nope.rs".into()), window, cx);
    });
    wait_drawn(vcx, |vcx| painted(vcx, "file-tab-nope.rs"));
    assert!(!painted(vcx, "change-patch-nope.rs"));
}
