//! The Run screen's tool output and Changes view, against the fake daemon.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use reno_client::fake::FakeConfig;
use reno_client::{ConnectionStatus, Feed, FeedState};
use reno_proto::{ErrorCode, ErrorObject, RunStatus};

use super::RunScreen;
use crate::app::RootView;
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
fn changes_open_from_the_details_and_a_file_shows_its_patch(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    let screen = open(vcx, &root, "builder1");
    wait_drawn(vcx, |vcx| painted(vcx, "open-changes"));
    assert!(painted(vcx, "header-changes"), "the header reaches it too");

    click(vcx, "open-changes");
    wait_drawn(vcx, |vcx| painted(vcx, "change-patch"));
    // The first file is picked: its patch shows.
    assert!(painted(vcx, "change-file-Cargo.toml"));

    click(vcx, "change-file-crates/reno-kernel/src/lease.rs");
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| {
            matches!(&s.patch_pane, PatchPane::Rows { rows, .. }
                if rows.iter().any(|r| r.text.contains("saturating_add")))
        })
    });
    screen.read_with(vcx, |s, _| {
        let PatchPane::Rows { rows, truncated } = &s.patch_pane else {
            panic!("rows");
        };
        assert!(!truncated);
        assert_eq!(rows.iter().filter(|r| r.kind == RowKind::Hunk).count(), 2);
    });

    // A binary file says so and asks for no patch.
    click(vcx, "change-file-assets/logo.png");
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| s.patch_pane == PatchPane::Binary)
    });

    // The branch and the worktree path can be copied.
    click(vcx, "copy-branch");
    assert_eq!(clipboard(vcx), "reno/builder1");
    click(vcx, "copy-path");
    assert!(clipboard(vcx).ends_with("run_builder1"));

    click(vcx, "changes-back");
    wait_drawn(vcx, |vcx| !painted(vcx, "changes-view"));
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

    click(vcx, "changes-result");
    wait_drawn(vcx, |vcx| painted(vcx, "changes-view"));
}

#[gpui::test]
fn a_workspace_with_no_changes_says_so(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    open(vcx, &root, "scout1");
    wait_drawn(vcx, |vcx| painted(vcx, "section-changes-empty"));
    click(vcx, "header-changes");
    wait_drawn(vcx, |vcx| painted(vcx, "view-changes-empty"));
}

#[gpui::test]
fn a_removed_workspace_says_there_is_nothing_to_show(cx: &mut TestAppContext) {
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    draw(vcx);
    open(vcx, &root, "builder0");
    wait_drawn(vcx, |vcx| painted(vcx, "section-changes-removed"));
    click(vcx, "header-changes");
    wait_drawn(vcx, |vcx| painted(vcx, "view-changes-removed"));
}

#[gpui::test]
fn a_run_that_never_had_a_workspace_has_no_changes_section(cx: &mut TestAppContext) {
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
    assert!(!painted(vcx, "header-changes"));
    assert!(!painted(vcx, "section-changes-hidden"));
    assert!(!painted(vcx, "section-changes-failed"));
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
    wait_drawn(vcx, |vcx| painted(vcx, "section-changes-failed"));
    daemon.clear_failures();
    click(vcx, "changes-retry");
    wait_drawn(vcx, |vcx| painted(vcx, "open-changes"));
}

#[gpui::test]
fn an_older_daemon_says_to_update_and_offers_no_view(cx: &mut TestAppContext) {
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
    open(vcx, &root, "builder1");
    wait_drawn(vcx, |vcx| painted(vcx, "section-changes-unsupported"));
    assert!(!painted(vcx, "open-changes"));
    assert!(!painted(vcx, "header-changes"));
}
