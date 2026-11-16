//! First start, reading the directory, and the watch. Real files in a temp directory, the real
//! platform watcher, and no wall-clock assertion: waits are bounded by a failsafe that only
//! turns a hang into a failure.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use notify::event::{AccessKind, CreateKind, Event, Flag};
use reno_kernel::Definitions;
use reno_proto::defs::{DefKind, MAX_FILE_BYTES, ValidationContext, load_set};
use tempfile::TempDir;

use super::*;

const FAILSAFE: Duration = Duration::from_secs(60);

fn fixed(outcome: GitOutcome) -> GitInit {
    match outcome {
        GitOutcome::Initialized | GitOutcome::AlreadyARepository => {
            |_| Box::pin(async { GitOutcome::Initialized })
        }
        GitOutcome::Unavailable => |_| Box::pin(async { GitOutcome::Unavailable }),
        GitOutcome::Failed(_) => |_| Box::pin(async { GitOutcome::Failed("boom".into()) }),
    }
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn temp_root() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(".reno");
    (dir, root)
}

fn names(snapshot: &DefSnapshot) -> Vec<(DefKind, &str)> {
    snapshot
        .files
        .iter()
        .map(|f| (f.kind, f.name.as_str()))
        .collect()
}

// ---- first start --------------------------------------------------------------------------

#[tokio::test]
async fn first_start_creates_the_example_bot_and_the_default_policy() {
    let (_dir, root) = temp_root();
    let done = bootstrap(&root, fixed(GitOutcome::Initialized))
        .await
        .unwrap();
    assert_eq!(
        done,
        Bootstrapped {
            created: true,
            git: Some(GitOutcome::Initialized)
        }
    );

    assert_eq!(
        fs::read_to_string(root.join("bots/example.toml")).unwrap(),
        defaults::EXAMPLE_BOT
    );
    assert_eq!(
        fs::read_to_string(root.join("policy.toml")).unwrap(),
        defaults::POLICY
    );
    assert!(root.join("triggers").is_dir());
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );

    // What was written loads clean, with only the executors Reno can build.
    let fs_defs = FsDefinitions::open(&root, DEFAULT_DEBOUNCE).unwrap();
    let snapshot = fs_defs.snapshot().await.unwrap();
    assert!(snapshot.problems.is_empty());
    let set = load_set(&snapshot.files, &ValidationContext::with_builtin([])).unwrap();
    assert_eq!(set.bots().keys().collect::<Vec<_>>(), ["example"]);
    assert!(set.policy().is_some());
}

#[tokio::test]
async fn an_existing_directory_with_no_definitions_gets_the_examples_and_keeps_its_own_files() {
    let (_dir, root) = temp_root();
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    write(&root, "scratch/notes.txt", "mine");
    let done = bootstrap(&root, fixed(GitOutcome::Initialized))
        .await
        .unwrap();
    assert_eq!(
        done,
        Bootstrapped {
            created: true,
            git: Some(GitOutcome::Initialized)
        }
    );
    assert_eq!(
        fs::read_to_string(root.join("scratch/notes.txt")).unwrap(),
        "mine"
    );
    assert!(root.join("bots/example.toml").is_file());
    // Its mode is the user's to choose.
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[tokio::test]
async fn a_directory_that_is_already_a_repository_is_not_initialised_again() {
    let (_dir, root) = temp_root();
    fs::create_dir_all(root.join(".git")).unwrap();
    let done = bootstrap(&root, fixed(GitOutcome::Failed(String::new())))
        .await
        .unwrap();
    assert_eq!(done.git, Some(GitOutcome::AlreadyARepository));
    assert!(root.join("policy.toml").is_file());
}

#[tokio::test]
async fn once_any_definition_exists_the_directory_is_never_touched() {
    for existing in ["bots/mine.toml", "triggers/mine.toml", "policy.toml"] {
        let (_dir, root) = temp_root();
        write(&root, existing, "# mine\n");
        let done = bootstrap(&root, fixed(GitOutcome::Initialized))
            .await
            .unwrap();
        assert_eq!(
            done,
            Bootstrapped {
                created: false,
                git: None
            },
            "{existing}"
        );
        let mut seen = Vec::new();
        for entry in walk(&root) {
            seen.push(entry);
        }
        assert_eq!(
            seen,
            [existing.to_owned()],
            "nothing was added next to {existing}"
        );
    }

    // An empty `bots/` is the user's doing too: the example they may have deleted stays gone.
    let (_dir, root) = temp_root();
    fs::create_dir_all(root.join("bots")).unwrap();
    let again = bootstrap(&root, fixed(GitOutcome::Initialized))
        .await
        .unwrap();
    assert!(!again.created);
    assert!(!root.join("bots/example.toml").exists());

    // Starting again after a first start changes nothing, and runs no git.
    let (_dir2, root2) = temp_root();
    bootstrap(&root2, fixed(GitOutcome::Initialized))
        .await
        .unwrap();
    write(&root2, "bots/example.toml", "# edited by the user\n");
    let again = bootstrap(&root2, fixed(GitOutcome::Failed(String::new())))
        .await
        .unwrap();
    assert!(!again.created);
    assert_eq!(
        fs::read_to_string(root2.join("bots/example.toml")).unwrap(),
        "# edited by the user\n"
    );
}

/// Every file under `root`, as sorted relative paths.
fn walk(root: &Path) -> Vec<String> {
    fn go(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                go(&path, root, out);
            } else {
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                );
            }
        }
    }
    let mut out = Vec::new();
    go(root, root, &mut out);
    out.sort();
    out
}

