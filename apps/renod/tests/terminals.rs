//! The Terminal tab through the daemon (RP2): `term.*` over the real socket, against a run's
//! worktree in a real temporary git repository, with shells that are not real (`FakePty`). No
//! test here opens a real shell: the daemon's spawner is injected, and a daemon without one has
//! no `terminals` feature.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use common::{Fixture, id, within};
use futures::StreamExt;
use reno_client::ClientError;
use reno_proto::{
    CleanOutcome, CleanTarget, ErrorCode, FEATURES, RunId, RunStatus, TaskRunParams, TermEvent,
    TermOpenParams, TermOpened,
};
use renod::terminals::{FakeProcess, FakePty};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn repo(fx: &Fixture) -> PathBuf {
    let dir = fx.dir.path().canonicalize().unwrap().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "one"]);
    dir
}

fn task(cwd: &Path, executor: &str, in_place: bool) -> TaskRunParams {
    TaskRunParams {
        thread_id: None,
        model_options: Default::default(),
        effort: None,
        prompt: "work".into(),
        executor: Some(id(executor)),
        bot: None,
        cwd: Some(cwd.to_path_buf()),
        model: None,
        in_place,
        attachments: Vec::new(),
    }
}

async fn until_running(fx: &Fixture, run: &RunId) {
    within(async {
        loop {
            let now = fx.status(run).await;
            if now == RunStatus::Running {
                return;
            }
            assert!(!now.is_terminal(), "{run} ended {now:?}");
            tokio::task::yield_now().await;
        }
    })
    .await;
}

