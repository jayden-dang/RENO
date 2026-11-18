//! The D7 sandbox: a macOS seatbelt profile that keeps a person's shell from writing what the
//! protected-path guard watches.
//!
//! The guard (`reno-kernel/src/supervisor/protect.rs`) assumes that only the run writes in its
//! worktree, and judges the worktree's changes after every tool call. A person's shell in the
//! same tree would get the run blamed, reverted and stopped. So a shell opened while the run is
//! live cannot write the paths the guard looks at: not by `echo >`, not by `mv` into them, not by
//! a child process, not by `git commit` on the run's branch (it needs the worktree's private git
//! directory). The guard stays as it is and still catches the agent.
//!
//! # One list
//!
//! The names come from [`reno_proto::defs::PROTECTED_DIRS`] and
//! [`PROTECTED_FILE`](reno_proto::defs::PROTECTED_FILE), which the guard reads too, so the two
//! cannot drift. The guard matches them at any depth and ASCII case-insensitively, so the profile
//! does the same with `regex` filters on the canonical worktree root. `.git` is the exception: a
//! worktree's `.git` is one file at its root (the link to the private git directory), which is
//! denied with the private directory itself.
//!
//! # Real paths
//!
//! The profile must name real paths: `/var/...` silently does not match until it is written as
//! `/private/var/...`. [`Limits::for_worktree`] canonicalizes the root and the private git
//! directory. The protected directories usually do not exist yet, so they are never
//! canonicalized: the root is, and the names are matched below it.

use std::io;
use std::path::{Path, PathBuf};

use reno_proto::defs::{PROTECTED_DIRS, PROTECTED_FILE};

/// What a shell may not write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// The worktree's real path.
    pub root: PathBuf,
    /// The worktree's private git directory (`<repo>/.git/worktrees/<name>`), real path. `None`
    /// when `root/.git` is itself a directory.
    pub private_git_dir: Option<PathBuf>,
}

/// Why limits could not be made.
#[derive(Debug, thiserror::Error)]
pub enum LimitsError {
    /// The worktree (or its git directory) cannot be resolved.
    #[error("cannot resolve {path}: {source}")]
    Resolve {
        /// The path.
        path: String,
        /// Why.
        source: io::Error,
    },
    /// A path the profile cannot spell safely (a quote or a line break in it).
    #[error("the path {0:?} cannot be put in a sandbox profile")]
    Unspellable(String),
}

impl Limits {
    /// The limits of the worktree at `dir`, resolved to real paths.
    pub fn for_worktree(dir: &Path) -> Result<Self, LimitsError> {
        let root = std::fs::canonicalize(dir).map_err(|source| LimitsError::Resolve {
            path: dir.display().to_string(),
            source,
        })?;
        let private_git_dir = private_git_dir(&root)?;
        let limits = Self {
            root,
            private_git_dir,
        };
        // Refuse here what the profile cannot say, so the profile never has a hole.
        limits.check_spellable()?;
        Ok(limits)
    }

    fn check_spellable(&self) -> Result<(), LimitsError> {
        for path in std::iter::once(&self.root).chain(self.private_git_dir.as_ref()) {
            let text = path
                .to_str()
                .ok_or_else(|| LimitsError::Unspellable(path.display().to_string()))?;
            if text.contains(['"', '\n', '\r', '\0']) {
                return Err(LimitsError::Unspellable(text.to_owned()));
            }
        }
        Ok(())
    }

    /// The seatbelt profile: everything allowed except writes to the protected paths.
    pub fn profile(&self) -> String {
        let root = self.root.to_string_lossy();
        let mut deny = String::new();
        // The worktree's own `.git` (a link file), and its private git directory: HEAD, index and
        // the logs of the run's branch.
        deny.push_str(&format!(
            "  (subpath \"{}\")\n",
            string_escape(&format!("{root}/.git"))
        ));
        if let Some(dir) = &self.private_git_dir {
            deny.push_str(&format!(
                "  (subpath \"{}\")\n",
                string_escape(&dir.to_string_lossy())
            ));
        }
        let prefix = regex_escape(&root);
        let dirs: Vec<String> = PROTECTED_DIRS
            .iter()
            .filter(|name| **name != ".git")
            .map(|name| literal_nocase(name))
            .collect();
        if !dirs.is_empty() {
            deny.push_str(&format!(
                "  (regex #\"^{prefix}/(.*/)?({})(/|$)\")\n",
                dirs.join("|")
            ));
        }
        deny.push_str(&format!(
            "  (regex #\"^{prefix}/(.*/)?{}$\")\n",
            literal_nocase(PROTECTED_FILE)
        ));
        format!("(version 1)\n(allow default)\n(deny file-write*\n{deny})\n")
    }
}

