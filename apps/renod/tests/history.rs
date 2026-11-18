//! History and the Review upgrade through the daemon (RP3): `git.log`, `git.log_search`,
//! `workspace.file_sources`, and `workspace.diff` with scope `working_tree` and its checksum, over
//! the real socket, against a run's worktree in a real temporary git repository. Real `git`, no
//! network, no real agent. The test repository carries a hostile local git configuration, so
//! each setting a log or a diff is shaped by is shown to be pinned.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use common::{Fixture, id, within};
use futures::StreamExt;
use reno_client::ClientError;
use reno_proto::{
    CleanTarget, DiffScope, ErrorCode, FEATURES, FILE_SOURCE_MAX_BYTES, FileSourcesParams,
    GitLogParams, GitLogResult, GitLogRev, GitLogSearchParams, GitRefKind, RunId, RunStatus,
    TaskRunParams,
};

/// Seconds added to every test time: git reads a short number as something else.
const EPOCH: u64 = 1_700_000_000;

const AUTHORS: [(&str, &str); 3] = [
    ("Ada", "ada@example.invalid"),
    ("Bob", "bob@example.invalid"),
    ("Carol", "carol@example.invalid"),
];

fn git_as(dir: &Path, args: &[&str], who: (&str, &str), at: u64) -> String {
    let when = format!("{} +0000", EPOCH + at);
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", who.0)
        .env("GIT_AUTHOR_EMAIL", who.1)
        .env("GIT_COMMITTER_NAME", who.0)
        .env("GIT_COMMITTER_EMAIL", who.1)
        .env("GIT_AUTHOR_DATE", &when)
        .env("GIT_COMMITTER_DATE", &when)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    git_as(dir, args, AUTHORS[0], 1_000)
}

