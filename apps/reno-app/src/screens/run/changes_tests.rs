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
use gpui::px;

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

/// The key of the fold of a finished turn that holds the call `call` (its step is behind it).
fn fold_key(vcx: &mut VisualTestContext, screen: &Entity<RunScreen>, call: &str) -> u64 {
    screen.read_with(vcx, |s, _| {
        s.rows()
            .iter()
            .find_map(|r| {
                match r {
                Row::Worked { key, rows, .. }
                    if rows.iter().any(|r| {
                        matches!(r, Row::Tools { calls, .. } if calls.iter().any(|c| c.id == call))
                    }) =>
                {
                    Some(key.0)
                }
                _ => None,
            }
            })
            .expect("a fold")
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
    let key = fold_key(vcx, &screen, "t2");
    wait_drawn(vcx, |vcx| painted(vcx, &format!("worked-{key}")));

    // Folded: no steps, no block. Opened: the step, and still no block until it is opened.
    assert!(!painted(vcx, "step-t2"));
    click(vcx, &format!("worked-{key}"));
    wait_drawn(vcx, |vcx| painted(vcx, "step-t2"));
    assert!(!painted(vcx, "tool-output-t2"), "collapsed: no noise");
    click(vcx, "step-t2");
    assert!(painted(vcx, "tool-output-t2"), "the step opens its output");
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
    wait_drawn(vcx, |vcx| painted(vcx, "step-t1"));
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
    assert!(
        !painted(vcx, "review-finding"),
        "the panel does not repeat the status"
    );
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
    // The file's bar is in view where it starts, so no copy of it sticks to the top.
    assert!(!painted(vcx, &format!("sticky-change-file-{path}")));
    // Scrolled into the file, its bar sticks to the top.
    screen.read_with(vcx, |s, _| s.review_scroll_by(px(2_000.0)));
    draw(vcx);
    draw(vcx);
    let top = screen.read_with(vcx, |s, _| s.review_top());
    assert!(
        (50..300).contains(&top),
        "2,000 px of 18 px lines is about a hundred rows, not {top}"
    );
    assert!(
        painted(vcx, &format!("sticky-change-file-{path}")),
        "the bar of the file being read stays at the top"
    );
    // With the others folded, the cap's message is the last row of the list.
    screen.update(vcx, |s, cx| s.fold_all_but(path, cx));
    screen.read_with(vcx, |s, _| s.review_scroll_to_end());
    draw(vcx);
    draw(vcx);
    assert!(painted(vcx, "patch-cut"), "the cut message ends the file");
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

fn push_events(
    vcx: &mut VisualTestContext,
    store: &Entity<Store>,
    run: &reno_proto::RunId,
    events: impl IntoIterator<Item = reno_proto::ExecEvent>,
) {
    store.update(vcx, |s, cx| {
        for event in events {
            let seq = s
                .state()
                .transcript(run)
                .map_or(reno_proto::Seq(1), reno_client::Transcript::resume_from);
            s.apply(
                reno_client::Update::RunEvent {
                    run: run.clone(),
                    seq,
                    event,
                },
                cx,
            );
        }
    });
    draw(vcx);
}

fn tool(
    id: &str,
    name: &str,
    class: reno_proto::ActionClass,
    input: serde_json::Value,
) -> reno_proto::ExecEvent {
    reno_proto::ExecEvent::Tool {
        id: id.to_owned(),
        call: reno_proto::ToolCall::new(name, input, class),
    }
}

fn tool_result(id: &str, text: &str) -> reno_proto::ExecEvent {
    reno_proto::ExecEvent::ToolResult {
        id: id.to_owned(),
        ok: true,
        summary: None,
        output: Some(reno_proto::ToolOutput {
            text: text.to_owned(),
            truncated: false,
            bytes: text.len() as u64,
        }),
    }
}

#[gpui::test]
fn a_new_edit_or_command_opens_inside_its_group_while_live(cx: &mut TestAppContext) {
    use reno_proto::ActionClass::{Command, Edit, Read};
    use serde_json::json;

    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    let screen = open(vcx, &root, "builder1");
    let run = run_id("builder1");
    wait_drawn(vcx, |vcx| painted(vcx, "thinking"));
    assert!(screen.read_with(vcx, |s, _| s.is_following_tail()));

    push_events(
        vcx,
        &store,
        &run,
        [
            tool("quiet-read", "read", Read, json!({"path": "src/lease.rs"})),
            tool_result("quiet-read", "lease contents"),
            tool(
                "expand-no-diff",
                "edit",
                Edit,
                json!({"file_path": "src/lib.rs"}),
            ),
            tool(
                "expand-edit",
                "edit",
                Edit,
                json!({"file_path": "src/lease.rs", "old_string": "before", "new_string": "after"}),
            ),
            tool(
                "expand-command",
                "bash",
                Command,
                json!({"command": "cargo test"}),
            ),
        ],
    );
    assert!(painted(vcx, "step-quiet-read"));
    assert!(!painted(vcx, "tool-output-quiet-read"));
    assert!(!painted(vcx, "tool-detail-expand-no-diff"));
    assert!(
        painted(vcx, "tool-detail-expand-edit"),
        "the pending edit's diff is open"
    );
    assert!(
        painted(vcx, "tool-detail-expand-command"),
        "the pending command is open"
    );
    let bounds = |vcx: &mut VisualTestContext, id| vcx.debug_bounds(id).unwrap();
    assert!(
        bounds(vcx, "step-edits").bottom() <= bounds(vcx, "tool-detail-expand-edit").top()
            && bounds(vcx, "tool-detail-expand-edit").bottom()
                <= bounds(vcx, "step-expand-command").top()
            && bounds(vcx, "step-expand-command").bottom()
                <= bounds(vcx, "tool-detail-expand-command").top()
    );
    screen.read_with(vcx, |s, _| {
        assert!(
            s.is_following_tail(),
            "automatic growth follows the live tail"
        );
        assert!(matches!(s.rows().last(), Some(Row::Tools { calls, .. }) if
            calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().ends_with(
                &["quiet-read", "expand-no-diff", "expand-edit", "expand-command"]
            )
        ));
    });

    click(vcx, "step-expand-command");
    push_events(
        vcx,
        &store,
        &run,
        [tool_result("expand-command", "command output")],
    );
    assert!(!painted(vcx, "tool-detail-expand-command"));
    assert!(
        !painted(vcx, "tool-output-expand-command"),
        "a result respects the owner's close"
    );
    click(vcx, "step-expand-command");
    assert!(
        painted(vcx, "tool-output-expand-command"),
        "the owner can reopen it"
    );
}

#[gpui::test]
fn turning_the_settings_off_keeps_them_collapsed(cx: &mut TestAppContext) {
    use reno_proto::{ActionClass, ExecEvent};
    use serde_json::json;

    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    open(vcx, &root, "builder1");
    let run = run_id("builder1");
    wait_drawn(vcx, |vcx| painted(vcx, "thinking"));
    let command = |id| tool(id, "bash", ActionClass::Command, json!({"command": "pwd"}));
    push_events(vcx, &store, &run, [command("already-expanded")]);
    assert!(painted(vcx, "tool-detail-already-expanded"));

    vcx.update(|_, cx| {
        crate::prefs::update(cx, |p| {
            p.expand_new_edits = false;
            p.expand_new_commands = false;
        });
    });
    push_events(
        vcx,
        &store,
        &run,
        [
            tool(
                "expand-off-edit",
                "apply_patch",
                ActionClass::Edit,
                json!({"patch": "*** Update File: src/lease.rs\n@@\n-before\n+after"}),
            ),
            command("expand-off-command"),
        ],
    );
    assert!(painted(vcx, "tool-detail-already-expanded"));
    for id in ["expand-off-edit", "expand-off-command"] {
        assert!(painted(vcx, &format!("step-{id}")));
        assert!(!painted(vcx, &format!("tool-detail-{id}")));
    }

    click(vcx, "step-expand-off-edit");
    assert!(painted(vcx, "tool-detail-expand-off-edit"));
    click(vcx, "step-expand-off-edit");
    vcx.update(|_, cx| {
        crate::prefs::update(cx, |p| p.expand_new_commands = true);
    });
    push_events(
        vcx,
        &store,
        &run,
        [
            ExecEvent::Text {
                text: "Checking again.".to_owned(),
            },
            command("expand-next-command"),
        ],
    );
    assert!(!painted(vcx, "tool-detail-expand-off-command"));
    assert!(!painted(vcx, "tool-detail-expand-off-edit"));
    assert!(painted(vcx, "tool-detail-expand-next-command"));
}

#[gpui::test]
fn expanding_a_live_call_keeps_the_same_id_in_an_older_run_collapsed(cx: &mut TestAppContext) {
    use reno_proto::{ActionClass, ExecEvent};
    use serde_json::json;

    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    let old = run_id("builder0");
    let live = run_id("builder1");
    let old_screen = open(vcx, &root, "builder0");
    wait_drawn(vcx, |vcx| {
        store.read_with(vcx, |s, _| {
            s.thread_of(&old).is_some()
                && s.thread_of(&live).is_some()
                && s.state().transcript(&old).is_some_and(|t| {
                    t.events
                        .last()
                        .is_some_and(|(_, event)| matches!(event, ExecEvent::Done { .. }))
                })
        })
    });
    let fold = format!("worked-{}", fold_key(vcx, &old_screen, "t2"));
    store.update(vcx, |s, cx| {
        let mut threads: Vec<_> = s.state().threads().into_iter().cloned().collect();
        let new_at = threads
            .iter()
            .position(|thread| thread.runs.iter().any(|entry| entry.run_id == live))
            .unwrap();
        let mut newer = threads.remove(new_at);
        let older = threads
            .iter_mut()
            .find(|thread| thread.runs.iter().any(|entry| entry.run_id == old))
            .unwrap();
        older.runs.append(&mut newer.runs);
        s.apply(reno_client::Update::Threads(threads), cx);
    });

    let screen = open(vcx, &root, "builder1");
    wait_drawn(vcx, |vcx| painted(vcx, "thinking"));
    let history = |vcx: &mut VisualTestContext| {
        screen.update(vcx, |s, cx| {
            let ix = s
                .rows()
                .iter()
                .position(|r| matches!(r, Row::Worked { .. }))
                .unwrap();
            s.scroll_to_row(ix, cx);
        });
        draw(vcx);
    };
    history(vcx);
    click(vcx, &fold);
    assert!(painted(vcx, "step-t2"));
    assert!(!painted(vcx, "tool-output-t2"));

    // The live group's item is outside the old group's overdraw range.
    let filler = (0..100).flat_map(|n| {
        [
            ExecEvent::Reasoning {
                text: format!("Consider option {n}."),
            },
            ExecEvent::Text {
                text: format!("Checked option {n}."),
            },
        ]
    });
    push_events(
        vcx,
        &store,
        &live,
        filler.chain([
            tool(
                "t2",
                "bash",
                ActionClass::Command,
                json!({"command":"cargo test --lib"}),
            ),
            tool_result("t2", "live result"),
        ]),
    );
    screen.update(vcx, |s, cx| s.jump_to_latest(cx));
    draw(vcx);
    assert!(screen.read_with(vcx, |s, _| s.is_following_tail()));
    assert!(
        painted(vcx, "tool-output-t2"),
        "the live call's result is open"
    );

    history(vcx);
    assert!(painted(vcx, "step-t2"));
    assert!(
        !painted(vcx, "tool-output-t2"),
        "the older result stayed collapsed"
    );
}

#[gpui::test]
fn reopening_a_live_run_leaves_its_loaded_calls_collapsed(cx: &mut TestAppContext) {
    use reno_proto::ActionClass;
    use serde_json::json;

    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    open(vcx, &root, "builder1");
    let run = run_id("builder1");
    wait_drawn(vcx, |vcx| painted(vcx, "thinking"));
    let command = |id| tool(id, "bash", ActionClass::Command, json!({"command": "pwd"}));
    push_events(vcx, &store, &run, [command("before-reopening")]);
    assert!(painted(vcx, "tool-detail-before-reopening"));

    open(vcx, &root, "builder0");
    open(vcx, &root, "builder1");
    assert!(painted(vcx, "step-before-reopening"));
    assert!(!painted(vcx, "tool-detail-before-reopening"));
    push_events(vcx, &store, &run, [command("after-reopening")]);
    assert!(!painted(vcx, "tool-detail-before-reopening"));
    assert!(painted(vcx, "tool-detail-after-reopening"));
}

#[gpui::test]
fn closing_find_leaves_an_automatically_opened_command_open(cx: &mut TestAppContext) {
    use reno_proto::ActionClass;
    use serde_json::json;

    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    let screen = open(vcx, &root, "builder1");
    let run = run_id("builder1");
    wait_drawn(vcx, |vcx| painted(vcx, "thinking"));
    let command = |id| tool(id, "bash", ActionClass::Command, json!({"command": "pwd"}));
    push_events(
        vcx,
        &store,
        &run,
        [command("kept"), tool_result("kept", "first output")],
    );
    vcx.update(|_, cx| crate::prefs::update(cx, |p| p.expand_new_commands = false));
    push_events(
        vcx,
        &store,
        &run,
        [command("find-result"), tool_result("find-result", "needle")],
    );
    assert!(painted(vcx, "tool-output-kept"));
    assert!(!painted(vcx, "tool-output-find-result"));

    screen.update_in(vcx, |s, window, cx| s.open_find(window, cx));
    draw(vcx);
    vcx.simulate_keystrokes("n e e d l e");
    draw(vcx);
    assert_eq!(
        screen.read_with(vcx, |s, _| s.find_count_label()),
        Some("1 of 1".to_owned())
    );
    screen.update(vcx, |s, cx| s.find_step(true, cx));
    draw(vcx);
    assert!(painted(vcx, "tool-output-find-result"));
    assert!(painted(vcx, "tool-output-kept"));

    screen.update_in(vcx, |s, window, cx| s.close_find(window, cx));
    draw(vcx);
    assert!(!painted(vcx, "tool-output-find-result"));
    assert!(painted(vcx, "tool-output-kept"));
}

fn tool_run_summary(
    vcx: &mut VisualTestContext,
    store: &Entity<Store>,
    run: &reno_proto::RunId,
    attempt: u32,
    status: RunStatus,
) {
    store.update(vcx, |s, cx| {
        let mut summary = s.state().run(run).unwrap().clone();
        summary.attempt = attempt;
        summary.status = status;
        s.apply(reno_client::Update::RunUpserted(summary), cx);
    });
    draw(vcx);
}

#[gpui::test]
fn closing_find_after_running_keeps_only_eligible_deferred_output_open(cx: &mut TestAppContext) {
    use reno_proto::ActionClass;
    use serde_json::json;

    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    let screen = open(vcx, &root, "architect1");
    let run = run_id("architect1");
    let cases = [
        ("find-before-running", true, "needle eligible"),
        ("find-while-disabled", false, "needle disabled"),
        ("find-manual-close", true, "needle closed"),
    ];
    for (id, expands, result) in cases {
        vcx.update(|_, cx| crate::prefs::update(cx, |p| p.expand_new_commands = expands));
        push_events(
            vcx,
            &store,
            &run,
            [
                tool(id, "bash", ActionClass::Command, json!({"command": "pwd"})),
                tool_result(id, result),
            ],
        );
        assert!(!painted(vcx, &format!("tool-output-{id}")));
    }

    screen.update_in(vcx, |s, window, cx| s.open_find(window, cx));
    draw(vcx);
    vcx.simulate_keystrokes("n e e d l e");
    draw(vcx);
    assert_eq!(
        screen.read_with(vcx, |s, _| s.find_count_label()),
        Some("1 of 3".to_owned())
    );
    for (id, _, _) in cases {
        screen.update(vcx, |s, cx| s.find_step(true, cx));
        draw(vcx);
        assert!(painted(vcx, &format!("tool-output-{id}")));
    }
    click(vcx, "step-find-manual-close");
    assert!(!painted(vcx, "tool-output-find-manual-close"));

    tool_run_summary(vcx, &store, &run, 1, RunStatus::Running);
    assert!(painted(vcx, "tool-output-find-before-running"));
    assert!(painted(vcx, "tool-output-find-while-disabled"));
    assert!(!painted(vcx, "tool-output-find-manual-close"));

    screen.update_in(vcx, |s, window, cx| s.close_find(window, cx));
    draw(vcx);
    assert!(
        painted(vcx, "tool-output-find-before-running"),
        "confirmed automatic disclosure survives closing Find"
    );
    assert!(!painted(vcx, "tool-output-find-while-disabled"));
    assert!(!painted(vcx, "tool-output-find-manual-close"));

    screen.update_in(vcx, |s, window, cx| s.open_find(window, cx));
    draw(vcx);
    vcx.simulate_keystrokes("space e l i g i b l e");
    draw(vcx);
    assert_eq!(
        screen.read_with(vcx, |s, _| s.find_count_label()),
        Some("1 of 1".to_owned())
    );
    screen.update(vcx, |s, cx| s.find_step(true, cx));
    screen.update_in(vcx, |s, window, cx| s.close_find(window, cx));
    draw(vcx);
    assert!(painted(vcx, "tool-output-find-before-running"));
    assert!(!painted(vcx, "tool-output-find-while-disabled"));
    assert!(!painted(vcx, "tool-output-find-manual-close"));
}

#[gpui::test]
fn a_new_command_waits_for_its_running_summary(cx: &mut TestAppContext) {
    use reno_proto::ActionClass;
    use serde_json::json;

    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    let screen = open(vcx, &root, "architect1");
    let run = run_id("architect1");
    let command = |id| tool(id, "bash", ActionClass::Command, json!({"command": "pwd"}));
    push_events(vcx, &store, &run, [command("before-running")]);
    assert!(painted(vcx, "step-before-running"));
    assert!(!painted(vcx, "tool-detail-before-running"));

    vcx.update(|_, cx| crate::prefs::update(cx, |p| p.expand_new_commands = false));
    push_events(vcx, &store, &run, [command("disabled-before-running")]);
    vcx.update(|_, cx| crate::prefs::update(cx, |p| p.expand_new_commands = true));
    push_events(vcx, &store, &run, [command("manual-before-running")]);
    click(vcx, "step-manual-before-running");
    assert!(painted(vcx, "tool-detail-manual-before-running"));
    click(vcx, "step-manual-before-running");
    assert!(!painted(vcx, "tool-detail-manual-before-running"));
    screen.update(vcx, |s, cx| s.jump_to_latest(cx));
    draw(vcx);

    tool_run_summary(vcx, &store, &run, 1, RunStatus::Running);
    assert!(
        painted(vcx, "tool-detail-before-running"),
        "the older event opens when Running is reported"
    );
    assert!(!painted(vcx, "tool-detail-disabled-before-running"));
    assert!(!painted(vcx, "tool-detail-manual-before-running"));
    assert!(screen.read_with(vcx, |s, _| s.is_following_tail()));
}

#[gpui::test]
fn a_resumed_command_waits_for_its_running_summary(cx: &mut TestAppContext) {
    use reno_proto::{ActionClass, ExecEvent, SessionRef, StopReason};
    use serde_json::json;

    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    let screen = open(vcx, &root, "builder1");
    let run = run_id("builder1");
    wait_drawn(vcx, |vcx| painted(vcx, "thinking"));
    tool_run_summary(vcx, &store, &run, 1, RunStatus::Interrupted);
    push_events(
        vcx,
        &store,
        &run,
        [
            tool(
                "ended-command",
                "bash",
                ActionClass::Command,
                json!({"command":"ls"}),
            ),
            ExecEvent::Done {
                stop: StopReason::Interrupted,
                diagnostic: None,
                result: None,
                reason: None,
            },
            ExecEvent::Resumed {
                from_attempt: 1,
                session: SessionRef("resumed".to_owned()),
            },
            tool(
                "resumed-command",
                "bash",
                ActionClass::Command,
                json!({"command":"pwd"}),
            ),
        ],
    );
    assert!(painted(vcx, "step-resumed-command"));
    assert!(!painted(vcx, "tool-detail-resumed-command"));

    tool_run_summary(vcx, &store, &run, 2, RunStatus::Running);
    assert!(
        painted(vcx, "tool-detail-resumed-command"),
        "the resumed attempt opens when its own summary arrives"
    );
    assert!(!painted(vcx, "tool-detail-ended-command"));
    assert!(screen.read_with(vcx, |s, _| s.is_following_tail()));
}

fn tool_text_at(
    vcx: &mut VisualTestContext,
    screen: &Entity<RunScreen>,
    selector: &'static str,
    text: &str,
    off: usize,
) -> gpui::Point<gpui::Pixels> {
    let bounds = vcx
        .debug_bounds(selector)
        .expect("the tool detail is visible");
    screen
        .read_with(vcx, |s, _| {
            s.select.registry.borrow().iter().find_map(|piece| {
                if piece.bounds.top() < bounds.top() || piece.bounds.bottom() > bounds.bottom() {
                    return None;
                }
                let at = piece.layout.text().find(text)? + off;
                let origin = piece.layout.position_for_index(at)?;
                Some(origin + gpui::point(px(1.0), piece.layout.line_height() / 2.0))
            })
        })
        .unwrap_or_else(|| panic!("tool text {text:?} in {selector} participates in selection"))
}

fn drag_tool_text(
    vcx: &mut VisualTestContext,
    from: gpui::Point<gpui::Pixels>,
    to: gpui::Point<gpui::Pixels>,
) {
    use gpui::{Modifiers, MouseButton, point};

    vcx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_move(
        point(from.x + px(8.0), from.y + px(8.0)),
        MouseButton::Left,
        Modifiers::none(),
    );
    draw(vcx);
    vcx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    vcx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    draw(vcx);
}

#[gpui::test]
fn selecting_tool_details_copies_the_unicode_file_labels_and_bodies_in_order(
    cx: &mut TestAppContext,
) {
    use reno_proto::{ActionClass, ExecEvent};
    use serde_json::json;

    cx.update(super::bind_keys);
    let store = fake_store(cx);
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    let screen = open(vcx, &root, "builder1");
    let run = run_id("builder1");
    wait_drawn(vcx, |vcx| painted(vcx, "thinking"));
    let command = "printf 'λ command'";
    push_events(
        vcx,
        &store,
        &run,
        [
            tool(
                "copy-command",
                "bash",
                ActionClass::Command,
                json!({"command":command}),
            ),
            tool(
                "copy-edit",
                "edit",
                ActionClass::Edit,
                json!({
                    "file_path":"src/é.rs","old_string":"old","new_string":"déjà λx"
                }),
            ),
            tool(
                "copy-next-edit",
                "edit",
                ActionClass::Edit,
                json!({
                    "file_path":"src/次.rs","old_string":"before","new_string":"after"
                }),
            ),
            ExecEvent::Text {
                text: "Following **entry** begins here.".to_owned(),
            },
        ],
    );

    let from = tool_text_at(vcx, &screen, "tool-detail-copy-edit", "déjà λx", 1);
    let to = tool_text_at(
        vcx,
        &screen,
        "tool-detail-copy-edit",
        "déjà λx",
        "déjà λ".len(),
    );
    drag_tool_text(vcx, from, to);
    vcx.simulate_keystrokes("cmd-c");
    assert_eq!(clipboard(vcx), "éjà λ");
    let anchors = screen.read_with(vcx, |s, cx| s.selection_anchors(cx));
    let prefix = "- old\n+ ".len();
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0].range, Some(prefix + 1..prefix + "déjà λ".len()));

    let from = tool_text_at(
        vcx,
        &screen,
        "tool-detail-copy-command",
        command,
        "printf '".len(),
    );
    let to = screen
        .read_with(vcx, |s, _| s.painted_point(" begins here.", 7))
        .expect("next reply");
    drag_tool_text(vcx, from, to);
    vcx.simulate_keystrokes("cmd-c");
    let seen = clipboard(vcx);
    assert!(seen.starts_with("λ command'\n"), "{seen:?}");
    assert!(
        seen.contains("src/é.rs\n- old\n+ déjà λx\nsrc/次.rs\n- before\n+ after"),
        "{seen:?}"
    );
    assert!(seen.ends_with("\n\nFollowing entry begins"), "{seen:?}");

    screen.update(vcx, |s, cx| s.copy_markdown(cx));
    let source = clipboard(vcx);
    assert!(source.starts_with("λ command'\n"), "{source:?}");
    assert!(
        source.contains("src/é.rs\n- old\n+ déjà λx\nsrc/次.rs\n- before\n+ after"),
        "{source:?}"
    );
    assert!(
        source.ends_with("\n\nFollowing **entry** begins"),
        "{source:?}"
    );

    let anchors = screen.read_with(vcx, |s, cx| s.selection_anchors(cx));
    push_events(
        vcx,
        &store,
        &run,
        [tool_result("copy-command", "command result")],
    );
    assert_eq!(
        screen.read_with(vcx, |s, cx| s.selection_anchors(cx)),
        anchors
    );
    assert!(painted(vcx, "tool-output-copy-command"));
    screen.update(vcx, |s, cx| s.copy_markdown(cx));
    assert_eq!(
        clipboard(vcx),
        source,
        "the input anchor survives the command result"
    );
}

fn render_tool_cards(
    cx: &mut TestAppContext,
) -> (
    Entity<Store>,
    Entity<RootView>,
    &mut VisualTestContext,
    Entity<RunScreen>,
) {
    let store = store_with(
        cx,
        FakeConfig {
            transcript_threads: true,
            ..FakeConfig::default()
        },
    );
    connected(cx, &store);
    let (root, vcx) = window(cx, &store, Appearance::Dark);
    let screen = open(vcx, &root, reno_client::fake::transcript::RENDER2_RUN);
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |screen, _| {
            matches!(screen.rows().last(), Some(Row::Done { .. }))
        })
    });
    (store, root, vcx, screen)
}