/// The worktree's private git directory, read from its `.git` link file (`gitdir: <path>`), as
/// git itself reads it. `None` when `.git` is a directory or absent.
fn private_git_dir(root: &Path) -> Result<Option<PathBuf>, LimitsError> {
    let link = root.join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&link) else {
        return Ok(None);
    };
    if !meta.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&link).map_err(|source| LimitsError::Resolve {
        path: link.display().to_string(),
        source,
    })?;
    let Some(target) = text.lines().find_map(|l| l.strip_prefix("gitdir:")) else {
        return Ok(None);
    };
    let target = Path::new(target.trim());
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        root.join(target)
    };
    std::fs::canonicalize(&target)
        .map(Some)
        .map_err(|source| LimitsError::Resolve {
            path: target.display().to_string(),
            source,
        })
}

/// `text` inside a seatbelt string literal.
fn string_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
}

/// `text` as a regular expression that matches it literally.
fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if ".^$*+?()[]{}|\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A regular expression matching `name` literally, each ASCII letter in either case (macOS
/// compares names so, and so does the guard).
fn literal_nocase(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphabetic() {
            out.push('[');
            out.push(c.to_ascii_lowercase());
            out.push(c.to_ascii_uppercase());
            out.push(']');
        } else if ".^$*+?()[]{}|\\".contains(c) {
            out.push('[');
            out.push(c);
            out.push(']');
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_profile_names_every_protected_name_from_the_shared_list() {
        let limits = Limits {
            root: PathBuf::from("/real/tree"),
            private_git_dir: Some(PathBuf::from("/real/repo/.git/worktrees/w1")),
        };
        let profile = limits.profile();
        assert!(profile.starts_with("(version 1)\n(allow default)\n(deny file-write*"));
        assert!(
            profile.contains("(subpath \"/real/tree/.git\")"),
            "{profile}"
        );
        assert!(
            profile.contains("(subpath \"/real/repo/.git/worktrees/w1\")"),
            "{profile}"
        );
        for name in PROTECTED_DIRS.iter().filter(|name| **name != ".git") {
            assert!(profile.contains(&literal_nocase(name)), "{name}: {profile}");
        }
        assert!(
            profile.contains(&literal_nocase(PROTECTED_FILE)),
            "{profile}"
        );
    }

    #[test]
    fn regex_metacharacters_in_a_path_are_escaped() {
        assert_eq!(regex_escape("/a.b/c+d"), "/a\\.b/c\\+d");
        assert_eq!(
            literal_nocase(".mcp.json"),
            "[.][mM][cC][pP][.][jJ][sS][oO][nN]"
        );
    }

    #[test]
    fn a_path_with_a_quote_is_refused_instead_of_spelled_wrongly() {
        let dir = tempfile::tempdir().unwrap();
        let odd = dir.path().join("a\"b");
        std::fs::create_dir(&odd).unwrap();
        assert!(matches!(
            Limits::for_worktree(&odd),
            Err(LimitsError::Unspellable(_))
        ));
    }

    #[test]
    fn limits_use_real_paths_and_the_private_git_directory_of_a_linked_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        let repo = real.join("repo");
        let tree = real.join("tree");
        std::fs::create_dir_all(repo.join(".git/worktrees/w1")).unwrap();
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(
            tree.join(".git"),
            format!("gitdir: {}\n", repo.join(".git/worktrees/w1").display()),
        )
        .unwrap();
        // Asked through a symlink, the limits are of the real directory.
        let link = real.join("link");
        std::os::unix::fs::symlink(&tree, &link).unwrap();
        let limits = Limits::for_worktree(&link).unwrap();
        assert_eq!(limits.root, tree);
        assert_eq!(limits.private_git_dir, Some(repo.join(".git/worktrees/w1")));
        // A directory `.git` has no private directory of its own.
        let plain = real.join("plain");
        std::fs::create_dir_all(plain.join(".git")).unwrap();
        assert_eq!(Limits::for_worktree(&plain).unwrap().private_git_dir, None);
    }
}
