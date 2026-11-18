//! Git worktrees for write runs (T1.3): the `git` CLI as a subprocess, never libgit2.
//!
//! # Layout
//!
//! ```text
//! <root>/                       the data directory's `worktrees/`
//!   <repo key>/<thread id>/     one worktree per thread, on the branch `reno/<thread id>`
//! ```
//!
//! The **repo key** is `<name>-<12 hex>`: `<name>` is the main working tree's directory name (the
//! parent of `.git`; for a bare repository its own name) with every
//! character outside `[A-Za-z0-9._-]` replaced by `_` (at most 32 characters), and the hex is the
//! first 12 digits of the SHA-256 of the canonical path of the repository's *common git
//! directory*. The hash makes two repositories that share a directory name collision-free; using
//! the common directory makes every linked worktree of one repository share one key, and keeps
//! the key stable whatever subdirectory a task targets.
//!
//! # The user's checkout is never touched
//!
//! In the user's checkout this module runs only `rev-parse`, `worktree add/remove/prune/list`,
//! `merge-base --is-ancestor`, `rev-list` and `update-ref` on `refs/heads/reno/*`. It never
//! checks out, switches, stashes, resets, cleans or configures anything there, and it never runs
//! `status` there (`status` can refresh the index). `status` runs only inside a Reno worktree.
//! (Inside a run's own worktree, never the user's checkout, `protected_changes` and
//! `revert_protected` also run `diff`, `ls-files`, `ls-tree`, `checkout`, `rm --cached`, `clean`
//! and `commit`, to undo a write to a protected path; `changes_since_base` runs only read-only
//! `diff`, `ls-files` and `rev-list` there, never `add`, and never follows a symlink.)
//! The branch is cut from a commit sha resolved first, so the user's branch never moves. Git hooks
//! are disabled for the commands Reno runs, so a user's `post-checkout` hook does not fire in a
//! worktree an agent will work in.
//!
//! Uncommitted changes in the user's checkout are not in the worktree: it starts from the `HEAD`
//! commit.
//!
//! # Subprocess hygiene (ARCHITECTURE 17.3)
//!
//! Each `git` is spawned through `process::owned::spawn_grouped`, so it leads its own process
//! group and a timeout ends the whole group. Standard input is closed, prompts are off
//! (`GIT_TERMINAL_PROMPT=0`), the environment that could redirect git (`GIT_DIR`,
//! `GIT_WORK_TREE`, `GIT_INDEX_FILE`...) is removed, and output is captured up to a bound; the
//! rest is read and dropped so the child never blocks on a full pipe.
//!
//! # Locking
//!
//! `worktree add`, `remove`, `prune` and ref deletion take locks inside the repository's `.git`.
//! One async mutex per repository (keyed by its canonical common directory) serialises them, so
//! two runs on one repository work side by side and a second repository is never held up.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use reno_proto::{ChangeKind, ChangedFile};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::process::Command;
use crate::process::owned::spawn_grouped;

/// Bytes of each of a git command's stdout and stderr that are kept.
const OUTPUT_CAP: usize = 1 << 20;
/// How long a killed git gets to end before its group is killed.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// The default bound on one git command.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Why a git operation failed. Typed, so a caller can tell a refusal from a fault.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// `git` could not be started.
    #[error("cannot run git: {0}")]
    GitMissing(String),
    /// The directory is not inside a git repository.
    #[error("{} is not inside a git repository", dir.display())]
    NotARepo {
        /// The directory.
        dir: PathBuf,
    },
    /// The repository is bare: it has no working tree to branch from.
    #[error("{} is a bare repository", repo.display())]
    Bare {
        /// The repository.
        repo: PathBuf,
    },
    /// The repository has no commits yet, so there is nothing to cut a branch from.
    #[error("the repository at {} has no commits", repo.display())]
    NoCommits {
        /// The repository.
        repo: PathBuf,
    },
    /// A value that must not reach git (a run id that is not a safe ref name).
    #[error("refused: {0}")]
    Unsafe(String),
    /// The worktree's path is taken by something that is not this run's worktree.
    #[error("{} exists and is not this run's worktree", path.display())]
    PathTaken {
        /// The path.
        path: PathBuf,
    },
    /// A git command failed.
    #[error("git {op} failed ({code}): {stderr}")]
    Failed {
        /// The command, such as `worktree add`.
        op: String,
        /// Its exit code, or `signal` when it was killed by one.
        code: String,
        /// The tail of what it printed to stderr.
        stderr: String,
    },
    /// A git command ran past its timeout and was ended.
    #[error("git {op} timed out after {secs}s")]
    Timeout {
        /// The command.
        op: String,
        /// The bound, in seconds.
        secs: u64,
    },
    /// A file system call failed.
    #[error("{what}: {message}")]
    Io {
        /// What was being done.
        what: String,
        /// The error.
        message: String,
    },
}