fn put(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Writes `rel` and commits all changes as `who` at `at` (seconds); the new commit's name.
fn commit(dir: &Path, rel: &str, text: &str, msg: &str, who: (&str, &str), at: u64) -> String {
    put(dir, rel, text);
    git_as(dir, &["add", "-A"], who, at);
    git_as(dir, &["commit", "-qm", msg], who, at);
    git_as(dir, &["rev-parse", "HEAD"], who, at)
}

/// The settings a log or a diff is shaped by, each set to a value that would break a parser that
/// did not pin its own.
fn hostile(dir: &Path) {
    for (key, value) in [
        ("log.showSignature", "true"),
        ("log.mailmap", "true"),
        ("log.decorate", "full"),
        ("log.date", "iso"),
        ("log.abbrevCommit", "true"),
        ("log.follow", "true"),
        ("log.excludeDecoration", "refs/tags/*"),
        ("format.pretty", "oneline"),
        ("i18n.logOutputEncoding", "ISO-8859-1"),
        ("grep.patternType", "perl"),
        ("core.abbrev", "4"),
        ("core.quotePath", "true"),
        ("color.ui", "always"),
        ("diff.renames", "false"),
        ("diff.noprefix", "true"),
        ("diff.mnemonicPrefix", "true"),
        ("status.renames", "false"),
    ] {
        git(dir, &["config", key, value]);
    }
}

/// What the base repository holds: its commits (oldest first).
struct Repo {
    dir: PathBuf,
    shas: Vec<String>,
}

/// A repository with three commits on `main`, a lightweight tag on the second, an annotated tag
/// and a remote-tracking branch on the third, and some files to change.
fn repo(fx: &Fixture) -> Repo {
    let dir = fx.dir.path().canonicalize().unwrap().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    hostile(&dir);
    let a = AUTHORS[0];
    let mut shas = Vec::new();
    shas.push(commit(
        &dir,
        "src/lib.rs",
        "fn alpha() {}\nfn beta() {}\n",
        "one",
        a,
        1_000,
    ));
    put(&dir, "README.md", "Alpha readme\n");
    put(&dir, "old.txt", "old\n");
    put(&dir, "docs/guide.md", "# Guide\n");
    put(&dir, "bin.dat", "bin\0ary\n");
    let big = "0123456789abcdef\n".repeat(75_000);
    shas.push(commit(&dir, "big.txt", &big, "two: files", a, 2_000));
    shas.push(commit(
        &dir,
        "extra.txt",
        "x\n",
        "three: fix(parser)",
        a,
        3_000,
    ));
    git(&dir, &["tag", "light", &shas[1]]);
    git(&dir, &["tag", "-a", "v1", "-m", "release one", &shas[2]]);
    git(&dir, &["update-ref", "refs/remotes/origin/main", &shas[2]]);
    git(
        &dir,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    Repo { dir, shas }
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

/// A live run in a worktree of `repo`: the run, the worktree and its branch.
async fn live(fx: &Fixture, repo: &Path) -> (RunId, PathBuf, String) {
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
    (run, info.path.canonicalize().unwrap(), info.branch)
}

fn log(run: &RunId) -> GitLogParams {
    GitLogParams {
        run_id: run.clone(),
        cursor: None,
        limit: None,
        rev: None,
    }
}

fn search(run: &RunId, query: &str) -> GitLogSearchParams {
    GitLogSearchParams {
        run_id: run.clone(),
        query: query.into(),
        limit: None,
        rev: None,
    }
}

fn sources(run: &RunId, path: &str, scope: Option<DiffScope>) -> FileSourcesParams {
    FileSourcesParams {
        run_id: run.clone(),
        path: path.into(),
        scope,
        checksum: None,
    }
}

fn code<T: std::fmt::Debug>(result: Result<T, ClientError>) -> ErrorCode {
    result
        .expect_err("an error")
        .code()
        .expect("a daemon error")
}

fn kind<T: std::fmt::Debug>(result: Result<T, ClientError>) -> String {
    let ClientError::Remote(object) = result.expect_err("an error") else {
        panic!("not a daemon error")
    };
    object.data.expect("typed")["kind"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Every page of the log, joined.
async fn all(fx: &Fixture, mut params: GitLogParams, per_page: u32) -> Vec<reno_proto::GitCommit> {
    params.limit = Some(per_page);
    let mut commits = Vec::new();
    loop {
        let page = within(fx.client.git_log(params.clone())).await.unwrap();
        assert!(page.commits.len() <= per_page as usize);
        commits.extend(page.commits);
        match page.next_cursor {
            Some(next) => params.cursor = Some(next),
            None => return commits,
        }
    }
}

/// Every commit comes before all its parents.
fn assert_topological(commits: &[reno_proto::GitCommit]) {
    let at = |sha: &str| commits.iter().position(|c| c.sha == sha);
    for (n, commit) in commits.iter().enumerate() {
        for parent in &commit.parents {
            if let Some(m) = at(parent) {
                assert!(n < m, "{} is listed after its parent {parent}", commit.sha);
            }
        }
    }
}

/// The worktree's history: Ada's and Bob's commits on the thread branch, a side branch by Carol
/// merged in. Returns the commits, oldest first, the merge last.
fn work(tree: &Path) -> Vec<String> {
    let (a, b, c) = (AUTHORS[0], AUTHORS[1], AUTHORS[2]);
    let branch = git(tree, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let w1 = commit(tree, "w1.txt", "one\n", "w1: first change", a, 4_000);
    git_as(tree, &["switch", "-q", "-c", "side"], c, 4_100);
    let s1 = commit(tree, "side.txt", "side\n", "s1: on the side", c, 4_200);
    git_as(tree, &["switch", "-q", &branch], b, 4_300);
    let w2 = commit(tree, "w2.txt", "two\n", "w2: second change", b, 4_400);
    git_as(
        tree,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
        b,
        4_500,
    );
    let merge = git(tree, &["rev-parse", "HEAD"]);
    vec![w1, s1, w2, merge]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_offers_the_history_feature() {
    let fx = Fixture::start().await;
    assert!(FEATURES.contains(&"history"));
    assert!(fx.client.supports("history"));
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_log_lists_a_threads_branch_with_its_merge_its_tags_and_its_refs() {
    let fx = Fixture::start().await;
    let base = repo(&fx);
    let (run, tree, branch) = live(&fx, &base.dir).await;
    let made = work(&tree);
    let merge = made.last().unwrap().clone();

    let page = within(fx.client.git_log(log(&run))).await.unwrap();
    assert!(page.repo && !page.in_place && !page.shallow);
    assert_eq!(page.head.as_deref(), Some(merge.as_str()));
    assert!(page.next_cursor.is_none());
    // Three base commits and four of the thread's (two on the thread branch, one on the side,
    // and the merge).
    assert_eq!(page.commits.len(), 7);
    assert_topological(&page.commits);
    let top = &page.commits[0];
    assert_eq!(top.sha, merge);
    assert_eq!(top.subject, "merge side");
    assert_eq!(top.parents.len(), 2);
    assert_eq!(top.parents[0], made[2]);
    assert_eq!(top.parents[1], made[1]);
    assert_eq!(top.author_name, "Bob");
    assert_eq!(top.time_ms, (EPOCH + 4_500) * 1000);

    // The refs: HEAD and the thread's branch on the merge; tags (the annotated one peeled to its
    // commit) and the remote branch on the base commits; origin/HEAD is not a ref of its own.
    let refs = |sha: &str| -> Vec<(String, GitRefKind)> {
        page.commits
            .iter()
            .find(|c| c.sha == sha)
            .unwrap()
            .refs
            .iter()
            .map(|r| (r.name.clone(), r.kind))
            .collect()
    };
    assert_eq!(
        refs(&merge),
        [
            ("HEAD".to_owned(), GitRefKind::Head),
            (branch.clone(), GitRefKind::Branch)
        ]
    );
    assert_eq!(
        refs(&base.shas[2]),
        [
            ("main".to_owned(), GitRefKind::Branch),
            ("v1".to_owned(), GitRefKind::Tag),
            ("origin/main".to_owned(), GitRefKind::Remote)
        ]
    );
    assert_eq!(refs(&base.shas[1]), [("light".to_owned(), GitRefKind::Tag)]);
    assert_eq!(refs(&made[1]), [("side".to_owned(), GitRefKind::Branch)]);

    // An author is a key, never an email.
    let wire = serde_json::to_string(&page).unwrap();
    assert!(!wire.contains("example.invalid"), "{wire}");
    assert_eq!(
        page.commits[0].author_key,
        reno_proto::author_key(AUTHORS[1].1)
    );
    assert_ne!(
        page.commits[0].author_key,
        page.commits.last().unwrap().author_key
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_log_pages_by_a_cursor_that_a_new_commit_does_not_shift() {
    let fx = Fixture::start().await;
    let base = repo(&fx);
    let (run, tree, _) = live(&fx, &base.dir).await;
    work(&tree);

    let whole = all(&fx, log(&run), 100).await;
    assert_eq!(whole.len(), 7);
    // The same list, three at a time.
    let paged = all(&fx, log(&run), 3).await;
    assert_eq!(paged, whole);
    assert_eq!(
        paged
            .iter()
            .map(|c| &c.sha)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        7
    );

    // A commit lands between page one and page two: the next page is still the next of the old
    // list, and the new commit is not in it.
    let mut first = log(&run);
    first.limit = Some(3);
    let one = within(fx.client.git_log(first.clone())).await.unwrap();
    let late = commit(&tree, "late.txt", "late\n", "late", AUTHORS[0], 9_000);
    first.cursor = one.next_cursor.clone();
    let two = within(fx.client.git_log(first.clone())).await.unwrap();
    let mut joined = one.commits.clone();
    joined.extend(two.commits.clone());
    assert_eq!(joined, whole[..6]);
    assert!(joined.iter().all(|c| c.sha != late));
    // A fresh listing starts from the new commit.
    let fresh = within(fx.client.git_log(log(&run))).await.unwrap();
    assert_eq!(fresh.commits[0].sha, late);
    assert_eq!(fresh.head.as_deref(), Some(late.as_str()));

    // The limit is clamped, and a cursor this daemon did not make is refused.
    let big = GitLogParams {
        limit: Some(10_000),
        ..log(&run)
    };
    assert_eq!(
        within(fx.client.git_log(big)).await.unwrap().commits.len(),
        8
    );
    for bad in [
        "nonsense",
        "h1.main.0",
        "h1.--output=x.1",
        "h2.0123456789012345678901234567890123456789.1",
        "h1.0123456789012345678901234567890123456789.1",
        "h1.0123456789012345678901234567890123456789.x",
    ] {
        let mut params = log(&run);
        params.cursor = Some(bad.into());
        assert_eq!(
            code(fx.client.git_log(params.clone()).await),
            ErrorCode::InvalidParams,
            "{bad}"
        );
        assert_eq!(kind(fx.client.git_log(params).await), "bad_cursor");
    }
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_log_can_be_scoped_to_the_thread_or_the_run() {
    let fx = Fixture::start().await;
    let base = repo(&fx);
    let (run, tree, _) = live(&fx, &base.dir).await;
    let made = work(&tree);

    let mut thread = log(&run);
    thread.rev = Some(GitLogRev::Thread);
    let own = all(&fx, thread.clone(), 100).await;
    let names: Vec<&str> = own.iter().map(|c| c.sha.as_str()).collect();
    assert_eq!(own.len(), 4);
    for sha in &made {
        assert!(names.contains(&sha.as_str()), "{sha}");
    }
    assert!(base.shas.iter().all(|s| !names.contains(&s.as_str())));
    // The first run started at the thread's base: the same commits.
    thread.rev = Some(GitLogRev::Run);
    assert_eq!(all(&fx, thread, 100).await, own);
    // Paging works inside a range too.
    let mut ranged = log(&run);
    ranged.rev = Some(GitLogRev::Thread);
    assert_eq!(all(&fx, ranged, 3).await, own);
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_finds_a_message_an_author_or_the_start_of_a_name() {
    let fx = Fixture::start().await;
    let base = repo(&fx);
    let (run, tree, _) = live(&fx, &base.dir).await;
    let made = work(&tree);
    let subjects = |found: &reno_proto::GitLogSearchResult| -> Vec<String> {
        found.commits.iter().map(|c| c.subject.clone()).collect()
    };

    // A message, any case.
    let found = within(fx.client.git_log_search(search(&run, "SECOND change")))
        .await
        .unwrap();
    assert_eq!(subjects(&found), ["w2: second change"]);
    assert!(found.repo && !found.truncated);
    // By author alone: Carol wrote one commit and no message says her name.
    let found = within(fx.client.git_log_search(search(&run, "carol")))
        .await
        .unwrap();
    assert_eq!(subjects(&found), ["s1: on the side"]);
    let found = within(fx.client.git_log_search(search(&run, "bob@example")))
        .await
        .unwrap();
    assert_eq!(subjects(&found), ["merge side", "w2: second change"]);
    // By the start of a name.
    let found = within(fx.client.git_log_search(search(&run, &made[0][..8])))
        .await
        .unwrap();
    assert_eq!(subjects(&found), ["w1: first change"]);
    // A text, not a pattern: the settings of the repository (`grep.patternType=perl`) do not turn
    // it into one, and `fix(` is not a broken regular expression.
    let found = within(fx.client.git_log_search(search(&run, "fix(")))
        .await
        .unwrap();
    assert_eq!(subjects(&found), ["three: fix(parser)"]);
    let found = within(fx.client.git_log_search(search(&run, ".*")))
        .await
        .unwrap();
    assert!(found.commits.is_empty());
    // An option is a text.
    let found = within(fx.client.git_log_search(search(&run, "--output=hacked")))
        .await
        .unwrap();
    assert!(found.commits.is_empty());
    assert!(!tree.join("hacked").exists());
    // A match carries its refs, and the scope limits it.
    let found = within(fx.client.git_log_search(search(&run, "fix(")))
        .await
        .unwrap();
    assert_eq!(found.commits[0].refs[0].name, "main");
    let mut scoped = search(&run, "fix(");
    scoped.rev = Some(GitLogRev::Thread);
    assert!(
        within(fx.client.git_log_search(scoped))
            .await
            .unwrap()
            .commits
            .is_empty()
    );
    let mut scoped = search(&run, &base.shas[2][..8]);
    scoped.rev = Some(GitLogRev::Thread);
    assert!(
        within(fx.client.git_log_search(scoped))
            .await
            .unwrap()
            .commits
            .is_empty()
    );
    // Newest first, and a limit says it cut.
    let mut some = search(&run, "e");
    some.limit = Some(2);
    let found = within(fx.client.git_log_search(some)).await.unwrap();
    assert_eq!(found.commits.len(), 2);
    assert!(found.truncated);
    assert!(found.commits[0].time_ms >= found.commits[1].time_ms);

    // A query that is no text.
    for bad in ["", "   ", "a\nb", &"x".repeat(201), "a\u{0}b"] {
        assert_eq!(
            code(fx.client.git_log_search(search(&run, bad)).await),
            ErrorCode::InvalidParams,
            "{bad:?}"
        );
    }
    assert_eq!(
        kind(fx.client.git_log_search(search(&run, "")).await),
        "bad_query"
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_with_no_worktree_is_served_from_its_folder_a_new_repository_and_a_shallow_one() {
    let fx = Fixture::start().await;
    let base = repo(&fx);
    let plain = fx.dir.path().canonicalize().unwrap().join("plain");
    put(&plain, "notes.txt", "hello\n");
    let run = within(fx.client.run_task(task(&plain, "mock", true)))
        .await
        .unwrap();
    fx.events(&run, None).await;
    assert!(fx.client.get_workspace(run.clone()).await.is_err());

    // Not a repository: no history, said plainly, not an error.
    let none = within(fx.client.git_log(log(&run))).await.unwrap();
    assert!(!none.repo && none.in_place && none.commits.is_empty() && none.head.is_none());
    let found = within(fx.client.git_log_search(search(&run, "a")))
        .await
        .unwrap();
    assert!(!found.repo && found.in_place && found.commits.is_empty());

    // A repository with no commits yet.
    git(&plain, &["init", "-q", "-b", "main"]);
    let fresh = within(fx.client.git_log(log(&run))).await.unwrap();
    assert!(fresh.repo && fresh.in_place && fresh.commits.is_empty() && fresh.head.is_none());
    assert!(
        within(fx.client.git_log_search(search(&run, "a")))
            .await
            .unwrap()
            .commits
            .is_empty()
    );

    // A shallow clone: its history stops short, and it says so.
    std::fs::remove_dir_all(&plain).unwrap();
    git(
        fx.dir.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", base.dir.display()),
            plain.to_str().unwrap(),
        ],
    );
    let shallow = within(fx.client.git_log(log(&run))).await.unwrap();
    assert!(shallow.repo && shallow.shallow && shallow.in_place);
    assert_eq!(shallow.commits.len(), 1);
    assert_eq!(shallow.commits[0].subject, "three: fix(parser)");
    // Served read-only from the folder, so a scope that needs the thread's worktree has none.
    for rev in [GitLogRev::Thread, GitLogRev::Run] {
        let mut scoped = log(&run);
        scoped.rev = Some(rev);
        assert_eq!(
            code(fx.client.git_log(scoped.clone()).await),
            ErrorCode::InvalidParams
        );
        assert_eq!(kind(fx.client.git_log(scoped).await), "needs_worktree");
    }
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removed_worktree_and_an_unknown_run_are_typed_errors_for_every_method() {
    let fx = Fixture::start().await;
    let base = repo(&fx);
    let (run, tree, _) = live(&fx, &base.dir).await;
    commit(&tree, "agent.txt", "work\n", "agent", AUTHORS[0], 5_000);
    // Alive: served.
    assert!(
        within(fx.client.git_log(log(&run)))
            .await
            .unwrap()
            .commits
            .len()
            > 3
    );
    within(fx.client.cancel_run(&run, None)).await.unwrap();
    fx.events(&run, None).await;
    // Ended, kept: still served.
    assert!(within(fx.client.git_log(log(&run))).await.unwrap().repo);

    within(fx.client.clean_workspaces(
        CleanTarget::Run {
            run_id: run.clone(),
        },
        true,
    ))
    .await
    .unwrap();
    assert!(!tree.exists());
    let gone = ErrorCode::WorkspaceUnavailable;
    assert_eq!(code(fx.client.git_log(log(&run)).await), gone);
    assert_eq!(
        code(fx.client.git_log_search(search(&run, "a")).await),
        gone
    );
    assert_eq!(
        code(
            fx.client
                .workspace_file_sources(sources(&run, "agent.txt", None))
                .await
        ),
        gone
    );
    let nobody = RunId::from_suffix("nobody").unwrap();
    let missing = ErrorCode::NotFound;
    assert_eq!(code(fx.client.git_log(log(&nobody)).await), missing);
    assert_eq!(
        code(fx.client.git_log_search(search(&nobody, "a")).await),
        missing
    );
    assert_eq!(
        code(
            fx.client
                .workspace_file_sources(sources(&nobody, "a", None))
                .await
        ),
        missing
    );
    fx.stop().await;
}

/// The thread's worktree for the file-source tests: a base with `src/lib.rs`, `README.md`,
/// `old.txt`, `docs/guide.md`, `bin.dat` and `big.txt`, and then
///
/// - committed (`w1`): `src/lib.rs` gains a function, `added.txt` is new, `old.txt` is deleted,
///   `docs/guide.md` becomes `docs/manual.md`;
/// - committed (`w2`): `src/lib.rs` gains another, so a commit and the worktree differ;
/// - not committed: `README.md` changes, `scratch.txt` is new, `bin.dat` changes, `big.txt`
///   grows.
async fn changed(fx: &Fixture) -> (RunId, PathBuf, Repo, [String; 2]) {
    let base = repo(fx);
    let (run, tree, _) = live(fx, &base.dir).await;
    let a = AUTHORS[0];
    git_as(&tree, &["rm", "-q", "old.txt"], a, 5_000);
    git_as(&tree, &["mv", "docs/guide.md", "docs/manual.md"], a, 5_000);
    put(&tree, "added.txt", "added\n");
    let w1 = commit(
        &tree,
        "src/lib.rs",
        "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n",
        "w1",
        a,
        5_100,
    );
    let w2 = commit(
        &tree,
        "src/lib.rs",
        "fn alpha() {}\nfn beta() {}\nfn gamma() {}\nfn delta() {}\n",
        "w2",
        a,
        5_200,
    );
    put(&tree, "README.md", "Alpha readme\nchanged\n");
    put(&tree, "scratch.txt", "scratch\n");
    put(&tree, "bin.dat", "bin\0changed\n");
    let big = format!("{}tail\n", "0123456789abcdef\n".repeat(75_000));
    put(&tree, "big.txt", &big);
    (run, tree, base, [w1, w2])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn working_tree_scope_is_what_is_not_committed_and_the_diff_carries_a_checksum() {
    let fx = Fixture::start().await;
    let (run, tree, _, _) = changed(&fx).await;

    // The thread's diff has everything; the working tree's only what no commit holds, untracked
    // files included.
    let thread = within(
        fx.client
            .workspace_diff_scoped(run.clone(), None, DiffScope::Thread),
    )
    .await
    .unwrap();
    let paths = |d: &reno_proto::WorkspaceDiffResult| -> Vec<String> {
        d.files.iter().map(|f| f.path.clone()).collect()
    };
    assert!(paths(&thread).contains(&"added.txt".to_owned()));
    assert!(!thread.checksum.is_empty());
    let dirty = within(
        fx.client
            .workspace_diff_scoped(run.clone(), None, DiffScope::WorkingTree),
    )
    .await
    .unwrap();
    assert_eq!(
        paths(&dirty),
        ["README.md", "big.txt", "bin.dat", "scratch.txt"]
    );
    assert_eq!(dirty.commits, 0);
    let head = git(&tree, &["rev-parse", "HEAD"]);
    assert_eq!(dirty.base_sha, head);
    // The patch of an untracked file, and one the commits touched is not there.
    let one = within(fx.client.workspace_diff_scoped(
        run.clone(),
        Some("scratch.txt".into()),
        DiffScope::WorkingTree,
    ))
    .await
    .unwrap();
    assert!(one.patch.unwrap().contains("+scratch"));
    let committed = within(fx.client.workspace_diff_scoped(
        run.clone(),
        Some("src/lib.rs".into()),
        DiffScope::WorkingTree,
    ))
    .await
    .unwrap();
    assert!(committed.patch.is_none() && committed.files.len() == 4);

    // The checksum changes when the diff does, and not otherwise.
    let ask = |path: Option<&str>| {
        let (client, run) = (fx.client.clone(), run.clone());
        let path = path.map(str::to_owned);
        async move {
            within(client.workspace_diff_scoped(run, path, DiffScope::WorkingTree))
                .await
                .unwrap()
        }
    };
    let before = ask(Some("scratch.txt")).await;
    assert_eq!(ask(Some("scratch.txt")).await.checksum, before.checksum);
    put(&tree, "scratch.txt", "scratch\nmore\n");
    let after = ask(Some("scratch.txt")).await;
    assert_ne!(after.checksum, before.checksum);
    // A change to another file moves the list's counts, not this patch: the checksum still moves
    // (the file list is part of it).
    put(&tree, "another.txt", "x\n");
    assert_ne!(ask(Some("scratch.txt")).await.checksum, after.checksum);
    // Nothing uncommitted, nothing in the list.
    git(&tree, &["add", "-A"]);
    git(&tree, &["commit", "-qm", "all in"]);
    let clean = ask(None).await;
    assert!(clean.files.is_empty());
    assert!(!clean.checksum.is_empty());
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_sources_gives_both_sides_of_a_file_for_each_scope() {
    let fx = Fixture::start().await;
    let (run, _tree, base, [w1, _w2]) = changed(&fx).await;
    let sources_of = |path: &'static str, scope: Option<DiffScope>| {
        let (client, run) = (fx.client.clone(), run.clone());
        async move {
            within(client.workspace_file_sources(sources(&run, path, scope)))
                .await
                .unwrap()
        }
    };

    // The thread: the base's blob against the worktree's file.
    let lib = sources_of("src/lib.rs", None).await;
    assert_eq!(
        lib.old_text.as_deref(),
        Some("fn alpha() {}\nfn beta() {}\n")
    );
    assert!(
        lib.new_text
            .as_deref()
            .unwrap()
            .ends_with("fn delta() {}\n")
    );
    assert_eq!(
        lib.old_hash,
        Some(reno_proto::image_hash(b"fn alpha() {}\nfn beta() {}\n"))
    );
    assert!(!lib.stale && !lib.binary && !lib.truncated && !lib.checksum.is_empty());
    // An added file has no old side, a deleted one no new side, a renamed one reads the old side
    // at its old path.
    let added = sources_of("added.txt", None).await;
    assert_eq!(
        (added.old_text, added.new_text.as_deref(), added.old_hash),
        (None, Some("added\n"), None)
    );
    let removed = sources_of("old.txt", None).await;
    assert_eq!(
        (
            removed.old_text.as_deref(),
            removed.new_text,
            removed.new_hash
        ),
        (Some("old\n"), None, None)
    );
    let moved = sources_of("docs/manual.md", None).await;
    assert_eq!(moved.old_path.as_deref(), Some("docs/guide.md"));
    assert_eq!(moved.old_text.as_deref(), Some("# Guide\n"));
    assert_eq!(moved.new_text.as_deref(), Some("# Guide\n"));
    // Untracked, and not committed.
    let scratch = sources_of("scratch.txt", None).await;
    assert_eq!(scratch.new_text.as_deref(), Some("scratch\n"));
    // The first run started at the thread's base: its scope shows the same.
    assert_eq!(
        sources_of("src/lib.rs", Some(DiffScope::Run))
            .await
            .old_text,
        lib.old_text
    );

    // One commit: its own two blobs, not the worktree.
    let by_commit = sources_of("src/lib.rs", Some(DiffScope::Commit { sha: w1.clone() })).await;
    assert_eq!(
        by_commit.old_text.as_deref(),
        Some("fn alpha() {}\nfn beta() {}\n")
    );
    assert_eq!(
        by_commit.new_text.as_deref(),
        Some("fn alpha() {}\nfn beta() {}\nfn gamma() {}\n")
    );
    // A commit that is not the thread's own (it is history before it), or no commit: refused.
    for sha in [base.shas[1].as_str(), "deadbeef00", "zzzz", "-rf"] {
        let refused = fx
            .client
            .workspace_file_sources(sources(
                &run,
                "src/lib.rs",
                Some(DiffScope::Commit { sha: sha.into() }),
            ))
            .await;
        assert_eq!(code(refused), ErrorCode::InvalidParams, "{sha}");
    }

    // The working tree: HEAD's blob against the file.
    let readme = sources_of("README.md", Some(DiffScope::WorkingTree)).await;
    assert_eq!(readme.old_text.as_deref(), Some("Alpha readme\n"));
    assert_eq!(readme.new_text.as_deref(), Some("Alpha readme\nchanged\n"));
    let fresh = sources_of("scratch.txt", Some(DiffScope::WorkingTree)).await;
    assert_eq!(
        (fresh.old_text, fresh.new_text.as_deref()),
        (None, Some("scratch\n"))
    );
    // A file that only a commit changed is not a change of the working tree.
    let refused = fx
        .client
        .workspace_file_sources(sources(&run, "src/lib.rs", Some(DiffScope::WorkingTree)))
        .await;
    assert_eq!(code(refused), ErrorCode::NotFound);
    let refused = fx
        .client
        .workspace_file_sources(sources(&run, "src/lib.rs", Some(DiffScope::WorkingTree)))
        .await;
    assert_eq!(kind(refused), "not_changed");
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_sources_marks_binary_cuts_large_files_refuses_bad_paths_and_says_when_stale() {
    let fx = Fixture::start().await;
    let (run, tree, _, _) = changed(&fx).await;

    // Binary: no text and no hashes on either side.
    let bin = within(
        fx.client
            .workspace_file_sources(sources(&run, "bin.dat", None)),
    )
    .await
    .unwrap();
    assert!(bin.binary && bin.old_text.is_none() && bin.new_text.is_none());
    assert!(bin.old_hash.is_none() && bin.new_hash.is_none());

    // Large: each side is cut at 1 MiB and says so.
    let big = within(
        fx.client
            .workspace_file_sources(sources(&run, "big.txt", None)),
    )
    .await
    .unwrap();
    assert!(big.truncated && !big.binary);
    for text in [
        big.old_text.as_ref().unwrap(),
        big.new_text.as_ref().unwrap(),
    ] {
        assert!(text.len() <= FILE_SOURCE_MAX_BYTES && text.len() > FILE_SOURCE_MAX_BYTES - 64);
    }

    // The path is the sandbox's: refused before anything is read, and a file the diff does not
    // list is not found.
    for bad in ["../x", "/etc/passwd", ".git/config", "src/../../x"] {
        assert_eq!(
            code(
                fx.client
                    .workspace_file_sources(sources(&run, bad, None))
                    .await
            ),
            ErrorCode::PathRejected,
            "{bad}"
        );
    }
    assert_eq!(
        code(
            fx.client
                .workspace_file_sources(sources(&run, "no/such.rs", None))
                .await
        ),
        ErrorCode::NotFound
    );
    // A link that leaves the tree is not followed to its target.
    let outside = fx.dir.path().canonicalize().unwrap().join("outside.txt");
    std::fs::write(&outside, "secret outside\n").unwrap();
    std::os::unix::fs::symlink(&outside, tree.join("leak.txt")).unwrap();
    assert_eq!(
        code(
            fx.client
                .workspace_file_sources(sources(&run, "leak.txt", None))
                .await
        ),
        ErrorCode::PathRejected
    );

    // Stale: the checksum of the diff the client holds, for this path and scope.
    let diff = within(fx.client.workspace_diff_scoped(
        run.clone(),
        Some("scratch.txt".into()),
        DiffScope::Thread,
    ))
    .await
    .unwrap();
    let mut asked = sources(&run, "scratch.txt", None);
    asked.checksum = Some(diff.checksum.clone());
    let now = within(fx.client.workspace_file_sources(asked.clone()))
        .await
        .unwrap();
    assert!(!now.stale);
    assert_eq!(now.checksum, diff.checksum);
    put(&tree, "scratch.txt", "scratch\nedited\n");
    let later = within(fx.client.workspace_file_sources(asked.clone()))
        .await
        .unwrap();
    assert!(later.stale);
    assert_eq!(later.new_text.as_deref(), Some("scratch\nedited\n"));
    assert_ne!(later.checksum, diff.checksum);
    // Passing the new checksum back is current again; a made-up one is stale.
    asked.checksum = Some(later.checksum.clone());
    assert!(
        !within(fx.client.workspace_file_sources(asked.clone()))
            .await
            .unwrap()
            .stale
    );
    asked.checksum = Some("0".repeat(64));
    assert!(
        within(fx.client.workspace_file_sources(asked))
            .await
            .unwrap()
            .stale
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_served_from_its_folder_has_no_diff_to_read() {
    let fx = Fixture::start().await;
    let plain = fx.dir.path().canonicalize().unwrap().join("plain");
    put(&plain, "notes.txt", "hello\n");
    let run = within(fx.client.run_task(task(&plain, "mock", true)))
        .await
        .unwrap();
    fx.events(&run, None).await;
    assert_eq!(
        code(
            fx.client
                .workspace_file_sources(sources(&run, "notes.txt", None))
                .await
        ),
        ErrorCode::WorkspaceUnavailable
    );
    fx.stop().await;
}

const TOKEN: &str = "ghp_exampleExampleExample1234";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_is_redacted_in_the_sources_of_a_file_and_in_a_subject() {
    use renod::secrets::{KeychainSecrets, MemoryKeychain};
    let secrets = Arc::new(KeychainSecrets::new(Arc::new(MemoryKeychain::new())));
    let fx = Fixture::with(|c| c.secrets = Some(secrets)).await;
    let mut watch = within(fx.client.watch_defs()).await.unwrap();
    let path = fx.paths.defs_dir.join("bots/keeper.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"mock-hold\"]\nsecrets = [\"GITHUB_TOKEN\"]\n",
    )
    .unwrap();
    within(async {
        while let Some(snapshot) = watch.next().await {
            if snapshot
                .unwrap()
                .defs
                .iter()
                .any(|d| d.kind == reno_proto::defs::DefKind::Bot && d.name == "keeper")
            {
                return;
            }
        }
        panic!("the watch ended");
    })
    .await;
    within(fx.client.set_secret("GITHUB_TOKEN", TOKEN))
        .await
        .unwrap();

    let base = repo(&fx);
    // The base holds a file with the value in it, so the old side has it too.
    commit(
        &base.dir,
        "leak.env",
        &format!("GITHUB_TOKEN={TOKEN}\n"),
        "env",
        AUTHORS[0],
        3_500,
    );
    let mut spec = task(&base.dir, "mock-hold", false);
    spec.executor = None;
    spec.bot = Some("keeper".into());
    let run = within(fx.client.run_task(spec)).await.unwrap();
    until_running(&fx, &run).await;
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
    put(
        &info.path,
        "leak.env",
        &format!("GITHUB_TOKEN={TOKEN}\nmore={TOKEN}\n"),
    );
    git_as(
        &info.path,
        &["commit", "-qam", &format!("rotate {TOKEN}")],
        AUTHORS[0],
        6_000,
    );
    put(&info.path, "leak.env", &format!("again={TOKEN}\n"));

    let file = within(
        fx.client
            .workspace_file_sources(sources(&run, "leak.env", None)),
    )
    .await
    .unwrap();
    assert_eq!(
        file.old_text.as_deref(),
        Some("GITHUB_TOKEN=[secret:GITHUB_TOKEN]\n")
    );
    assert_eq!(
        file.new_text.as_deref(),
        Some("again=[secret:GITHUB_TOKEN]\n")
    );
    assert!(!serde_json::to_string(&file).unwrap().contains(TOKEN));

    let page = within(fx.client.git_log(log(&run))).await.unwrap();
    assert_eq!(page.commits[0].subject, "rotate [secret:GITHUB_TOKEN]");
    assert!(!serde_json::to_string(&page).unwrap().contains(TOKEN));
    let found = within(fx.client.git_log_search(search(&run, "rotate")))
        .await
        .unwrap();
    assert_eq!(found.commits[0].subject, "rotate [secret:GITHUB_TOKEN]");
    assert!(!serde_json::to_string(&found).unwrap().contains(TOKEN));
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_run_with_no_commits_yet_shows_the_history_it_was_cut_from() {
    // Nothing started: no folder to read.
    let fx = Fixture::start().await;
    let base = repo(&fx);
    let (run, _tree, _) = live(&fx, &base.dir).await;
    let page: GitLogResult = within(fx.client.git_log(log(&run))).await.unwrap();
    assert_eq!(page.commits.len(), 3);
    fx.stop().await;
}