#[tokio::test]
async fn a_missing_git_is_reported_and_the_files_are_still_made() {
    let (_dir, root) = temp_root();
    let done = bootstrap(&root, fixed(GitOutcome::Unavailable))
        .await
        .unwrap();
    assert_eq!(done.git, Some(GitOutcome::Unavailable));
    assert!(root.join("bots/example.toml").is_file());
    assert!(!root.join(".git").exists());
}

#[tokio::test]
async fn a_failing_git_is_reported_and_the_files_are_still_made() {
    let (_dir, root) = temp_root();
    let done = bootstrap(&root, fixed(GitOutcome::Failed(String::new())))
        .await
        .unwrap();
    assert_eq!(done.git, Some(GitOutcome::Failed("boom".into())));
    assert!(root.join("policy.toml").is_file());
}

#[tokio::test]
async fn a_root_that_cannot_be_created_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("in-the-way");
    fs::write(&file, "x").unwrap();
    let result = bootstrap(&file.join("sub"), fixed(GitOutcome::Initialized)).await;
    assert!(result.is_err());
}

/// The real `git init`, when git is installed; otherwise the missing-git path is the real one.
#[tokio::test]
async fn the_real_git_init_makes_a_repository_or_says_git_is_missing() {
    let installed = std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok();
    let (_dir, root) = temp_root();
    let done = bootstrap(&root, git_init).await.unwrap();
    if installed {
        assert_eq!(done.git, Some(GitOutcome::Initialized));
        assert!(root.join(".git").is_dir());
    } else {
        assert_eq!(done.git, Some(GitOutcome::Unavailable));
    }
}

#[test]
fn the_default_directory_is_dot_reno_unless_overridden() {
    use std::collections::HashMap;
    use std::ffi::OsString;
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        let map: HashMap<_, _> = pairs.iter().copied().collect();
        move |key: &str| map.get(key).map(OsString::from)
    };
    assert_eq!(
        defs_dir_from(env(&[("HOME", "/Users/u")])),
        Some(PathBuf::from("/Users/u/.reno"))
    );
    assert_eq!(
        defs_dir_from(env(&[("HOME", "/Users/u"), (DEFS_DIR_ENV, "/tmp/defs")])),
        Some(PathBuf::from("/tmp/defs"))
    );
    assert_eq!(
        defs_dir_from(env(&[("HOME", "/Users/u"), (DEFS_DIR_ENV, "")])),
        Some(PathBuf::from("/Users/u/.reno"))
    );
    assert_eq!(defs_dir_from(env(&[])), None);
}

// ---- reading ------------------------------------------------------------------------------