fn show_render_tool(vcx: &mut VisualTestContext, screen: &Entity<RunScreen>, id: &str) {
    let fold = crate::transcript::Key(fold_key(vcx, screen, id));
    screen.update(vcx, |screen, cx| {
        screen.toggle(fold, cx);
        let top = screen
            .rows()
            .iter()
            .position(|row| row.key() == fold)
            .unwrap();
        screen.scroll_to_row(top, cx);
    });
    wait_drawn(vcx, |vcx| painted(vcx, &format!("step-{id}")));
    click(vcx, &format!("step-{id}"));
}

#[gpui::test]
fn a_one_file_edit_shows_its_badge_go_to_file_and_inline_diff(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = render_tool_cards(cx);
    show_render_tool(vcx, &screen, "e1");
    assert!(painted(vcx, "edit-badge-e1"));
    assert!(painted(vcx, "edit-diff-e1"));
    click(vcx, "edit-file-e1");
    wait_drawn(vcx, |vcx| {
        screen.read_with(vcx, |s, _| {
            s.files_model()
                .and_then(|files| files.doc())
                .is_some_and(|doc| doc.path == "src/supervisor.rs")
        })
    });
}

#[gpui::test]
fn a_patch_shows_one_hunk_block_per_file(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = render_tool_cards(cx);
    show_render_tool(vcx, &screen, "p1");
    assert!(painted(vcx, "patch-file-p1-0"));
    assert!(painted(vcx, "patch-file-p1-1"));
    assert!(!painted(vcx, "patch-file-p1-2"));
}

