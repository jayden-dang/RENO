//! The D7 sandbox against the real `sandbox-exec` (macOS only): what a person's shell in a live
//! run's worktree cannot write, and what it still can. The tool only restricts, so running it is
//! safe: no PTY, no shell of the person's, a tempdir repository and worktree.
//!
//! The last tests open a real PTY and are opt-in: they run only with `RENO_LIVE_PTY=1`.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use renod::terminals::pty::{PtySpawner, RealPty, SANDBOX_EXEC, SpawnError, SpawnSpec};
use renod::terminals::sandbox::Limits;

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

/// A repository with one commit, and a linked worktree on branch `reno/t1` the way Reno makes
/// one. The temp directory is under `/var`, which is `/private/var`: paths here are deliberately
/// the symlinked spelling.
struct Tree {
    _dir: tempfile::TempDir,
    /// The worktree, spelled through the `/var` symlink.
    path: PathBuf,
    repo: PathBuf,
}

fn tree() -> Tree {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "one"]);
    let path = dir.path().join("my tree.d");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "reno/t1",
            path.to_str().unwrap(),
        ],
    );
    Tree {
        _dir: dir,
        path,
        repo,
    }
}

/// Runs `script` in `sh` inside the worktree under the profile of its limits, as the person's
/// shell would, and returns its output and whether it succeeded.
fn sandboxed(tree: &Tree, script: &str) -> (String, bool) {
    let limits = Limits::for_worktree(&tree.path).unwrap();
    let out = Command::new(SANDBOX_EXEC)
        .arg("-p")
        .arg(limits.profile())
        .args(["/bin/sh", "-c", script])
        .current_dir(&tree.path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("sandbox-exec runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (text, out.status.success())
}

fn denied(tree: &Tree, script: &str) {
    let (text, ok) = sandboxed(tree, script);
    assert!(!ok, "should have been denied: {script}\n{text}");
    assert!(
        text.contains("Operation not permitted")
            || text.contains("denied")
            || text.contains("fatal"),
        "{script}: {text}"
    );
}

fn allowed(tree: &Tree, script: &str) {
    let (text, ok) = sandboxed(tree, script);
    assert!(ok, "should have worked: {script}\n{text}");
}

#[test]
fn protected_files_cannot_be_written_created_renamed_into_or_removed() {
    let t = tree();
    // Writes into what does not exist yet.
    denied(&t, "mkdir .claude");
    denied(
        &t,
        "mkdir -p .claude/sub && echo x > .claude/sub/settings.json",
    );
    denied(&t, "echo '{}' > .mcp.json");
    denied(&t, "mkdir -p .codex && echo x > .codex/config.toml");
    denied(&t, "mkdir .reno");
    // Through a child process: the denial is the sandbox's, not the shell's.
    denied(&t, "sh -c 'sh -c \"echo x > .mcp.json\"'");
    // A rename into a protected path, and out of one.
    std::fs::write(t.path.join("plain.txt"), "p").unwrap();
    denied(&t, "mv plain.txt .mcp.json");
    denied(&t, "mkdir .x && mv .x .claude");
    // Names count at any depth, and in any case, as the guard counts them.
    denied(&t, "mkdir -p pkg/.claude && echo x > pkg/.claude/s.json");
    denied(&t, "mkdir -p pkg/deep/.codex");
    denied(&t, "mkdir -p pkg && echo x > pkg/.mcp.json");
    denied(&t, "mkdir -p pkg && echo x > pkg/.MCP.JSON");
    denied(&t, "mkdir .CLAUDE");
    denied(&t, "mkdir .Reno");
    // Existing protected files cannot be changed or removed either.
    // (They are made outside the sandbox, as the run or git would.)
    std::fs::create_dir_all(t.path.join(".claude")).unwrap();
    std::fs::write(t.path.join(".claude/settings.json"), "{}").unwrap();
    std::fs::write(t.path.join(".mcp.json"), "{}").unwrap();
    denied(&t, "echo changed > .claude/settings.json");
    denied(&t, "rm .claude/settings.json");
    denied(&t, "rm .mcp.json");
    denied(&t, "mv .mcp.json elsewhere.json");
    denied(&t, "touch .claude/settings.json");
    denied(&t, "chmod 777 .mcp.json");
    // Reading them is fine.
    allowed(&t, "cat .claude/settings.json .mcp.json");
}

#[test]
fn ordinary_work_and_read_only_git_still_work() {
    let t = tree();
    allowed(
        &t,
        "echo new > src_new.rs && mkdir -p src/deep && echo x > src/deep/f.rs",
    );
    allowed(&t, "mkdir node_modules && echo x > node_modules/m.js");
    allowed(&t, "mv src_new.rs renamed.rs && rm renamed.rs");
    allowed(
        &t,
        "echo x > .gitignore && echo y > .github.yml && mkdir .githooks_not",
    );
    // Reading a tree needs no write in the git directory.
    allowed(
        &t,
        "git status --short && git log --oneline && git diff && git branch --show-current",
    );
    let (branch, _) = sandboxed(&t, "git branch --show-current");
    assert_eq!(branch.trim(), "reno/t1");
    // A write outside the worktree is the person's own business.
    let elsewhere = tempfile::tempdir().unwrap();
    allowed(
        &t,
        &format!("echo x > '{}/out.txt'", elsewhere.path().display()),
    );
}

#[test]
fn a_commit_on_the_runs_branch_is_refused_also_from_a_child() {
    let t = tree();
    let head = git(&t.path, &["rev-parse", "HEAD"]);
    allowed(&t, "echo more >> a.txt");
    denied(&t, "git add a.txt");
    denied(&t, "git commit -qam two");
    denied(&t, "sh -c 'git commit -qam two'");
    denied(&t, "git checkout -q -b other");
    // Nothing moved: the run's branch is where it was, and the repository's own checkout is
    // not the sandbox's business.
    assert_eq!(git(&t.path, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&t.path, &["branch", "--show-current"]), "reno/t1");
    std::fs::write(t.repo.join("b.txt"), "b\n").unwrap();
    git(&t.repo, &["add", "b.txt"]);
    git(&t.repo, &["commit", "-qm", "main moves"]);
}

/// What the supervisor's guard sees in `tree`: the `Workspace` port's `protected_changes`, the
/// very call it makes after every tool call that could write.
async fn guard_sees(tree: &Tree) -> Vec<String> {
    use reno_harness::proc::git_workspace::{GitWorktrees, Location};
    let base_sha = git(&tree.repo, &["rev-parse", "main"]);
    GitWorktrees::new(tree.path.parent().unwrap().to_path_buf())
        .protected_changes(&Location {
            repo: tree.repo.clone(),
            repo_key: "k".into(),
            path: tree.path.clone(),
            subdir: String::new(),
            branch: "reno/t1".into(),
            base_sha,
        })
        .await
        .unwrap()
}

/// Every way a person's shell might touch a protected path, each from the guard's own list.
fn attempts() -> Vec<String> {
    let mut attempts = Vec::new();
    for name in reno_proto::defs::PROTECTED_DIRS
        .iter()
        .filter(|name| **name != ".git")
    {
        attempts.push(format!("mkdir '{name}' && echo x > '{name}/f'"));
        attempts.push(format!("mkdir -p 'sub/{name}' && echo x > 'sub/{name}/f'"));
    }
    attempts.push(format!("echo x > {}", reno_proto::defs::PROTECTED_FILE));
    attempts.push(format!(
        "mkdir -p pkg && echo x > pkg/{}",
        reno_proto::defs::PROTECTED_FILE
    ));
    attempts.push("mkdir -p .CLAUDE && echo x > .CLAUDE/f".to_owned());
    attempts
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_guard_sees_nothing_of_what_a_sandboxed_shell_tried() {
    let t = tree();
    assert!(guard_sees(&t).await.is_empty());
    for attempt in attempts() {
        let (_, _) = sandboxed(&t, &attempt);
    }
    // Ordinary work shows as work, never as a protected path.
    allowed(&t, "echo new > feature.rs");
    assert_eq!(
        guard_sees(&t).await,
        Vec::<String>::new(),
        "the guard would blame the run for a person's attempt"
    );
    assert_eq!(
        git(&t.path, &["status", "--porcelain", "--untracked-files=all"]),
        "?? feature.rs"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_attempts_without_the_sandbox_do_trip_the_guard() {
    // The control: the check above can fail. Unsandboxed, the guard's port reports the paths.
    let t = tree();
    for attempt in attempts() {
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(&attempt)
            .current_dir(&t.path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{attempt}");
    }
    let seen = guard_sees(&t).await;
    for expected in [
        ".claude/f",
        ".codex/f",
        ".reno/f",
        ".mcp.json",
        "sub/.claude/f",
        "pkg/.mcp.json",
    ] {
        assert!(
            seen.iter().any(|p| p == expected),
            "{expected} missing from {seen:?}"
        );
    }
}

#[test]
fn the_profile_needs_the_real_path_and_the_limits_use_it() {
    let t = tree();
    // The tempdir is spelled `/var/...`, and `/var` is a link to `/private/var`.
    let spelled = t.path.to_str().unwrap();
    let limits = Limits::for_worktree(&t.path).unwrap();
    if spelled.starts_with("/var/") {
        assert!(limits.root.starts_with("/private/var"), "{:?}", limits.root);
    }
    // The profile as a person would write it from the symlinked spelling does not deny: the
    // spike's finding, kept as a test so a change that stops canonicalizing fails here.
    let naive =
        format!("(version 1)(allow default)(deny file-write* (subpath \"{spelled}/.claude\"))");
    if spelled.starts_with("/var/") {
        let out = Command::new(SANDBOX_EXEC)
            .arg("-p")
            .arg(&naive)
            .args(["/bin/sh", "-c", "mkdir .claude"])
            .current_dir(&t.path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "the naive profile unexpectedly denied"
        );
        std::fs::remove_dir_all(t.path.join(".claude")).unwrap();
    }
    // Ours denies, asked from either spelling.
    denied(&t, "mkdir .claude");
    let (text, ok) = {
        let out = Command::new(SANDBOX_EXEC)
            .arg("-p")
            .arg(limits.profile())
            .args(["/bin/sh", "-c", "mkdir .claude"])
            .current_dir(t.path.canonicalize().unwrap())
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.success(),
        )
    };
    assert!(!ok, "{text}");
}

#[test]
fn a_missing_or_failing_sandbox_exec_refuses_the_shell_before_anything_starts() {
    let t = tree();
    let limits = Limits::for_worktree(&t.path).unwrap();
    let spec = |limits: Option<Limits>| SpawnSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec![OsString::from("-c"), OsString::from("exit 0")],
        cwd: t.path.clone(),
        env: Vec::new(),
        cols: 80,
        rows: 24,
        limits,
    };
    // The tool is not there.
    let missing = RealPty::new().with_sandbox_exec("/nonexistent/sandbox-exec");
    let error = missing.spawn(spec(Some(limits.clone()))).unwrap_err();
    assert!(
        matches!(error, SpawnError::SandboxUnavailable(_)),
        "{error}"
    );
    // The tool is there and refuses (it is `false`).
    let refusing = RealPty::new().with_sandbox_exec("/usr/bin/false");
    let error = refusing.spawn(spec(Some(limits))).unwrap_err();
    assert!(
        matches!(error, SpawnError::SandboxUnavailable(_)),
        "{error}"
    );
}

fn live() -> bool {
    if std::env::var_os("RENO_LIVE_PTY").is_some_and(|v| v == "1") {
        return true;
    }
    #[allow(clippy::print_stderr)]
    {
        eprintln!("skipped: set RENO_LIVE_PTY=1 to open a real PTY");
    }
    false
}

/// Reads a PTY to its end.
fn drain(mut reader: Box<dyn Read + Send>) -> String {
    let mut out = Vec::new();
    let mut buf = [0_u8; 4096];
    while let Ok(n) = reader.read(&mut buf) {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn live_pty_a_real_shell_echoes_and_reports_its_exit_code() {
    if !live() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut parts = RealPty::new()
        .spawn(SpawnSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "echo reno-pty-$TERM; exit 7".into()],
            cwd: dir.path().to_path_buf(),
            env: vec![("TERM".into(), "xterm-256color".into())],
            cols: 80,
            rows: 24,
            limits: None,
        })
        .unwrap();
    // Read while the shell runs: closing a tty waits for its output to be read on macOS.
    let reader = std::mem::replace(&mut parts.reader, Box::new(std::io::empty()));
    let output = std::thread::spawn(move || drain(reader));
    let info = parts.child.wait();
    assert_eq!(info.code, Some(7));
    parts.control.release();
    drop(parts.writer);
    let output = output.join().unwrap();
    assert!(output.contains("reno-pty-xterm-256color"), "{output:?}");
}

#[test]
fn live_pty_a_sandboxed_real_shell_cannot_write_a_protected_path() {
    if !live() {
        return;
    }
    let t = tree();
    let limits = Limits::for_worktree(&t.path).unwrap();
    let mut parts = RealPty::new()
        .spawn(SpawnSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".into(),
                "echo x > .mcp.json; echo mcp-rc=$?; echo y > ok.txt; echo ok-rc=$?".into(),
            ],
            cwd: t.path.clone(),
            env: Vec::new(),
            cols: 80,
            rows: 24,
            limits: Some(limits),
        })
        .unwrap();
    let reader = std::mem::replace(&mut parts.reader, Box::new(std::io::empty()));
    let output = std::thread::spawn(move || drain(reader));
    let info = parts.child.wait();
    assert_eq!(info.code, Some(0));
    parts.control.release();
    drop(parts.writer);
    let output = output.join().unwrap();
    assert!(output.contains("mcp-rc=1"), "{output:?}");
    assert!(output.contains("ok-rc=0"), "{output:?}");
    assert!(!t.path.join(".mcp.json").exists());
}

/// A real shell through the whole service: the reader thread, the pump, the lane, the exit.
async fn live_session(dir: &Path, live: bool, input: &str) -> (String, Option<i32>) {
    use futures::StreamExt;
    use reno_proto::{TermEvent, encode_bytes};
    use renod::terminals::{OpenRequest, Terminals, Timing};
    let terms = Terminals::new(
        Some(std::sync::Arc::new(RealPty::new())),
        vec![
            ("SHELL".into(), "/bin/sh".into()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ],
        Timing::default(),
    );
    let opened = terms
        .open(OpenRequest {
            dir: dir.to_path_buf(),
            in_place: false,
            live,
            cols: 100,
            rows: 30,
            command: None,
        })
        .await
        .unwrap();
    assert_eq!(opened.sandboxed, live);
    let mut out = Box::pin(terms.attach(&opened.id, None).unwrap());
    terms
        .write(&opened.id, &encode_bytes(input.as_bytes()))
        .unwrap();
    let mut text = Vec::new();
    let mut code = None;
    let ended = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Some(event) = out.next().await {
            match event {
                TermEvent::Data { .. } => text.extend(event.bytes().unwrap()),
                TermEvent::Exit { code: exit, .. } => {
                    code = exit;
                    break;
                }
                TermEvent::Gap { .. } => {}
            }
        }
    })
    .await;
    assert!(
        ended.is_ok(),
        "the shell never ended: {:?}",
        String::from_utf8_lossy(&text)
    );
    terms.shutdown().await;
    (String::from_utf8_lossy(&text).into_owned(), code)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_pty_the_service_runs_a_real_shell_from_input_to_exit_code() {
    if !live() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (text, code) = live_session(dir.path(), false, "echo reno-live-$((1+2))\nexit 3\n").await;
    assert!(text.contains("reno-live-3"), "{text:?}");
    assert_eq!(code, Some(3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_pty_the_service_limits_a_real_shell_on_a_live_run() {
    if !live() {
        return;
    }
    let t = tree();
    let (text, code) = live_session(
        &t.path,
        true,
        "echo x > .mcp.json; echo mcp-rc=$?; echo y > fine.txt; echo fine-rc=$?; git commit -qam no; echo commit-rc=$?; exit 0\n",
    )
    .await;
    assert!(text.contains("mcp-rc=1"), "{text:?}");
    assert!(text.contains("fine-rc=0"), "{text:?}");
    assert!(!text.contains("commit-rc=0"), "{text:?}");
    assert_eq!(code, Some(0));
    assert!(!t.path.join(".mcp.json").exists());
    assert!(t.path.join("fine.txt").exists());
}

/// A started real terminal, to be driven by a test.
async fn live_open(
    dir: &Path,
    live: bool,
) -> (
    renod::terminals::Terminals,
    reno_proto::TermId,
    std::pin::Pin<Box<dyn futures::Stream<Item = reno_proto::TermEvent> + Send>>,
) {
    use renod::terminals::{OpenRequest, Terminals, Timing};
    let terms = Terminals::new(
        Some(std::sync::Arc::new(RealPty::new())),
        vec![
            ("SHELL".into(), "/bin/sh".into()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ],
        Timing::default(),
    );
    let opened = terms
        .open(OpenRequest {
            dir: dir.to_path_buf(),
            in_place: false,
            live,
            cols: 100,
            rows: 30,
            command: None,
        })
        .await
        .unwrap();
    assert_eq!(opened.sandboxed, live);
    let out = Box::pin(terms.attach(&opened.id, None).unwrap());
    (terms, opened.id, out)
}

/// Waits until `status` says what `want` is, for at most ten seconds.
async fn status_becomes(
    terms: &renod::terminals::Terminals,
    id: &reno_proto::TermId,
    want: bool,
) -> bool {
    for _ in 0..200 {
        if terms.status(id).unwrap().busy == want {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

async fn busy_follows_the_foreground(live: bool) {
    use futures::StreamExt;
    use reno_proto::encode_bytes;
    let t = tree();
    let (terms, id, mut out) = live_open(&t.path, live).await;
    // The shell's own prompt has been printed once something has come out.
    let first = tokio::time::timeout(std::time::Duration::from_secs(20), out.next())
        .await
        .expect("the shell printed nothing");
    assert!(first.is_some());
    assert!(!terms.status(&id).unwrap().busy, "idle at the prompt");
    terms.write(&id, &encode_bytes(b"sleep 4\n")).unwrap();
    assert!(status_becomes(&terms, &id, true).await, "busy during sleep");
    assert!(
        status_becomes(&terms, &id, false).await,
        "idle again after it"
    );
    terms.write(&id, &encode_bytes(b"exit\n")).unwrap();
    terms.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_pty_status_says_busy_while_a_program_runs_and_a_limited_shell_keeps_its_pid() {
    if !live() {
        return;
    }
    busy_follows_the_foreground(false).await;
    // Under sandbox-exec the shell is still the PTY's leader: the same answers.
    busy_follows_the_foreground(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_pty_a_background_job_holding_the_terminal_does_not_hold_the_exit_or_the_shutdown() {
    use futures::StreamExt;
    use reno_proto::{TermEvent, encode_bytes};
    if !live() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (terms, id, mut out) = live_open(dir.path(), false).await;
    let started = std::time::Instant::now();
    terms
        .write(&id, &encode_bytes(b"sleep 6 &\nexit 0\n"))
        .unwrap();
    let mut code = None;
    let ended = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while let Some(event) = out.next().await {
            if let TermEvent::Exit { code: exit, .. } = event {
                code = exit;
                break;
            }
        }
    })
    .await;
    assert!(
        ended.is_ok(),
        "no exit while a background job holds the terminal"
    );
    assert_eq!(code, Some(0));
    // The exit waits for the output to end, but not for the job: one grace, not six seconds.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let stopping = std::time::Instant::now();
    terms.shutdown().await;
    assert!(
        stopping.elapsed() < std::time::Duration::from_secs(4),
        "{:?}",
        stopping.elapsed()
    );
}