#[tokio::test]
async fn a_snapshot_has_every_definition_file_in_order() {
    let (_dir, root) = temp_root();
    write(&root, "bots/zed.toml", "z");
    write(&root, "bots/alpha.toml", "a");
    write(&root, "triggers/nightly.toml", "t");
    write(&root, "policy.toml", "p");
    // Not definitions: another extension, a subdirectory, a stray file at the root.
    write(&root, "bots/notes.md", "n");
    write(&root, "bots/alpha.toml.bak", "n");
    write(&root, "bots/sub/deep.toml", "n");
    write(&root, "other.toml", "n");
    write(&root, "flows/x.toml", "n");

    let fs_defs = FsDefinitions::open(&root, DEFAULT_DEBOUNCE).unwrap();
    let snapshot = fs_defs.snapshot().await.unwrap();
    assert!(snapshot.problems.is_empty(), "{:?}", snapshot.problems);
    assert_eq!(
        names(&snapshot),
        [
            (DefKind::Bot, "alpha"),
            (DefKind::Bot, "zed"),
            (DefKind::Trigger, "nightly"),
            (DefKind::Policy, "policy"),
        ]
    );
    assert_eq!(snapshot.files[0].text, "a");
}

#[tokio::test]
async fn missing_directories_and_an_empty_root_are_an_empty_snapshot() {
    let (_dir, root) = temp_root();
    fs::create_dir(&root).unwrap();
    let fs_defs = FsDefinitions::open(&root, DEFAULT_DEBOUNCE).unwrap();
    assert_eq!(fs_defs.snapshot().await.unwrap(), DefSnapshot::default());
}

#[tokio::test]
async fn an_oversized_file_is_handed_over_so_the_loader_can_name_the_limit() {
    let (_dir, root) = temp_root();
    // Ends in a multi-byte character that the byte limit cuts in half.
    let text = format!("{}{}", "a".repeat(MAX_FILE_BYTES - 1), "é".repeat(100));
    write(&root, "bots/big.toml", &text);
    let fs_defs = FsDefinitions::open(&root, DEFAULT_DEBOUNCE).unwrap();
    let snapshot = fs_defs.snapshot().await.unwrap();
    assert!(snapshot.problems.is_empty(), "{:?}", snapshot.problems);
    let errors = load_set(&snapshot.files, &ValidationContext::with_builtin([])).unwrap_err();
    assert!(
        errors[0]
            .to_string()
            .starts_with("bots/big.toml: the file is "),
        "{errors:?}"
    );
}

#[tokio::test]
async fn a_file_that_is_not_utf8_is_a_problem() {
    let (_dir, root) = temp_root();
    fs::create_dir_all(root.join("bots")).unwrap();
    fs::write(root.join("bots/bad.toml"), [0xff, 0xfe, 0x00]).unwrap();
    write(&root, "bots/good.toml", "x");
    let fs_defs = FsDefinitions::open(&root, DEFAULT_DEBOUNCE).unwrap();
    let snapshot = fs_defs.snapshot().await.unwrap();
    assert_eq!(snapshot.problems.len(), 1);
    assert_eq!(
        snapshot.problems[0].to_string(),
        "bots/bad.toml: the file is not valid UTF-8"
    );
    assert_eq!(names(&snapshot), [(DefKind::Bot, "good")]);
}

#[tokio::test]
async fn a_directory_with_too_many_files_is_cut_and_reported() {
    let (_dir, root) = temp_root();
    fs::create_dir_all(root.join("bots")).unwrap();
    for n in 0..=MAX_FILES_PER_KIND {
        fs::write(root.join(format!("bots/b{n}.toml")), "x").unwrap();
    }
    let fs_defs = FsDefinitions::open(&root, DEFAULT_DEBOUNCE).unwrap();
    let snapshot = fs_defs.snapshot().await.unwrap();
    assert_eq!(snapshot.files.len(), MAX_FILES_PER_KIND);
    assert_eq!(snapshot.problems.len(), 1);
    assert!(
        snapshot.problems[0]
            .message
            .contains("holds more than 1024 files")
    );
}

#[tokio::test]
async fn a_symlink_to_a_directory_is_not_a_file() {
    let (_dir, root) = temp_root();
    fs::create_dir_all(root.join("bots/elsewhere")).unwrap();
    std::os::unix::fs::symlink(root.join("bots/elsewhere"), root.join("bots/link.toml")).unwrap();
    let fs_defs = FsDefinitions::open(&root, DEFAULT_DEBOUNCE).unwrap();
    assert!(fs_defs.snapshot().await.unwrap().files.is_empty());
}