async fn until_ended(fx: &Fixture, run: &RunId) {
    within(async {
        loop {
            if fx.status(run).await.is_terminal() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// A live run in a worktree of `repo`, and the worktree (real path).
async fn live(fx: &Fixture, repo: &Path) -> (RunId, PathBuf) {
    let run = within(fx.client.run_task(task(repo, "mock-hold", false)))
        .await
        .unwrap();
    until_running(fx, &run).await;
    let info = within(async {
        loop {
            if let Ok(info) = fx.client.get_workspace(run.clone()).await
                && info.status == reno_proto::WorkspaceStatus::Active
            {
                return info;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    (run, info.path.canonicalize().unwrap())
}

/// A daemon whose shells are `pty`'s.
async fn daemon(pty: &FakePty) -> Fixture {
    let pty = Arc::new(pty.clone());
    Fixture::with(move |c| c.pty = Some(pty)).await
}

fn open(run: &RunId) -> TermOpenParams {
    TermOpenParams {
        run_id: run.clone(),
        cols: 100,
        rows: 30,
        command: None,
    }
}

fn code<T: std::fmt::Debug>(result: Result<T, ClientError>) -> ErrorCode {
    result
        .expect_err("an error")
        .code()
        .expect("a daemon error")
}

/// Ends a live run, and waits for it to be over.
async fn end(fx: &Fixture, run: &RunId) {
    within(fx.client.cancel_run(run, None)).await.unwrap();
    until_ended(fx, run).await;
}

async fn shell_of(pty: &FakePty, n: usize) -> FakeProcess {
    within(async {
        loop {
            if let Some(shell) = pty.processes().get(n) {
                return shell.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminals_are_offered_only_by_a_daemon_that_can_start_shells() {
    assert!(FEATURES.contains(&"terminals"));
    // The default test daemon has no spawner: no feature, and a client refuses up front.
    let plain = Fixture::start().await;
    assert!(!plain.client.supports("terminals"));
    let run = RunId::from_suffix("a").unwrap();
    let error = within(plain.client.term_open(open(&run)))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ClientError::Protocol(why) if why.contains("terminals")),
        "{error}"
    );
    plain.stop().await;

    let fx = daemon(&FakePty::silent()).await;
    assert!(fx.client.supports("terminals"));
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_on_a_live_run_is_sandboxed_and_echoes_through_the_socket() {
    let pty = FakePty::echo();
    let fx = daemon(&pty).await;
    let (run, tree) = live(&fx, &repo(&fx)).await;

    let opened = within(fx.client.term_open(open(&run))).await.unwrap();
    assert!(opened.sandboxed && !opened.in_place);
    assert_eq!(opened.cwd, tree.display().to_string());
    let shell = shell_of(&pty, 0).await;
    let spec = shell.spec();
    assert_eq!(spec.cwd.canonicalize().unwrap(), tree);
    let limits = spec.limits.expect("a live run's shell is limited");
    assert_eq!(limits.root, tree);
    let git_dir = limits
        .private_git_dir
        .expect("a linked worktree's own git dir");
    assert!(
        git_dir.to_string_lossy().contains(".git/worktrees/"),
        "{git_dir:?}"
    );
    assert_eq!((spec.cols, spec.rows), (100, 30));

    // Input comes back as output, numbered, over the socket; `exit` ends it.
    let mut out = within(fx.client.term_attach(opened.id.clone(), None))
        .await
        .unwrap();
    within(fx.client.term_write(opened.id.clone(), b"ls\r"))
        .await
        .unwrap();
    let first = within(out.next()).await.unwrap().unwrap();
    assert_eq!(first.bytes().as_deref(), Some(&b"ls\r"[..]));
    assert_eq!(first.seq(), 1);
    within(fx.client.term_resize(opened.id.clone(), 120, 40))
        .await
        .unwrap();
    within(fx.client.term_write(opened.id.clone(), b"exit\r"))
        .await
        .unwrap();
    let mut last = None;
    while let Some(event) = within(out.next()).await {
        last = Some(event.unwrap());
    }
    assert!(
        matches!(last, Some(TermEvent::Exit { code: Some(0), .. })),
        "{last:?}"
    );
    assert_eq!(shell.resizes(), vec![(120, 40)]);

    let status = within(fx.client.term_status(opened.id.clone()))
        .await
        .unwrap();
    assert!(status.exited && status.sandboxed && !status.busy);
    let listed = within(fx.client.term_list(run.clone())).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, opened.id);
    assert!(listed[0].exited && listed[0].sandboxed);

    within(fx.client.term_close(opened.id.clone()))
        .await
        .unwrap();
    assert_eq!(
        code(within(fx.client.term_status(opened.id)).await),
        ErrorCode::NotFound
    );
    end(&fx, &run).await;
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_opened_after_the_run_ended_is_free_and_one_opened_before_keeps_its_limits() {
    let pty = FakePty::silent();
    let fx = daemon(&pty).await;
    let (run, _tree) = live(&fx, &repo(&fx)).await;
    let during = within(fx.client.term_open(open(&run))).await.unwrap();
    assert!(during.sandboxed);

    end(&fx, &run).await;
    // The shell that was limited still is, and says so, for "reopen without limits".
    let status = within(fx.client.term_status(during.id.clone()))
        .await
        .unwrap();
    assert!(status.sandboxed);
    // "Reopen without limits": a new shell, in the same place, once the run is over.
    let after = within(fx.client.term_open(open(&run))).await.unwrap();
    assert!(!after.sandboxed);
    assert_eq!(after.cwd, during.cwd);
    let specs: Vec<_> = pty
        .processes()
        .iter()
        .map(|p| p.spec().limits.is_some())
        .collect();
    assert_eq!(specs, [true, false]);
    let listed = within(fx.client.term_list(run)).await.unwrap();
    assert_eq!(
        listed.iter().map(|t| t.sandboxed).collect::<Vec<_>>(),
        [true, false]
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_run_gets_no_terminal_when_the_sandbox_cannot_start() {
    let pty = FakePty::silent();
    pty.refuse_sandbox(Some("sandbox-exec is missing"));
    let fx = daemon(&pty).await;
    let (run, _) = live(&fx, &repo(&fx)).await;
    assert_eq!(
        code(within(fx.client.term_open(open(&run))).await),
        ErrorCode::SandboxUnavailable
    );
    assert_eq!(pty.started(), 0);
    assert!(
        within(fx.client.term_list(run.clone()))
            .await
            .unwrap()
            .is_empty()
    );
    // Once the run is over there is nothing to protect, and the terminal opens.
    end(&fx, &run).await;
    let opened: TermOpened = within(fx.client.term_open(open(&run))).await.unwrap();
    assert!(!opened.sandboxed);
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_with_no_worktree_gets_its_own_folder_flagged_and_unlimited() {
    let pty = FakePty::silent();
    let fx = daemon(&pty).await;
    let folder = fx.dir.path().canonicalize().unwrap().join("plain");
    std::fs::create_dir_all(&folder).unwrap();
    let run = within(fx.client.run_task(task(&folder, "mock-hold", true)))
        .await
        .unwrap();
    until_running(&fx, &run).await;
    let opened = within(fx.client.term_open(open(&run))).await.unwrap();
    assert!(opened.in_place && !opened.sandboxed);
    assert_eq!(
        pty.processes()[0].spec().cwd.canonicalize().unwrap(),
        folder
    );
    let listed = within(fx.client.term_list(run.clone())).await.unwrap();
    assert!(listed[0].in_place);
    end(&fx, &run).await;
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_that_has_not_started_or_whose_worktree_is_gone_has_no_terminal() {
    let pty = FakePty::silent();
    let fx = daemon(&pty).await;
    let repo = repo(&fx);
    let (run, _tree) = live(&fx, &repo).await;
    end(&fx, &run).await;
    let kept = within(fx.client.term_open(open(&run))).await.unwrap();
    // Removed with the run's worktree: the terminal goes, and a new one is refused as the Files
    // tab is.
    let items = within(fx.client.clean_workspaces(
        CleanTarget::Run {
            run_id: run.clone(),
        },
        true,
    ))
    .await
    .unwrap();
    assert_eq!(items[0].outcome, CleanOutcome::Removed);
    assert_eq!(items[0].closed_terminals, vec![kept.id.clone()]);
    assert!(pty.processes()[0].hung_up());
    assert_eq!(
        code(within(fx.client.term_open(open(&run))).await),
        ErrorCode::WorkspaceUnavailable
    );
    assert_eq!(
        code(within(fx.client.term_list(run.clone())).await),
        ErrorCode::WorkspaceUnavailable
    );
    assert_eq!(
        code(within(fx.client.term_status(kept.id)).await),
        ErrorCode::NotFound
    );
    // An unknown run is not found.
    let unknown = RunId::from_suffix("nonesuch").unwrap();
    assert_eq!(
        code(within(fx.client.term_open(open(&unknown))).await),
        ErrorCode::NotFound
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_a_thread_closes_the_terminals_of_the_worktree_it_removes() {
    let pty = FakePty::silent();
    let fx = daemon(&pty).await;
    let (run, _tree) = live(&fx, &repo(&fx)).await;
    let thread = within(fx.client.list_threads(None)).await.unwrap()[0]
        .thread_id
        .clone();
    let opened = within(fx.client.term_open(open(&run))).await.unwrap();

    // A live run refuses to close, and refuses before it takes anything: the shell is untouched.
    let error = within(fx.client.close_thread(thread.clone(), true))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Conflict), "{error}");
    let cleaned = within(fx.client.clean_workspaces(
        CleanTarget::Run {
            run_id: run.clone(),
        },
        true,
    ))
    .await;
    assert_eq!(code(cleaned), ErrorCode::Conflict);
    assert!(!pty.processes()[0].hung_up());
    assert!(
        !within(fx.client.term_status(opened.id.clone()))
            .await
            .unwrap()
            .exited
    );

    end(&fx, &run).await;
    let closed = within(fx.client.close_thread(thread, true)).await.unwrap();
    assert_eq!(closed.workspace, Some(CleanOutcome::Removed));
    assert_eq!(closed.closed_terminals, vec![opened.id.clone()]);
    assert!(pty.processes()[0].hung_up());
    assert_eq!(
        code(within(fx.client.term_status(opened.id)).await),
        ErrorCode::NotFound
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn naming_a_worktree_closes_its_shells_first_even_when_it_is_then_kept() {
    let pty = FakePty::silent();
    let fx = daemon(&pty).await;
    let repo = repo(&fx);
    let (run, tree) = live(&fx, &repo).await;
    // Work that is not merged: a clean without force keeps the worktree.
    std::fs::write(tree.join("work.txt"), "unfinished\n").unwrap();
    end(&fx, &run).await;
    let opened = within(fx.client.term_open(open(&run))).await.unwrap();
    let items = within(fx.client.clean_workspaces(
        CleanTarget::Run {
            run_id: run.clone(),
        },
        false,
    ))
    .await
    .unwrap();
    // The person named this worktree: nothing writes into it while it is considered.
    assert_eq!(items[0].outcome, CleanOutcome::Kept, "{items:?}");
    assert_eq!(items[0].closed_terminals, vec![opened.id.clone()]);
    assert!(pty.processes()[0].hung_up());
    assert_eq!(
        code(within(fx.client.term_status(opened.id)).await),
        ErrorCode::NotFound
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bulk_clean_closes_the_shells_of_the_worktrees_it_removes_and_only_those() {
    let pty = FakePty::silent();
    let fx = daemon(&pty).await;
    let repo = repo(&fx);
    // One worktree is clean and goes; one holds unfinished work and stays.
    let (clean_run, _) = live(&fx, &repo).await;
    end(&fx, &clean_run).await;
    let (dirty_run, dirty_tree) = live(&fx, &repo).await;
    std::fs::write(dirty_tree.join("work.txt"), "unfinished\n").unwrap();
    end(&fx, &dirty_run).await;
    let in_clean = within(fx.client.term_open(open(&clean_run))).await.unwrap();
    let in_dirty = within(fx.client.term_open(open(&dirty_run))).await.unwrap();

    let items = within(fx.client.clean_workspaces(CleanTarget::AllClean, false))
        .await
        .unwrap();
    let by_run = |run: &RunId| items.iter().find(|i| i.run_id == *run).expect("an item");
    assert_eq!(
        by_run(&clean_run).outcome,
        CleanOutcome::Removed,
        "{items:?}"
    );
    assert_eq!(
        by_run(&clean_run).closed_terminals,
        vec![in_clean.id.clone()]
    );
    assert_eq!(by_run(&dirty_run).outcome, CleanOutcome::Kept, "{items:?}");
    assert!(by_run(&dirty_run).closed_terminals.is_empty());
    assert!(pty.processes()[0].hung_up());
    assert!(!pty.processes()[1].hung_up());
    assert!(
        !within(fx.client.term_status(in_dirty.id))
            .await
            .unwrap()
            .exited
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_ends_every_shell_when_it_stops() {
    let pty = FakePty::silent();
    let fx = daemon(&pty).await;
    let (run, _) = live(&fx, &repo(&fx)).await;
    within(fx.client.term_open(open(&run))).await.unwrap();
    within(fx.client.term_open(open(&run))).await.unwrap();
    fx.stop().await;
    assert!(pty.processes().iter().all(FakeProcess::hung_up));
}

/// `reno`, built now (a stale one beside `renod` would not know `term`).
fn reno_binary() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    BUILD.call_once(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "--package", "reno"])
            .status()
            .expect("run cargo");
        assert!(status.success(), "cargo build --package reno failed");
    });
    Path::new(env!("CARGO_BIN_EXE_renod")).with_file_name("reno")
}

/// Runs `reno` against the daemon of `fx` with `input` on its stdin, off the async threads.
async fn reno(fx: &Fixture, args: &[&str], input: &str) -> (String, String, bool) {
    let socket = fx.paths.socket.clone();
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let input = input.to_owned();
    let binary = tokio::task::spawn_blocking(reno_binary).await.unwrap();
    within(tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut child = Command::new(binary)
            .arg("--socket")
            .arg(socket)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("run reno");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
        let out = child.wait_with_output().expect("reno ends");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.success(),
        )
    }))
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cli_opens_lists_attaches_and_closes_terminals() {
    let pty = FakePty::echo();
    let fx = daemon(&pty).await;
    let (run, _tree) = live(&fx, &repo(&fx)).await;

    let (out, err, ok) = reno(&fx, &["term", "open", run.as_str()], "").await;
    assert!(ok, "{err}");
    let id = out.trim().to_owned();
    assert!(id.starts_with("term_"), "{out:?}");
    assert!(err.contains("cannot write protected paths"), "{err}");

    let (out, _, ok) = reno(&fx, &["term", "list", run.as_str()], "").await;
    assert!(ok);
    assert!(out.contains(&id) && out.contains("sandboxed"), "{out}");

    // Piped input is passed to the shell as it is, the output comes back as it is, and the
    // shell's end is the command's.
    let (out, err, ok) = reno(&fx, &["term", "attach", &id], "echo hi\rexit\r").await;
    assert!(ok, "{err}");
    assert!(out.contains("echo hi\r"), "{out:?}");
    assert!(err.contains("the shell ended: exit code 0"), "{err}");

    let (_, err, ok) = reno(&fx, &["term", "close", &id], "").await;
    assert!(ok, "{err}");
    let (out, _, _) = reno(&fx, &["--json", "term", "list", run.as_str()], "").await;
    assert_eq!(out.trim(), "[]");
    // A terminal that is not there says so.
    let (_, err, ok) = reno(&fx, &["term", "close", &id], "").await;
    assert!(!ok);
    assert!(err.contains("not found"), "{err}");
    end(&fx, &run).await;
    fx.stop().await;
}