/// Where a run's worktree is: everything needed to find, inspect and remove it later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The repository's working tree root.
    pub repo: PathBuf,
    /// The repository's key in the worktrees directory.
    pub repo_key: String,
    /// The worktree's root.
    pub path: PathBuf,
    /// The directory the task targeted, relative to the repository root; empty for the root.
    pub subdir: String,
    /// The branch, `reno/<thread id>`.
    pub branch: String,
    /// The commit the branch is cut from.
    pub base_sha: String,
}

impl Location {
    /// The run's working directory: the worktree plus the targeted subdirectory.
    pub fn cwd(&self) -> PathBuf {
        if self.subdir.is_empty() {
            self.path.clone()
        } else {
            self.path.join(&self.subdir)
        }
    }
}

/// What a worktree holds beyond its base commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Changes {
    /// Commits the base does not have.
    pub commits: u32,
    /// Uncommitted changes or untracked files.
    pub dirty: bool,
}

impl Changes {
    /// Nothing to lose.
    pub fn is_clean(&self) -> bool {
        self.commits == 0 && !self.dirty
    }
}

/// What [`GitWorktrees::inspect`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inspection {
    /// The worktree directory exists.
    pub exists: bool,
    /// The branch exists.
    pub branch_exists: bool,
    /// What it holds. Dirtiness is unknown (`false`) when the directory is gone.
    pub changes: Changes,
}

/// How [`GitWorktrees::release`] decides what may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// Remove only what holds nothing to lose: no commits beyond the base, nothing uncommitted.
    IfClean,
    /// Also remove what is merged: the branch tip is reachable from the repository's `HEAD`, and
    /// the worktree has no uncommitted changes.
    IfMerged,
    /// Remove regardless, deleting the branch and losing whatever it held.
    Force,
}

/// What [`GitWorktrees::release`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Released {
    /// The worktree and the branch are gone.
    Removed,
    /// Left as it was, because of what it holds.
    Kept(Changes),
    /// The directory was already gone and was pruned; the branch was kept because it holds
    /// commits.
    GoneBranchKept(Changes),
}

/// The git worktree manager. Cheap to share: wrap it in an `Arc`.
#[derive(Debug)]
pub struct GitWorktrees {
    root: PathBuf,
    timeout: Duration,
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

struct Output {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

impl Output {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

impl GitWorktrees {
    /// A manager that keeps worktrees under `root` (the data directory's `worktrees/`).
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            timeout: DEFAULT_TIMEOUT,
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// The same manager with `timeout` as the bound on each git command.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The worktrees directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The repository key of the repository whose common git directory is `common_dir`: see the
    /// module docs.
    pub fn repo_key(common_dir: &Path) -> String {
        // The main working tree's name: the parent of `.git`, whichever linked worktree asks.
        let named = if common_dir.file_name().is_some_and(|n| n == ".git") {
            common_dir.parent().unwrap_or(common_dir)
        } else {
            common_dir
        };
        let name: String = named
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .take(32)
            .collect();
        let name = name.trim_matches('.');
        let name = if name.is_empty() { "repo" } else { name };
        let digest = Sha256::digest(common_dir.as_os_str().as_encoded_bytes());
        let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
        format!("{name}-{hex}")
    }