#[gpui::test]
fn a_restored_call_shows_no_elapsed_time(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = render_tool_cards(cx);
    show_render_tool(vcx, &screen, "b1");
    assert!(painted(vcx, "command-status-b1"));
    assert!(!painted(vcx, "command-elapsed-b1"));
}

#[gpui::test]
fn a_numbered_read_shows_as_a_code_block_and_raw_input_opens(cx: &mut TestAppContext) {
    let (_store, _root, vcx, screen) = render_tool_cards(cx);
    show_render_tool(vcx, &screen, "r1");
    assert!(painted(vcx, "read-numbered-r1"));
    click(vcx, "output-copy-r1");
    assert!(clipboard(vcx).starts_with("    38\t"));
    click(vcx, "raw-input-r1");
    assert!(painted(vcx, "raw-input-body-r1"));
}

fn show_watched_command(
    store: &Entity<Store>,
    vcx: &mut VisualTestContext,
    run: &reno_proto::RunId,
) {
    use reno_proto::{ActionClass, ExecEvent, SessionRef};
    use std::time::Duration;

    tool_run_summary(vcx, store, run, 4, RunStatus::Queued);
    store.update(vcx, |store, _| store.confirm_tool_watch_for_test(run));
    push_events(
        vcx,
        store,
        run,
        [ExecEvent::Resumed {
            from_attempt: 3,
            session: SessionRef("watched-session".into()),
        }],
    );
    tool_run_summary(vcx, store, run, 4, RunStatus::Running);
    push_events(
        vcx,
        store,
        run,
        [tool(
            "watched-command",
            "bash",
            ActionClass::Command,
            serde_json::json!({"command":"pwd"}),
        )],
    );
    vcx.executor().advance_clock(Duration::from_millis(1200));
    push_events(
        vcx,
        store,
        run,
        [tool_result("watched-command", "/home/fake/workspace")],
    );
}

