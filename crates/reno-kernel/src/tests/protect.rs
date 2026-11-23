#![cfg(test)]

//! Protected paths enforced after the fact (see `supervisor::protect`): a run whose harness
//! cannot be asked writes `.reno/policy.toml`, and the supervisor reverts it, audits it and
//! stops the run. The worktree is the `FakeWorkspace`: a test says what the run left in it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reno_harness::exec::{MockExecutor, MockStep};
use reno_proto::defs::{DefKind, PROTECTED_RULE};
use reno_proto::{
    ActionClass, ApprovalOutcome, ExecEvent, NotResumed, ResolvedBy, RunId, RunStatus,
    SandboxLevel, Seq, StopDetail, StopReason, ToolCall,
};

use super::approvals::resolved;
use super::defs::{FakePort, file, setup};
use super::*;
use crate::FakeWorkspace;

fn write(id: &str, path: &str) -> [MockStep; 2] {
    [
        MockStep::Emit(ExecEvent::Tool {
            id: id.into(),
            call: ToolCall::new(
                "Write",
                serde_json::json!({ "file_path": path }),
                ActionClass::Edit,
            ),
        }),
        MockStep::Emit(ExecEvent::ToolResult {
            id: id.into(),
            ok: true,
            summary: None,
            output: None,
        }),
    ]
}

fn steps(parts: impl IntoIterator<Item = [MockStep; 2]>) -> Vec<MockStep> {
    let mut steps = vec![
        MockStep::started("s", "mock-1"),
        MockStep::Sleep(Duration::from_secs(5)),
    ];
    steps.extend(parts.into_iter().flatten());
    steps.push(MockStep::Sleep(Duration::from_secs(600)));
    steps.push(MockStep::text("never reached"));
    steps
}

async fn run_in(fx: &Fx, cwd: &str) -> RunId {
    fx.kernel
        .submit("work", id("mock"), PathBuf::from(cwd), None)
        .await
        .unwrap()
}

async fn placed(ws: &FakeWorkspace, run: &RunId) {
    until(async || ws.exists(run).then_some(())).await;
}

async fn ended(fx: &Fx, run: &RunId, status: RunStatus) -> Vec<(Seq, ExecEvent)> {
    fx.wait_status(run, status).await;
    fx.journal_with_done(run).await
}

fn done(journal: &[(Seq, ExecEvent)]) -> (StopReason, Option<StopDetail>, Option<String>) {
    match journal.last() {
        Some((
            _,
            ExecEvent::Done {
                stop,
                reason,
                diagnostic,
                ..
            },
        )) => (*stop, reason.clone(), diagnostic.clone()),
        other => panic!("not a Done: {other:?}"),
    }
}

async fn rig(mock: &MockExecutor, ws: &Arc<FakeWorkspace>, sandbox: SandboxLevel) -> Fx {
    let cfg = KernelConfig {
        sandbox,
        ..config(1)
    };
    Fx::with_workspace(cfg, registry(std::slice::from_ref(mock)), None, ws.clone()).await
}