    /// Whether `dir` is inside a git repository with a working tree. `Ok(None)` means it is
    /// not; a bare repository is an error.
    pub async fn probe(&self, dir: &Path) -> Result<Option<Probe>, GitError> {
        let common = self
            .git(
                dir,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await?;
        if !common.ok() {
            if common.stderr.contains("not a git repository") {
                return Ok(None);
            }
            return Err(failed("rev-parse", &common));
        }
        let bare = self
            .git(dir, &["rev-parse", "--is-bare-repository"])
            .await?;
        if bare.ok() && bare.stdout.trim() == "true" {
            return Err(GitError::Bare {
                repo: PathBuf::from(common.stdout.trim()),
            });
        }
        let top = self.git(dir, &["rev-parse", "--show-toplevel"]).await?;
        if !top.ok() {
            // Inside the `.git` directory itself: there is no working tree here.
            return Ok(None);
        }
        let prefix = self.git(dir, &["rev-parse", "--show-prefix"]).await?;
        if !prefix.ok() {
            return Err(failed("rev-parse", &prefix));
        }
        let toplevel = canonical(Path::new(top.stdout.trim())).await?;
        let common_dir = canonical(Path::new(common.stdout.trim())).await?;
        Ok(Some(Probe {
            toplevel,
            common_dir,
            subdir: prefix.stdout.trim().trim_end_matches('/').to_owned(),
        }))
    }

    /// Plans the worktree named `run` (a thread id: the directory and the branch carry the
    /// name): where it will be and which commit it starts from. Reads the repository
    /// (`rev-parse` only) and writes nothing.
    ///
    /// `Ok(None)` means `dir` is not in a git repository.
    pub async fn plan(&self, run: &str, dir: &Path) -> Result<Option<Location>, GitError> {
        check_run_id(run)?;
        let Some(probe) = self.probe(dir).await? else {
            return Ok(None);
        };
        let head = self
            .git(
                &probe.toplevel,
                &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
            )
            .await?;
        if !head.ok() {
            return Err(GitError::NoCommits {
                repo: probe.toplevel,
            });
        }
        let repo_key = Self::repo_key(&probe.common_dir);
        Ok(Some(Location {
            path: self.root.join(&repo_key).join(run),
            repo: probe.toplevel,
            repo_key,
            subdir: probe.subdir,
            branch: branch_of(run),
            base_sha: head.stdout.trim().to_owned(),
        }))
    }

    /// Creates the worktree and branch of `loc`. Idempotent: a worktree that already exists at
    /// the path on that branch is accepted, and a branch left by an earlier attempt is reused.
    pub async fn create(&self, loc: &Location) -> Result<(), GitError> {
        let _guard = self.lock(&loc.repo).await?;
        if tokio::fs::try_exists(&loc.path).await.unwrap_or(false) {
            return if self.registered(loc).await? {
                self.ensure_subdir(loc).await
            } else {
                Err(GitError::PathTaken {
                    path: loc.path.clone(),
                })
            };
        }
        if let Some(parent) = loc.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| io("create the worktrees directory", &e))?;
        }
        // A worktree whose directory vanished still blocks `add`: forget it first.
        self.expect_ok(&loc.repo, &["worktree", "prune"], "worktree prune")
            .await?;
        let path = loc.path.to_string_lossy().into_owned();
        let branch_ref = format!("refs/heads/{}", loc.branch);
        let have_branch = self
            .git(
                &loc.repo,
                &["rev-parse", "--verify", "--quiet", &branch_ref],
            )
            .await?
            .ok();
        if have_branch {
            self.expect_ok(
                &loc.repo,
                &["worktree", "add", "--quiet", &path, &loc.branch],
                "worktree add",
            )
            .await?;
        } else {
            self.expect_ok(
                &loc.repo,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "--no-track",
                    "-b",
                    &loc.branch,
                    &path,
                    &loc.base_sha,
                ],
                "worktree add",
            )
            .await?;
        }
        self.ensure_subdir(loc).await
    }

    /// What the worktree and branch of `loc` hold now.
    pub async fn inspect(&self, loc: &Location) -> Result<Inspection, GitError> {
        let exists = tokio::fs::try_exists(&loc.path).await.unwrap_or(false);
        let branch_ref = format!("refs/heads/{}", loc.branch);
        let branch_exists = self
            .git(
                &loc.repo,
                &["rev-parse", "--verify", "--quiet", &branch_ref],
            )
            .await?
            .ok();
        let range = format!("{}..{}", loc.base_sha, branch_ref);
        let mut commits = 0;
        let mut dirty = false;
        if exists {
            let head_range = format!("{}..HEAD", loc.base_sha);
            commits = self.count(&loc.path, &head_range).await?;
            let status = self
                .git(
                    &loc.path,
                    &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
                )
                .await?;
            if !status.ok() {
                return Err(failed("status", &status));
            }
            dirty = !status.stdout.is_empty();
        } else if branch_exists {
            commits = self.count(&loc.repo, &range).await?;
        }
        Ok(Inspection {
            exists,
            branch_exists,
            changes: Changes { commits, dirty },
        })
    }

    /// Removes the worktree and branch of `loc` as `how` allows. See [`Release`] and
    /// [`Released`]. A branch is deleted only by compare-and-delete against the commit it was
    /// seen at, so a commit that lands in between is never lost; `Force` is the one exception.
    pub async fn release(&self, loc: &Location, how: Release) -> Result<Released, GitError> {
        let seen = self.inspect(loc).await?;
        let branch_ref = format!("refs/heads/{}", loc.branch);
        let tip = if seen.branch_exists {
            let out = self
                .git(&loc.repo, &["rev-parse", "--verify", &branch_ref])
                .await?;
            out.ok().then(|| out.stdout.trim().to_owned())
        } else {
            None
        };
        if how != Release::Force {
            let mut removable = seen.changes.is_clean();
            if !removable && how == Release::IfMerged && !seen.changes.dirty {
                removable = match &tip {
                    Some(tip) => self.is_ancestor(&loc.repo, tip).await?,
                    None => false,
                };
            }
            if !removable {
                return Ok(if seen.exists {
                    Released::Kept(seen.changes)
                } else {
                    // The directory is gone: forget the registration, keep the branch.
                    let _guard = self.lock(&loc.repo).await?;
                    self.expect_ok(&loc.repo, &["worktree", "prune"], "worktree prune")
                        .await?;
                    Released::GoneBranchKept(seen.changes)
                });
            }
        }
        let _guard = self.lock(&loc.repo).await?;
        if seen.exists {
            let path = loc.path.to_string_lossy().into_owned();
            let mut args = vec!["worktree", "remove"];
            if how == Release::Force {
                args.push("--force");
            }
            args.push(&path);
            let removed = self.git(&loc.repo, &args).await?;
            if !removed.ok() {
                // Something changed under us (a new file, a lock): look again, keep it.
                let now = self.inspect(loc).await?;
                if now.exists && how != Release::Force {
                    return Ok(Released::Kept(now.changes));
                }
                return Err(failed("worktree remove", &removed));
            }
        }
        self.expect_ok(&loc.repo, &["worktree", "prune"], "worktree prune")
            .await?;
        if let Some(tip) = tip {
            let deleted = if how == Release::Force {
                self.git(&loc.repo, &["update-ref", "-d", &branch_ref])
                    .await?
            } else {
                self.git(&loc.repo, &["update-ref", "-d", &branch_ref, &tip])
                    .await?
            };
            if !deleted.ok() {
                return Err(failed("update-ref", &deleted));
            }
        }
        // Leave no empty `<repo key>/` directory behind; harmless if it is not empty.
        if let Some(parent) = loc.path.parent() {
            let _ = tokio::fs::remove_dir(parent).await;
        }
        Ok(Released::Removed)
    }