#[tokio::test]
async fn a_root_that_is_gone_cannot_be_opened() {
    let (_dir, root) = temp_root();
    assert!(FsDefinitions::open(&root, DEFAULT_DEBOUNCE).is_err());
}

// ---- which events count -------------------------------------------------------------------

fn event(kind: EventKind, paths: &[&str]) -> Event {
    paths.iter().fold(Event::new(kind), |event, path| {
        event.add_path(PathBuf::from(path))
    })
}

#[test]
fn only_the_definition_files_count_as_edits() {
    let root = Path::new("/r");
    let create = EventKind::Create(CreateKind::File);
    let counts = |paths: &[&str]| is_relevant(&event(create, paths), root);

    for relevant in [
        &["/r/bots/a.toml"][..],
        &["/r/triggers/t.toml"],
        &["/r/policy.toml"],
        &["/r/bots"],
        &["/r/triggers"],
        &["/r"],
        &["/r/.git/index.lock", "/r/bots/a.toml"],
        &["/elsewhere/x"],
    ] {
        assert!(counts(relevant), "{relevant:?} should count");
    }
    for irrelevant in [
        &["/r/.git/index.lock"][..],
        &["/r/.git"],
        &["/r/scratch/2026-11-09/notes.md"],
        &["/r/.gitignore"],
        &["/r/policy.toml.swp"],
        &["/r/bots-old/a.toml"],
        &["/r/README.md"],
    ] {
        assert!(!counts(irrelevant), "{irrelevant:?} should not count");
    }
    assert!(
        is_relevant(&event(EventKind::Any, &[]), root),
        "unknown paths: assume it counts"
    );
    assert!(!is_relevant(
        &event(EventKind::Access(AccessKind::Any), &["/r/bots/a.toml"]),
        root
    ));
}

#[test]
fn a_rescan_notice_or_a_watcher_error_reloads_everything() {
    let root = Path::new("/r");
    let rescan = |paths: &[&str]| event(EventKind::Other, paths).set_flag(Flag::Rescan);

    // Not a definition path by itself, but the platform dropped events under it.
    for paths in [
        &["/r/scratch"][..],
        &["/r/.git"],
        &["/r"],
        &["/"],
        &["/elsewhere"],
        &[],
    ] {
        let notice = rescan(paths);
        assert!(signals_reload(&Ok(notice), root), "{paths:?} should reload");
    }
    // The same path without the flag is just a scratch file.
    assert!(!signals_reload(
        &Ok(event(EventKind::Other, &["/r/scratch"])),
        root
    ));
    // Even a read-kind event carrying the flag.
    let odd = event(EventKind::Access(AccessKind::Any), &["/r/scratch"]).set_flag(Flag::Rescan);
    assert!(signals_reload(&Ok(odd), root));

    let error = Err(notify::Error::generic("event buffer overflowed"));
    assert!(signals_reload(&error, root));
}

// ---- the debounce -------------------------------------------------------------------------