#[tokio::test(start_paused = true)]
async fn a_write_to_a_protected_path_is_reverted_audited_and_stops_the_run() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock(steps([
        write("t1", "src/a.rs"),
        write("t2", ".reno/policy.toml"),
    ]));
    let fx = rig(&m, &ws, SandboxLevel::Unrestricted).await;
    let run = run_in(&fx, "/repo").await;
    placed(&ws, &run).await;
    // The run wrote an ordinary file and a protected one.
    ws.plant_protected(&run, &[".reno/policy.toml", "src/a.rs"]);

    let journal = ended(&fx, &run, RunStatus::Interrupted).await;
    let (stop, reason, diagnostic) = done(&journal);
    assert_eq!(stop, StopReason::Interrupted);
    let detail = StopDetail::ProtectedPathWrite {
        paths: vec![".reno/policy.toml".into()],
        reverted: true,
    };
    assert_eq!(reason, Some(detail.clone()));
    assert_eq!(diagnostic, Some(detail.to_string()));
    assert!(
        detail
            .to_string()
            .starts_with("blocked: .reno/policy.toml is protected")
    );

    // Only the protected path was put back; the ordinary change stays for the owner.
    assert_eq!(
        ws.reverts(),
        [(run.clone(), vec![".reno/policy.toml".to_owned()])]
    );
    assert_eq!(ws.protected_left(&run), ["src/a.rs"]);

    // The journal and the approval history both say it was refused.
    let blocked = resolved(&journal);
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0].1, ApprovalOutcome::Denied);
    assert_eq!(
        blocked[0].2,
        ResolvedBy::Rule {
            name: PROTECTED_RULE.into()
        }
    );
    assert!(
        blocked[0]
            .3
            .as_deref()
            .unwrap()
            .starts_with("blocked: `.reno/policy.toml` is protected")
    );
    let history = fx.kernel.approval_history(Some(&run), None).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].subject.paths,
        [PathBuf::from(".reno/policy.toml")]
    );
    // A run stopped for this is never resumed on its own.
    assert_eq!(
        detail.forbids_auto_resume(),
        Some(NotResumed::ProtectedPathWrite)
    );
}

#[tokio::test(start_paused = true)]
async fn a_write_made_after_the_last_tool_is_caught_when_the_run_ends() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    // A harness that finishes by itself: a shell write no tool result followed.
    let m = mock([
        MockStep::started("s", "mock-1"),
        MockStep::Sleep(Duration::from_secs(5)),
        MockStep::done(StopReason::Completed),
    ]);
    let fx = rig(&m, &ws, SandboxLevel::Unrestricted).await;
    let run = run_in(&fx, "/repo").await;
    placed(&ws, &run).await;
    ws.plant_protected(&run, &[".git/hooks/pre-commit"]);

    let journal = ended(&fx, &run, RunStatus::Interrupted).await;
    let (stop, reason, _) = done(&journal);
    assert_eq!(stop, StopReason::Interrupted);
    assert!(matches!(
        reason,
        Some(StopDetail::ProtectedPathWrite { reverted: true, .. })
    ));
    assert!(ws.protected_left(&run).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_run_that_touches_no_protected_path_ends_normally() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let [call, result] = write("t1", "src/a.rs");
    let m = mock([
        MockStep::started("s", "mock-1"),
        MockStep::Sleep(Duration::from_secs(5)),
        call,
        result,
        MockStep::done(StopReason::Completed),
    ]);
    let fx = rig(&m, &ws, SandboxLevel::Unrestricted).await;
    let run = run_in(&fx, "/repo").await;
    placed(&ws, &run).await;
    ws.plant_protected(&run, &["src/a.rs"]);

    let journal = ended(&fx, &run, RunStatus::Succeeded).await;
    let (stop, reason, _) = done(&journal);
    assert_eq!((stop, reason), (StopReason::Completed, None));
    assert!(ws.reverts().is_empty());
    assert_eq!(ws.protected_left(&run), ["src/a.rs"]);
    assert!(resolved(&journal).is_empty());
}