    /// The protected paths (see `reno_proto::defs::is_protected_relative`) the worktree of `loc`
    /// holds different from its base commit: a tracked file changed, deleted or committed since
    /// the base, and any untracked file, ignored or not. Relative to the worktree root,
    /// `/`-separated, sorted. Runs only inside the run's own worktree, and writes nothing.
    pub async fn protected_changes(&self, loc: &Location) -> Result<Vec<String>, GitError> {
        let base = loc.base_sha.as_str();
        // Tracked paths against the base: staged, unstaged and committed alike.
        let tracked = self
            .names(
                &loc.path,
                &["diff", "--name-only", "-z", "--no-renames", base, "--"],
            )
            .await?;
        let untracked = self
            .names(
                &loc.path,
                &["ls-files", "-z", "--others", "--exclude-standard"],
            )
            .await?;
        // Ignored files are not listed above; look for them only where a protected name can
        // start, so the walk never enters `target/` or `node_modules/`.
        let ignored = self
            .names(
                &loc.path,
                &[
                    "ls-files",
                    "-z",
                    "--others",
                    "--ignored",
                    "--exclude-standard",
                    "--",
                    ":(icase,literal).claude",
                    ":(icase,literal).codex",
                    ":(icase,literal).reno",
                    ":(icase,literal).mcp.json",
                ],
            )
            .await?;
        let mut found: Vec<String> = tracked
            .into_iter()
            .chain(untracked)
            .chain(ignored)
            .filter(|path| reno_proto::defs::is_protected_relative(path))
            .collect();
        found.sort();
        found.dedup();
        Ok(found)
    }

    /// Puts `paths` of the worktree of `loc` back as the base commit has them: restored where
    /// the base holds the path, removed (from the index and the disk) where it does not. A run
    /// that committed one of them gets one more commit on its branch that undoes it. Only paths
    /// that are protected and inside the worktree are touched; anything else is refused.
    pub async fn revert_protected(&self, loc: &Location, paths: &[String]) -> Result<(), GitError> {
        for path in paths {
            let clean = !path.is_empty()
                && !path.starts_with('/')
                && !path.split('/').any(|part| part.is_empty() || part == "..")
                && !path.contains('\0')
                && reno_proto::defs::is_protected_relative(path);
            if !clean {
                return Err(GitError::Unsafe(format!(
                    "{path:?} is not a protected path inside the worktree"
                )));
            }
        }
        if paths.is_empty() {
            return Ok(());
        }
        let base = loc.base_sha.as_str();
        // Which of them the base has.
        let mut ls = vec!["ls-tree", "-r", "-z", "--name-only", base, "--"];
        ls.extend(paths.iter().map(String::as_str));
        let in_base: HashSet<String> = self.names(&loc.path, &ls).await?.into_iter().collect();
        let (present, absent): (Vec<&String>, Vec<&String>) =
            paths.iter().partition(|path| in_base.contains(*path));
        if !present.is_empty() {
            let mut args = vec!["checkout", "-q", base, "--"];
            args.extend(present.iter().map(|path| path.as_str()));
            self.expect_ok(&loc.path, &args, "checkout").await?;
        }
        if !absent.is_empty() {
            let mut rm = vec!["rm", "-q", "-r", "-f", "--cached", "--ignore-unmatch", "--"];
            rm.extend(absent.iter().map(|path| path.as_str()));
            self.expect_ok(&loc.path, &rm, "rm").await?;
            let mut clean = vec!["clean", "-q", "-f", "-d", "-x", "--"];
            clean.extend(absent.iter().map(|path| path.as_str()));
            self.expect_ok(&loc.path, &clean, "clean").await?;
            // The directories the run made for them are left empty; git does not track those.
            for path in &absent {
                let mut dir = loc.path.join(path);
                while dir.pop() && dir != loc.path {
                    if tokio::fs::remove_dir(&dir).await.is_err() {
                        break;
                    }
                }
            }
        }
        // A path the run committed is still changed at `HEAD`: undo it in a commit of its own.
        let mut committed = vec![
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            base,
            "HEAD",
            "--",
        ];
        committed.extend(paths.iter().map(String::as_str));
        if !self.names(&loc.path, &committed).await?.is_empty() {
            let mut commit = vec![
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Reno",
                "-c",
                "user.email=reno@localhost",
                "commit",
                "-q",
                "--no-verify",
                "--allow-empty-message",
                "-m",
                "reno: revert a change to protected paths",
                "--only",
                "--",
            ];
            commit.extend(paths.iter().map(String::as_str));
            self.expect_ok(&loc.path, &commit, "commit").await?;
        }
        Ok(())
    }