async fn tick() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_burst_is_one_signal_after_the_quiet() {
    let (raw_tx, raw_rx) = mpsc::channel(1);
    let (out_tx, mut out_rx) = mpsc::channel(1);
    let stop = CancellationToken::new();
    let task = tokio::spawn(debounce(
        raw_rx,
        out_tx,
        Duration::from_millis(100),
        stop.clone(),
    ));

    // Five signals 40 ms apart: each re-arms the deadline, so nothing fires during the burst.
    for _ in 0..5 {
        let _ = raw_tx.try_send(());
        tick().await;
        tokio::time::advance(Duration::from_millis(40)).await;
        tick().await;
        assert!(out_rx.try_recv().is_err(), "fired during the burst");
    }
    // 100 ms after the last signal, exactly one goes out.
    tokio::time::advance(Duration::from_millis(61)).await;
    tick().await;
    assert!(out_rx.try_recv().is_ok());
    assert!(out_rx.try_recv().is_err());

    // A later edit is a later signal.
    let _ = raw_tx.try_send(());
    tick().await;
    tokio::time::advance(Duration::from_millis(101)).await;
    tick().await;
    assert!(out_rx.try_recv().is_ok());

    stop.cancel();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_debounce_ends_when_the_watcher_goes() {
    let (raw_tx, raw_rx) = mpsc::channel(1);
    let (out_tx, mut out_rx) = mpsc::channel(1);
    let task = tokio::spawn(debounce(
        raw_rx,
        out_tx,
        Duration::from_millis(100),
        CancellationToken::new(),
    ));
    drop(raw_tx);
    task.await.unwrap();
    assert!(
        out_rx.recv().await.is_none(),
        "the kernel sees the watch end"
    );
}

// ---- the real watcher ---------------------------------------------------------------------

async fn next_signal(rx: &mut mpsc::Receiver<()>) {
    tokio::time::timeout(FAILSAFE, rx.recv())
        .await
        .expect("no change was reported")
        .expect("the watch ended");
}

/// Reads until `done` holds, re-reading on each signal. The failsafe only turns a hang into a
/// failure.
async fn until_snapshot(
    fs_defs: &FsDefinitions,
    changes: &mut mpsc::Receiver<()>,
    done: impl Fn(&DefSnapshot) -> bool,
) {
    tokio::time::timeout(FAILSAFE, async {
        while !done(&fs_defs.snapshot().await.unwrap()) {
            next_signal(changes).await;
        }
    })
    .await
    .expect("the change never showed up in a read");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_on_disk_is_reported_and_the_next_snapshot_has_it() {
    let _serial = crate::test_serial().await;
    let (_dir, root) = temp_root();
    bootstrap(&root, fixed(GitOutcome::Unavailable))
        .await
        .unwrap();
    let fs_defs = FsDefinitions::open(&root, Duration::from_millis(50)).unwrap();
    let mut changes = fs_defs.watch().unwrap();

    // A signal only says "read again" (it may be a rescan notice, or a late one for an earlier
    // write), so each step waits until a read shows the change rather than for one signal.
    write(&root, "bots/scout.toml", "name = \"scout\"\n");
    until_snapshot(&fs_defs, &mut changes, |s| {
        names(s).contains(&(DefKind::Bot, "scout"))
    })
    .await;

    // An edit to an existing file, and a delete, are changes too.
    write(&root, "bots/scout.toml", "name = \"scout\"\n# edited\n");
    until_snapshot(&fs_defs, &mut changes, |s| {
        s.files.iter().any(|f| f.text.contains("# edited"))
    })
    .await;

    fs::remove_file(root.join("bots/scout.toml")).unwrap();
    until_snapshot(&fs_defs, &mut changes, |s| {
        !names(s).contains(&(DefKind::Bot, "scout"))
    })
    .await;

    fs_defs.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_quick_writes_end_with_a_snapshot_that_has_all_of_them() {
    let _serial = crate::test_serial().await;
    let (_dir, root) = temp_root();
    bootstrap(&root, fixed(GitOutcome::Unavailable))
        .await
        .unwrap();
    let fs_defs = FsDefinitions::open(&root, Duration::from_millis(50)).unwrap();
    let mut changes = fs_defs.watch().unwrap();

    for n in 0..20 {
        write(&root, &format!("triggers/t{n}.toml"), "x");
    }
    // However the writes were batched, the last read has everything.
    until_snapshot(&fs_defs, &mut changes, |s| {
        s.files
            .iter()
            .filter(|f| f.kind == DefKind::Trigger)
            .count()
            == 20
    })
    .await;
    fs_defs.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_ends_the_watch() {
    let _serial = crate::test_serial().await;
    let (_dir, root) = temp_root();
    bootstrap(&root, fixed(GitOutcome::Unavailable))
        .await
        .unwrap();
    let fs_defs = FsDefinitions::open(&root, Duration::from_millis(50)).unwrap();
    let mut changes = fs_defs.watch().unwrap();
    fs_defs.shutdown().await;
    let ended = tokio::time::timeout(FAILSAFE, changes.recv())
        .await
        .unwrap();
    assert!(ended.is_none());
    // Twice is fine.
    fs_defs.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_watch_is_refused() {
    let _serial = crate::test_serial().await;
    let (_dir, root) = temp_root();
    bootstrap(&root, fixed(GitOutcome::Unavailable))
        .await
        .unwrap();
    let fs_defs = FsDefinitions::open(&root, Duration::from_millis(50)).unwrap();
    let _first = fs_defs.watch().unwrap();
    assert!(fs_defs.watch().is_err());
    fs_defs.shutdown().await;
}