#[gpui::test]
fn a_command_shows_copy_and_status_and_a_watched_call_its_elapsed_time(cx: &mut TestAppContext) {
    let (store, _root, vcx, _screen) = render_tool_cards(cx);
    let run = run_id(reno_client::fake::transcript::RENDER2_RUN);
    show_watched_command(&store, vcx, &run);
    assert!(painted(vcx, "command-status-watched-command"));
    assert!(painted(vcx, "command-elapsed-watched-command"));
    click(vcx, "command-copy-watched-command");
    assert_eq!(clipboard(vcx), "pwd");
}

#[gpui::test]
fn the_elapsed_time_survives_leaving_and_reopening_the_thread(cx: &mut TestAppContext) {
    let (store, root, vcx, _screen) = render_tool_cards(cx);
    let run = reno_client::fake::transcript::RENDER2_RUN;
    show_watched_command(&store, vcx, &run_id(run));
    open(vcx, &root, "builder1");
    open(vcx, &root, run);
    wait_drawn(vcx, |vcx| painted(vcx, "step-watched-command"));
    click(vcx, "step-watched-command");
    assert!(painted(vcx, "command-elapsed-watched-command"));
}

#[gpui::test]
fn a_run_seen_queueing_with_no_open_journal_does_not_time_cold_replay(cx: &mut TestAppContext) {
    use reno_proto::{ActionClass, ExecEvent, SessionRef, StopReason};

    let (store, root, vcx, _screen) = render_tool_cards(cx);
    let run_name = reno_client::fake::transcript::RENDER2_RUN;
    let run = run_id(run_name);
    open(vcx, &root, "builder1");
    tool_run_summary(vcx, &store, &run, 4, RunStatus::Queued);
    tool_run_summary(vcx, &store, &run, 4, RunStatus::Running);
    // The restored event feed can arrive after the runs feed has already confirmed completion.
    tool_run_summary(vcx, &store, &run, 4, RunStatus::Succeeded);
    open(vcx, &root, run_name);
    push_events(
        vcx,
        &store,
        &run,
        [
            ExecEvent::Resumed {
                from_attempt: 3,
                session: SessionRef("cold".into()),
            },
            tool(
                "cold-command",
                "bash",
                ActionClass::Command,
                serde_json::json!({"command":"pwd"}),
            ),
            tool_result("cold-command", "/home/fake/workspace"),
            ExecEvent::Done {
                stop: StopReason::Completed,
                result: None,
                diagnostic: None,
                reason: None,
            },
        ],
    );
    assert!(store.read_with(vcx, |store, _| {
        store.watched(&run, "cold-command").is_none()
    }));
}