    /// What the run changed in the worktree of `loc` against its base commit: committed, staged,
    /// unstaged and untracked files (ignored files are not changes), and, for `path`, that
    /// file's patch cut at [`PATCH_CAP`]. Reads only: no `add`, no index write
    /// (`GIT_OPTIONAL_LOCKS=0`), no external diff driver or textconv. A worktree whose directory
    /// is gone has no changes. `path` must be a clean repository-relative path and name a
    /// changed file, else there is no patch; a symlink is never followed.
    pub async fn changes_since_base(
        &self,
        loc: &Location,
        path: Option<&str>,
    ) -> Result<RunChanges, GitError> {
        self.changes_against(loc, &loc.base_sha, None, path).await
    }

    /// The commit the worktree of `loc` stands at now (`HEAD`).
    pub async fn head(&self, loc: &Location) -> Result<String, GitError> {
        let out = self
            .git(&loc.path, &["rev-parse", "--verify", "HEAD"])
            .await?;
        if !out.ok() {
            return Err(failed("rev-parse HEAD", &out));
        }
        Ok(out.stdout.trim().to_owned())
    }

    /// What one commit of the branch of `loc` changed against its parent (a root commit, against
    /// the empty tree), and the patch of `path` in it. The commit must be one of the branch's
    /// own: after `loc.base_sha` and reachable from `HEAD`, else [`GitError::Unsafe`]. Reads
    /// only. The name is resolved to a full commit id first, so nothing but a hexadecimal name
    /// reaches git as a revision.
    pub async fn commit_changes(
        &self,
        loc: &Location,
        sha: &str,
        path: Option<&str>,
    ) -> Result<RunChanges, GitError> {
        let hex = (7..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex {
            return Err(GitError::Unsafe(format!("{sha:?} is not a commit name")));
        }
        if !tokio::fs::try_exists(&loc.path).await.unwrap_or(false) {
            return Ok(RunChanges::default());
        }
        let spec = format!("{sha}^{{commit}}");
        let resolved = self
            .git(&loc.path, &["rev-parse", "--verify", "--quiet", &spec])
            .await?;
        if !resolved.ok() {
            return Err(GitError::Unsafe(format!("{sha} is not a commit here")));
        }
        let full = resolved.stdout.trim().to_owned();
        let range = format!("{}..HEAD", loc.base_sha);
        let own = self.names(&loc.path, &["rev-list", "-z", &range]).await?;
        if !own.contains(&full) {
            return Err(GitError::Unsafe(format!(
                "{sha} is not a commit of this thread's branch"
            )));
        }
        let parent = format!("{full}^");
        let has_parent = self
            .git(&loc.path, &["rev-parse", "--verify", "--quiet", &parent])
            .await?
            .ok();
        let base = if has_parent {
            parent
        } else {
            EMPTY_TREE.to_owned()
        };
        let mut out = self.changes_against(loc, &base, Some(&full), path).await?;
        out.commits = 1;
        Ok(out)
    }

    /// Changes of the worktree of `loc` against `base`: with no `to`, committed, staged,
    /// unstaged and untracked work; with `to`, the changes of that commit alone.
    async fn changes_against(
        &self,
        loc: &Location,
        base: &str,
        to: Option<&str>,
        path: Option<&str>,
    ) -> Result<RunChanges, GitError> {
        if let Some(path) = path
            && !is_clean_relative(path)
        {
            return Err(GitError::Unsafe(format!(
                "{path:?} is not a path inside the worktree"
            )));
        }
        let mut out = RunChanges::default();
        if !tokio::fs::try_exists(&loc.path).await.unwrap_or(false) {
            return Ok(out);
        }
        let revisions: Vec<&str> = std::iter::once(base).chain(to).collect();
        if to.is_none() {
            out.commits = self
                .count(&loc.path, &format!("{base}..HEAD"))
                .await
                .unwrap_or(0);
        }
        let mut numstat_args = vec![
            "diff",
            "--numstat",
            "-z",
            "-M",
            "--no-ext-diff",
            "--no-textconv",
        ];
        numstat_args.extend(&revisions);
        numstat_args.push("--");
        let numstat = self.git(&loc.path, &numstat_args).await?;
        if !numstat.ok() {
            return Err(failed("diff --numstat", &numstat));
        }
        let mut status_args = vec![
            "diff",
            "--name-status",
            "-z",
            "-M",
            "--no-ext-diff",
            "--no-textconv",
        ];
        status_args.extend(&revisions);
        status_args.push("--");
        let status = self.git(&loc.path, &status_args).await?;
        if !status.ok() {
            return Err(failed("diff --name-status", &status));
        }
        let mut files = parse_changes(&status.stdout, &numstat.stdout);
        let tracked_paths: HashSet<String> = files.iter().map(|f| f.path.clone()).collect();

        // A commit has no untracked files.
        let untracked = if to.is_none() {
            self.names(
                &loc.path,
                &["ls-files", "-z", "--others", "--exclude-standard"],
            )
            .await?
        } else {
            Vec::new()
        };
        for (n, name) in untracked.into_iter().enumerate() {
            if tracked_paths.contains(&name) {
                continue;
            }
            let (additions, binary) = if n < UNTRACKED_COUNTED {
                count_untracked(&loc.path.join(&name)).await
            } else {
                (0, false)
            };
            files.push(ChangedFile {
                path: name,
                from: None,
                kind: ChangeKind::Added,
                additions,
                deletions: 0,
                binary,
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));

        if let Some(path) = path
            && let Some(entry) = files.iter().find(|f| f.path == path)
            && !entry.binary
        {
            let raw = if tracked_paths.contains(path) {
                let mut args = vec![
                    "diff",
                    "--no-color",
                    "--src-prefix=a/",
                    "--dst-prefix=b/",
                    "--no-ext-diff",
                    "--no-textconv",
                    "-M",
                ];
                args.extend(&revisions);
                args.push("--");
                let spec = format!(":(literal){path}");
                let from_spec = entry.from.as_ref().map(|f| format!(":(literal){f}"));
                args.push(&spec);
                if let Some(from) = &from_spec {
                    args.push(from);
                }
                let diff = self.git(&loc.path, &args).await?;
                if !diff.ok() {
                    return Err(failed("diff", &diff));
                }
                diff.stdout
            } else {
                // Untracked: a new-file patch from outside the index. Exit 1 means "differs".
                // A symlink would show its target path, not the file behind it.
                let diff = self
                    .git(
                        &loc.path,
                        &[
                            "diff",
                            "--no-index",
                            "--no-color",
                            "--src-prefix=a/",
                            "--dst-prefix=b/",
                            "--no-ext-diff",
                            "--no-textconv",
                            "--",
                            "/dev/null",
                            path,
                        ],
                    )
                    .await?;
                if !matches!(diff.code, Some(0 | 1)) {
                    return Err(failed("diff --no-index", &diff));
                }
                diff.stdout
            };
            if !raw.is_empty() {
                let (patch, cut) = cut_patch(raw);
                out.patch = Some(patch);
                out.patch_truncated = cut;
            }
        }
        out.files = files;
        Ok(out)
    }

    /// The NUL-separated names `git args` prints.
    async fn names(&self, dir: &Path, args: &[&str]) -> Result<Vec<String>, GitError> {
        let out = self.git(dir, args).await?;
        if !out.ok() {
            return Err(failed(&args.join(" "), &out));
        }
        Ok(out
            .stdout
            .split('\0')
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// Forgets worktrees of `repo` whose directory has vanished (`git worktree prune`).
    pub async fn prune(&self, repo: &Path) -> Result<(), GitError> {
        if !tokio::fs::try_exists(repo).await.unwrap_or(false) {
            return Ok(());
        }
        let _guard = self.lock(repo).await?;
        self.expect_ok(repo, &["worktree", "prune"], "worktree prune")
            .await
    }

    /// Directories under the worktrees root that `known` does not list: leftovers whose record
    /// is gone. Reported, never removed: they may hold work.
    pub async fn unknown_dirs(&self, known: &HashSet<PathBuf>) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(mut keys) = tokio::fs::read_dir(&self.root).await else {
            return found;
        };
        while let Ok(Some(key)) = keys.next_entry().await {
            let Ok(mut runs) = tokio::fs::read_dir(key.path()).await else {
                continue;
            };
            while let Ok(Some(run)) = runs.next_entry().await {
                if !known.contains(&run.path()) {
                    found.push(run.path());
                }
            }
        }
        found.sort();
        found
    }

    // ---- internals ------------------------------------------------------------------------

    async fn ensure_subdir(&self, loc: &Location) -> Result<(), GitError> {
        if loc.subdir.is_empty() {
            return Ok(());
        }
        // A directory that is not tracked at `HEAD` is not in the worktree: make it empty.
        tokio::fs::create_dir_all(loc.cwd())
            .await
            .map_err(|e| io("create the working subdirectory", &e))
    }

    async fn registered(&self, loc: &Location) -> Result<bool, GitError> {
        let list = self
            .git(&loc.repo, &["worktree", "list", "--porcelain"])
            .await?;
        if !list.ok() {
            return Err(failed("worktree list", &list));
        }
        let want = canonical(&loc.path).await?;
        Ok(list
            .stdout
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .any(|path| Path::new(path) == want || Path::new(path) == loc.path))
    }

    async fn count(&self, dir: &Path, range: &str) -> Result<u32, GitError> {
        let out = self.git(dir, &["rev-list", "--count", range]).await?;
        if !out.ok() {
            return Err(failed("rev-list", &out));
        }
        Ok(out.stdout.trim().parse().unwrap_or(0))
    }

    async fn is_ancestor(&self, repo: &Path, tip: &str) -> Result<bool, GitError> {
        let out = self
            .git(repo, &["merge-base", "--is-ancestor", tip, "HEAD"])
            .await?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(failed("merge-base", &out)),
        }
    }

    async fn lock(&self, repo: &Path) -> Result<tokio::sync::OwnedMutexGuard<()>, GitError> {
        let key = match self
            .git(
                repo,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await
        {
            Ok(out) if out.ok() => canonical(Path::new(out.stdout.trim())).await?,
            _ => repo.to_path_buf(),
        };
        let mutex = {
            let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(locks.entry(key).or_default())
        };
        Ok(mutex.lock_owned().await)
    }

    async fn expect_ok(&self, dir: &Path, args: &[&str], op: &str) -> Result<(), GitError> {
        let out = self.git(dir, args).await?;
        if out.ok() {
            Ok(())
        } else {
            Err(failed(op, &out))
        }
    }

    /// Runs `git args` in `dir`: own process group, closed stdin, bounded output, a timeout.
    async fn git(&self, dir: &Path, args: &[&str]) -> Result<Output, GitError> {
        let op = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
        let mut command = Command::new("git");
        command
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C");
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
        ] {
            command.env_remove(key);
        }
        let mut child =
            spawn_grouped(&mut command).map_err(|e| GitError::GitMissing(e.to_string()))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let finished = tokio::time::timeout(self.timeout, async {
            let (status, out, err) = tokio::join!(child.wait(), capture(stdout), capture(stderr));
            (status, out, err)
        })
        .await;
        match finished {
            Ok((status, out, err)) => {
                let status = status.map_err(|e| io("wait for git", &e))?;
                Ok(Output {
                    stdout: out,
                    stderr: err,
                    code: status.code(),
                })
            }
            Err(_) => {
                child.shutdown(KILL_GRACE).await;
                Err(GitError::Timeout {
                    op,
                    secs: self.timeout.as_secs(),
                })
            }
        }
    }
}

/// The longest patch `changes_since_base` returns.
pub const PATCH_CAP: usize = 256 * 1024;
/// Untracked files larger than this are listed without line counts.
const COUNT_LIMIT: u64 = 1024 * 1024;
/// At most this many untracked files are read to count their lines.
const UNTRACKED_COUNTED: usize = 2000;

/// The empty tree: what a root commit is compared with.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// What a run changed against its base commit: see [`GitWorktrees::changes_since_base`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunChanges {
    /// Commits beyond the base.
    pub commits: u32,
    /// The changed files, by path.
    pub files: Vec<ChangedFile>,
    /// The patch of the requested file.
    pub patch: Option<String>,
    /// Whether `patch` was cut at [`PATCH_CAP`].
    pub patch_truncated: bool,
}

/// A path git can be asked about: relative, `/`-separated, no empty, `.` or `..` part, no NUL.
fn is_clean_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\0')
        && !path.contains('\\')
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

/// Cuts `patch` to [`PATCH_CAP`] bytes at the last line end that fits.
fn cut_patch(patch: String) -> (String, bool) {
    if patch.len() <= PATCH_CAP {
        return (patch, false);
    }
    let mut end = PATCH_CAP;
    while !patch.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(line) = patch[..end].rfind('\n') {
        end = line + 1;
    }
    (patch[..end].to_owned(), true)
}

/// Lines in an untracked regular file and whether it looks binary. A file over [`COUNT_LIMIT`],
/// unreadable, or not a regular file (a symlink is never followed) counts as nothing.
async fn count_untracked(path: &Path) -> (u32, bool) {
    let Ok(meta) = tokio::fs::symlink_metadata(path).await else {
        return (0, false);
    };
    if !meta.is_file() || meta.len() > COUNT_LIMIT {
        return (0, false);
    }
    let Ok(bytes) = tokio::fs::read(path).await else {
        return (0, false);
    };
    if bytes.iter().take(8000).any(|b| *b == 0) {
        return (0, true);
    }
    let newlines = bytes.iter().filter(|b| **b == b'\n').count();
    let tail = usize::from(bytes.last().is_some_and(|b| *b != b'\n'));
    (u32::try_from(newlines + tail).unwrap_or(u32::MAX), false)
}

/// Joins `git diff --name-status -z -M` and `--numstat -z -M` (the same entries, keyed by their
/// new path) into files. A record cut short by the output bound is dropped.
fn parse_changes(name_status: &str, numstat: &str) -> Vec<ChangedFile> {
    // `adds \t dels \t path \0`, or for a rename `adds \t dels \t \0 old \0 new \0`.
    let mut counts: HashMap<String, (Option<(u32, u32)>, bool)> = HashMap::new();
    let mut parts = numstat.split('\0');
    while let Some(record) = parts.next() {
        let mut fields = record.splitn(3, '\t');
        let (Some(adds), Some(dels), Some(path)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            let (Some(_old), Some(new)) = (parts.next(), parts.next()) else {
                break;
            };
            new
        } else {
            path
        };
        let binary = adds == "-" && dels == "-";
        let n = (adds.parse().unwrap_or(0), dels.parse().unwrap_or(0));
        counts.insert(path.to_owned(), (Some(n), binary));
    }
    // `S \0 path \0`, or `R<score> \0 old \0 new \0`.
    let mut files = Vec::new();
    let mut parts = name_status.split('\0');
    while let Some(code) = parts.next() {
        if code.is_empty() {
            continue;
        }
        let kind = match code.chars().next() {
            Some('A') => ChangeKind::Added,
            Some('D') => ChangeKind::Deleted,
            Some('R' | 'C') => ChangeKind::Renamed,
            _ => ChangeKind::Modified,
        };
        let (from, path) = if kind == ChangeKind::Renamed {
            let (Some(old), Some(new)) = (parts.next(), parts.next()) else {
                break;
            };
            (Some(old.to_owned()), new.to_owned())
        } else {
            let Some(path) = parts.next() else { break };
            (None, path.to_owned())
        };
        let (n, binary) = counts.remove(&path).unwrap_or((None, false));
        let (additions, deletions) = if binary { (0, 0) } else { n.unwrap_or((0, 0)) };
        files.push(ChangedFile {
            path,
            from,
            kind,
            additions,
            deletions,
            binary,
        });
    }
    files
}

/// What [`GitWorktrees::probe`] learned about a directory inside a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The working tree root.
    pub toplevel: PathBuf,
    /// The canonical common git directory.
    pub common_dir: PathBuf,
    /// The directory's path inside the working tree, empty at the root.
    pub subdir: String,
}

/// The project of `dir` (project-scoped standing rules): the **canonical** path of the nearest
/// ancestor of `dir`, itself included, that has a `.git` entry, a directory or a file (a linked
/// worktree or a submodule has a file). Symlinks are resolved, so `/tmp/x` and `/private/tmp/x`
/// are one project. `None` for a directory that does not exist, is in no repository, or whose
/// root path is not UTF-8. Walks up the file system; runs no `git`.
pub async fn project_root(dir: &Path) -> Option<PathBuf> {
    let canonical = tokio::fs::canonicalize(dir).await.ok()?;
    for candidate in canonical.ancestors() {
        if tokio::fs::symlink_metadata(candidate.join(".git"))
            .await
            .is_ok()
        {
            candidate.to_str()?;
            return Some(candidate.to_path_buf());
        }
    }
    None
}

/// The branch of a run.
pub fn branch_of(run: &str) -> String {
    format!("reno/{run}")
}

fn check_run_id(run: &str) -> Result<(), GitError> {
    let safe = !run.is_empty()
        && run.len() <= 128
        && run
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if safe {
        Ok(())
    } else {
        Err(GitError::Unsafe(format!(
            "run id {run:?} is not a safe branch name"
        )))
    }
}

fn failed(op: &str, out: &Output) -> GitError {
    let tail: String = out.stderr.trim().chars().take(600).collect();
    GitError::Failed {
        op: op.to_owned(),
        code: out
            .code
            .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
        stderr: tail,
    }
}

fn io(what: &str, error: &std::io::Error) -> GitError {
    GitError::Io {
        what: what.to_owned(),
        message: error.to_string(),
    }
}

async fn canonical(path: &Path) -> Result<PathBuf, GitError> {
    tokio::fs::canonicalize(path)
        .await
        .map_err(|e| io(&format!("resolve {}", path.display()), &e))
}

/// Reads `reader` to its end, keeping the first [`OUTPUT_CAP`] bytes.
async fn capture<R: AsyncRead + Unpin>(reader: Option<R>) -> String {
    let Some(mut reader) = reader else {
        return String::new();
    };
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = OUTPUT_CAP.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}