/// A kernel whose bot `loose` may write in place, over a directory that is no repository.
async fn opted_in_rig(mock: &MockExecutor, ws: &Arc<FakeWorkspace>) -> Fx {
    let port = FakePort::with(vec![file(
        DefKind::Bot,
        "loose",
        "name = \"loose\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\nin_place_writes = true\n",
    )]);
    Fx::with_workspace(
        config(1),
        registry(std::slice::from_ref(mock)),
        Some(setup(&port)),
        ws.clone(),
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn a_write_outside_the_worktree_that_the_worktree_cannot_show_still_stops_the_run() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock(steps([
        write("t1", "src/a.rs"),
        write("t2", "/home/owner/.reno/policy.toml"),
    ]));
    let fx = rig(&m, &ws, SandboxLevel::Unrestricted).await;
    let run = run_in(&fx, "/repo").await;
    let journal = ended(&fx, &run, RunStatus::Interrupted).await;
    let (_, reason, _) = done(&journal);
    assert_eq!(
        reason,
        Some(StopDetail::ProtectedPathWrite {
            paths: vec!["/home/owner/.reno/policy.toml".into()],
            reverted: false
        })
    );
    assert!(ws.reverts().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_in_place_run_is_stopped_and_nothing_is_deleted() {
    let ws = Arc::new(FakeWorkspace::new());
    let m = mock(steps([
        write("t1", "src/a.rs"),
        write("t2", ".reno/policy.toml"),
    ]));
    let fx = opted_in_rig(&m, &ws).await;
    let run = fx
        .kernel
        .submit_for_bot("loose", "go", None, PathBuf::from("/plain"), None)
        .await
        .unwrap();
    ws.plant_protected(&run, &[".reno/policy.toml"]);

    let journal = ended(&fx, &run, RunStatus::Interrupted).await;
    let (_, reason, _) = done(&journal);
    assert_eq!(
        reason,
        Some(StopDetail::ProtectedPathWrite {
            paths: vec![".reno/policy.toml".into()],
            reverted: false
        })
    );
    // No worktree, so there was nothing of Reno's to revert, and the user's files were not
    // touched.
    assert!(ws.reverts().is_empty());
    assert_eq!(ws.protected_left(&run), [".reno/policy.toml"]);
    assert_eq!(resolved(&journal).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_in_place_write_that_failed_does_not_stop_the_run() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let [call, _] = write("t1", ".reno/policy.toml");
    let m = mock([
        MockStep::started("s", "mock-1"),
        call,
        // A supervised harness whose request the approval step denied: the tool failed.
        MockStep::Emit(ExecEvent::ToolResult {
            id: "t1".into(),
            ok: false,
            summary: None,
            output: None,
        }),
        MockStep::done(StopReason::Completed),
    ]);
    let fx = rig(&m, &ws, SandboxLevel::ReadOnly).await;
    let run = run_in(&fx, "/repo").await;
    let journal = ended(&fx, &run, RunStatus::Succeeded).await;
    assert_eq!(done(&journal).1, None);
}

/// Project config of every harness and provider, which a run may edit.
const HARNESS_CONFIG: [&str; 8] = [
    ".claude/settings.json",
    ".claude/skills/x/SKILL.md",
    ".codex/config.toml",
    ".mcp.json",
    ".pi/extensions/e.ts",
    "opencode.json",
    ".cursor/rules/a.mdc",
    ".grok/settings.json",
];

#[tokio::test(start_paused = true)]
async fn writing_harness_config_in_a_worktree_run_does_not_stop_it() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock(finishing(HARNESS_CONFIG.map(|path| write(path, path))));
    let fx = rig(&m, &ws, SandboxLevel::Unrestricted).await;
    let run = run_in(&fx, "/repo").await;
    let journal = ended(&fx, &run, RunStatus::Succeeded).await;
    assert_eq!(done(&journal).1, None);
    assert!(ws.reverts().is_empty());
    assert!(resolved(&journal).is_empty());
}

#[tokio::test(start_paused = true)]
async fn writing_harness_config_in_an_in_place_run_does_not_stop_it() {
    let ws = Arc::new(FakeWorkspace::new());
    let m = mock(finishing(HARNESS_CONFIG.map(|path| write(path, path))));
    let fx = opted_in_rig(&m, &ws).await;
    let run = fx
        .kernel
        .submit_for_bot("loose", "go", None, PathBuf::from("/plain"), None)
        .await
        .unwrap();
    let journal = ended(&fx, &run, RunStatus::Succeeded).await;
    assert_eq!(done(&journal).1, None);
    assert!(ws.reverts().is_empty());
    assert!(resolved(&journal).is_empty());
}

fn finishing(parts: impl IntoIterator<Item = [MockStep; 2]>) -> Vec<MockStep> {
    let mut steps = vec![
        MockStep::started("s", "mock-1"),
        MockStep::Sleep(Duration::from_secs(5)),
    ];
    steps.extend(parts.into_iter().flatten());
    steps.push(MockStep::done(StopReason::Completed));
    steps
}

// ---- the built-in protection and the calls Pi's gate raises ---------------------------------
//
// The subjects have the shape `subject()` gives a gate call: the path as Pi will open it
// (`@` dropped, a `file:` URL decoded, `~` expanded with the daemon user's home), and for a
// refused URL both the decoded path and the text as written.

mod gate_calls {
    use reno_proto::ApprovalSubject;

    use super::super::approvals::{MODES, gate_bash, gate_call, gate_edit, reno_rig, script};
    use super::*;

    const DATA: &str = "/Users/u/Library/Application Support/Reno";
    const SCRATCH: &str = "/tmp/reno-scratch-1";

    /// A policy that allows everything the protection does not refuse, rule by rule, so a
    /// protected call is also one an `allow` rule names.
    const ALLOW_ALL: &str = r#"
default = "allow"
[[rules]]
note = "edits are fine"
effect = "allow"
match = { action = "edit" }
[[rules]]
note = "commands are fine"
effect = "allow"
match = { action = "command", command = "*" }
"#;

    fn edit_of(paths: &[&str]) -> ApprovalSubject {
        let mut subject = gate_edit("c1", paths[0]);
        subject.paths = paths.iter().map(Into::into).collect();
        subject
    }

    /// Run each subject as one call of a `reno` run and say what the ladder answered, in order.
    async fn judged(
        extra: &str,
        policy: &str,
        subjects: &[ApprovalSubject],
    ) -> Vec<(ApprovalOutcome, ResolvedBy, Option<String>)> {
        let calls = subjects
            .iter()
            .enumerate()
            .map(|(n, subject)| (format!("r{n}"), subject.clone()))
            .collect::<Vec<_>>();
        let steps = script(
            calls
                .iter()
                .map(|(id, subject)| (id.as_str(), subject.clone()))
                .collect(),
        );
        let rig = reno_rig(policy, extra, steps).await;
        let run = rig.run_as("keeper").await;
        rig.fx.wait_status(&run, RunStatus::Succeeded).await;
        assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
        let journal = rig.fx.journal_with_done(&run).await;
        let got = resolved(&journal);
        assert_eq!(got.len(), subjects.len(), "{got:?}");
        got.into_iter()
            .map(|(_, outcome, by, message)| (outcome, by, message))
            .collect()
    }

    fn protected() -> ResolvedBy {
        ResolvedBy::Rule {
            name: PROTECTED_RULE.into(),
        }
    }

    fn rows_of_edits() -> Vec<(&'static str, ApprovalSubject)> {
        let edit = |paths: &[&str]| edit_of(paths);
        vec![
            (
                "the gate extension",
                edit(&[&format!("{SCRATCH}/.reno/gate.mjs")]),
            ),
            (
                "a Pi session file in the data directory",
                edit(&[&format!("{DATA}/harness/.reno/pi-sessions/x/s.jsonl")]),
            ),
            (
                "the data directory itself",
                edit(&[&format!("{DATA}/reno.db")]),
            ),
            ("git config, relative", edit(&[".git/config"])),
            ("git config, absolute", edit(&["/work/tree/.git/config"])),
            (
                "a dot-dot that lands in .reno",
                edit(&["src/../.reno/policy.toml"]),
            ),
            (
                "a dot-dot that lands in .reno, absolute",
                edit(&["/work/tree/a/../.reno/x"]),
            ),
            (
                "a dot-dot that leaves and enters a repository",
                edit(&["/work/tree/x/../../outside/.git/hooks/pre-push"]),
            ),
            ("a dot-dot above the root", edit(&["/../.reno/x"])),
            ("a dot-dot between the two", edit(&[".reno/../.git/config"])),
            ("a current-directory dot", edit(&["./.reno/./x"])),
            ("doubled and trailing separators", edit(&["a//.reno//x/"])),
            (".RENO", edit(&[".RENO/policy.toml"])),
            (".rEnO", edit(&["x/.rEnO/defs/bot.toml"])),
            (".Git", edit(&[".Git/hooks/pre-commit"])),
            (".GIT, absolute", edit(&["/work/tree/a/../.GIT/config"])),
            (
                "the data directory in capitals",
                edit(&["/Users/u/LIBRARY/APPLICATION SUPPORT/RENO/harness/x"]),
            ),
            (
                "the data directory in lower case",
                edit(&["/Users/u/library/application support/reno/reno.db"]),
            ),
            (
                "a path under the home Pi expanded for ~",
                edit(&["/Users/u/.reno/policy.toml"]),
            ),
            (
                "a ~user path, which Pi does not expand",
                edit(&["~x/.reno/policy.toml"]),
            ),
            (
                "a dot-dot that leaves the worktree into the data directory",
                edit(&["../../Users/u/Library/Application Support/Reno/reno.db"]),
            ),
            (
                "a refused URL: the decoded path first",
                edit(&["/work/a/.reno/x", "file:///work/a%2F.reno/x"]),
            ),
            // Defensive rows: `subject()` puts the decoded path first, but the protection judges
            // every path, whatever its place.
            (
                "a refused URL: the text as written first",
                edit(&["file:///work/a%2F.reno/x", "/work/a/.reno/x"]),
            ),
            (
                "a refused URL naming another host",
                edit(&["/work/.git/x", "file://other/work/.git/x"]),
            ),
            (
                "a protected path among several (defensive: every path is judged)",
                edit(&["src/lib.rs", "docs/a.md", ".reno/x"]),
            ),
        ]
    }

    fn rows_of_commands() -> Vec<(&'static str, ApprovalSubject)> {
        let bash = |line: &str| gate_bash("c1", line);
        vec![
            (
                "rm of the gate extension",
                bash(&format!("rm {SCRATCH}/.reno/gate.mjs")),
            ),
            (
                "rm of a session file in the data directory",
                bash(&format!(
                    "rm \"{DATA}/harness/.reno/pi-sessions/x/s.jsonl\""
                )),
            ),
            (
                "a redirect onto a session file",
                bash(&format!(
                    "echo x > \"{DATA}/harness/.reno/pi-sessions/x/s.jsonl\""
                )),
            ),
            (
                "an appending redirect onto a session file",
                bash(&format!(
                    "echo x >> {DATA_ESCAPED}/harness/.reno/pi-sessions/x/s.jsonl"
                )),
            ),
            (
                "rm of the store",
                bash(&format!("rm {DATA_ESCAPED}/reno.db")),
            ),
            (
                "rm through a dot-dot that leaves the worktree into the data directory",
                bash("rm ../../Users/u/Library/Application\\ Support/Reno/reno.db"),
            ),
            (
                "a redirect onto the policy",
                bash("echo x > .reno/policy.toml"),
            ),
            ("tee onto git config", bash("echo x | tee .git/config")),
            ("sed -i on git config", bash("sed -i s/a/b/ .git/config")),
            ("cp onto a hook", bash("cp evil .git/hooks/pre-commit")),
            ("rm through a dot-dot", bash("rm -rf src/../.reno")),
            ("rm of .GIT", bash("rm .GIT/config")),
            (
                "rm of the data directory in capitals",
                bash("rm \"/Users/u/LIBRARY/APPLICATION SUPPORT/RENO/reno.db\""),
            ),
            ("sudo rm", bash("sudo rm .reno/policy.toml")),
            (
                "a shell script that removes",
                bash("bash -lc 'rm .reno/policy.toml'"),
            ),
            (
                "a tilde path that names .reno",
                bash("rm ~/.reno/policy.toml"),
            ),
        ]
    }

    const DATA_ESCAPED: &str = "/Users/u/Library/Application\\ Support/Reno";

    #[tokio::test(start_paused = true)]
    async fn an_edit_of_a_protected_path_is_denied_in_every_mode_before_any_allow() {
        let rows = rows_of_edits();
        let subjects: Vec<_> = rows.iter().map(|(_, subject)| subject.clone()).collect();
        for (mode, extra) in MODES {
            for policy in [ALLOW_ALL, "default = \"allow\"\n"] {
                let got = judged(extra, policy, &subjects).await;
                for ((name, _), (outcome, by, message)) in rows.iter().zip(got) {
                    assert_eq!(outcome, ApprovalOutcome::Denied, "{name} at {mode}");
                    assert_eq!(by, protected(), "{name} at {mode}");
                    let message = message.unwrap_or_default();
                    assert!(
                        message.starts_with("denied: `") && message.contains("is protected"),
                        "{name}: {message}"
                    );
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_command_that_writes_a_protected_path_is_denied_in_every_mode_before_any_allow() {
        let rows = rows_of_commands();
        let subjects: Vec<_> = rows.iter().map(|(_, subject)| subject.clone()).collect();
        for (mode, extra) in MODES {
            let got = judged(extra, ALLOW_ALL, &subjects).await;
            for ((name, _), (outcome, by, message)) in rows.iter().zip(got) {
                assert_eq!(outcome, ApprovalOutcome::Denied, "{name} at {mode}");
                assert_eq!(by, protected(), "{name} at {mode}");
                let message = message.unwrap_or_default();
                assert!(
                    message.starts_with("denied: `") && message.contains("is protected"),
                    "{name}: {message}"
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_protection_leaves_the_neighbours_of_a_protected_name_alone() {
        let read_of_reno = gate_call(
            "read",
            "c1",
            serde_json::json!({ "path": ".reno/policy.toml" }),
            &[".reno/policy.toml"],
        );
        let rows: Vec<(&str, ApprovalSubject)> = vec![
            ("an ordinary file", gate_edit("c1", "src/lib.rs")),
            (".gitignore", gate_edit("c1", ".gitignore")),
            (".github", gate_edit("c1", ".github/ci.yml")),
            ("a directory named reno", gate_edit("c1", "reno/x")),
            (
                "a dot-dot that leaves .reno before the file",
                gate_edit("c1", ".reno/../src/a.rs"),
            ),
            (
                "a dot-dot that leaves .git before the file",
                gate_edit("c1", "docs/.git/../x"),
            ),
            (
                "a path that only ends at .reno",
                gate_edit("c1", "a/.reno/.."),
            ),
            (
                "the data directory's name inside the worktree",
                gate_edit("c1", "Library/Application Support/Reno/x"),
            ),
            ("a read of .reno", read_of_reno),
            (
                "a command that reads .reno",
                gate_bash("c1", "cat .reno/policy.toml"),
            ),
            ("git itself", gate_bash("c1", "git status")),
            ("git config", gate_bash("c1", "git config user.name x")),
            ("an ordinary redirect", gate_bash("c1", "echo hi > out.txt")),
        ];
        let subjects: Vec<_> = rows.iter().map(|(_, subject)| subject.clone()).collect();
        for (mode, extra) in MODES {
            let got = judged(extra, ALLOW_ALL, &subjects).await;
            for ((name, _), (outcome, by, _)) in rows.iter().zip(got) {
                assert_eq!(outcome, ApprovalOutcome::Approved, "{name} at {mode}");
                assert_ne!(by, protected(), "{name} at {mode}");
            }
        }
    }
}
