//! The path sandbox: every path a person's request names is resolved here, and nowhere else.
//!
//! A request path is relative to the served root and written with `/`. It is refused when it has
//! a `..`, is absolute, has a NUL, names `.git` (or anything inside one), or leads, through
//! symbolic links, outside the root or into `.git`. What is returned is the path's real location,
//! with every link resolved, so a caller opens exactly what was checked.
//!
//! The check and the open are two steps, so a link swapped in between could still escape. The
//! served trees are a run's own worktree, where only the run and the person write, and the
//! methods only read; a stricter scheme (`openat2`, `O_NOFOLLOW` per component) is not worth its
//! cost here. The resolved path is checked again after it is opened where the caller can.

use std::path::{Component, Path, PathBuf};

use super::error::{FsError, Rejection};

/// The longest path a request may name.
const MAX_PATH_BYTES: usize = 4096;
/// The one name no request may reach.
pub(crate) const GIT_DIR: &str = ".git";

/// A served directory: its real path, every link resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Root {
    real: PathBuf,
}

/// A path inside a [`Root`] that passed the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// Where it really is, every link resolved.
    pub(crate) real: PathBuf,
    /// The path as the wire writes it: relative to the root, `/`-separated, no `.` parts. Empty
    /// for the root.
    pub(crate) rel: String,
}

impl Root {
    /// The root at `dir`. A directory that is gone, or is not one, is unavailable.
    pub(crate) fn open(dir: &Path) -> Result<Self, FsError> {
        let real = std::fs::canonicalize(dir).map_err(|error| {
            FsError::Unavailable(format!("{} cannot be opened: {error}", dir.display()))
        })?;
        if !real.is_dir() {
            return Err(FsError::Unavailable(format!(
                "{} is not a directory",
                dir.display()
            )));
        }
        Ok(Self { real })
    }

    /// The root's real path.
    pub(crate) fn path(&self) -> &Path {
        &self.real
    }

    /// Checks `rel` and finds where it really is. [`FsError::NotFound`] when nothing is there.
    pub(crate) fn resolve(&self, rel: &str) -> Result<Resolved, FsError> {
        let normal = lexical(rel)?;
        let wanted = normal.iter().fold(self.real.clone(), |mut at, part| {
            at.push(part);
            at
        });
        let real = match std::fs::canonicalize(&wanted) {
            Ok(real) => real,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Nothing there; but if the way leads out through a link, say that instead, so
                // "not found" never reports on what lies outside the root.
                self.check_ancestor(&wanted, rel)?;
                return Err(FsError::NotFound(rel.to_owned()));
            }
            Err(error) => return Err(FsError::io(format!("resolving {rel:?}"), &error)),
        };
        self.check_inside(&real, rel)?;
        Ok(Resolved {
            real,
            rel: normal.join("/"),
        })
    }
}

impl Root {
    /// Refuses a real path outside the root or inside a `.git`.
    fn check_inside(&self, real: &Path, rel: &str) -> Result<(), FsError> {
        let Ok(inside) = real.strip_prefix(&self.real) else {
            return Err(rejected(Rejection::SymlinkEscape, rel));
        };
        if inside.components().any(is_git) {
            return Err(rejected(Rejection::GitInternals, rel));
        }
        Ok(())
    }

    /// For a path that does not exist: checks the nearest part of it that does.
    fn check_ancestor(&self, wanted: &Path, rel: &str) -> Result<(), FsError> {
        for ancestor in wanted.ancestors().skip(1) {
            if let Ok(real) = std::fs::canonicalize(ancestor) {
                return self.check_inside(&real, rel);
            }
        }
        Ok(())
    }
}

fn is_git(component: Component<'_>) -> bool {
    matches!(component, Component::Normal(name) if name == GIT_DIR)
}

fn rejected(why: Rejection, path: &str) -> FsError {
    FsError::Rejected {
        why,
        path: path.to_owned(),
    }
}

/// The path's own parts, after the checks that need no file system: no `..`, not absolute, no
/// NUL, no `.git`.
pub(crate) fn lexical(rel: &str) -> Result<Vec<String>, FsError> {
    if rel.len() > MAX_PATH_BYTES || rel.contains('\0') {
        return Err(rejected(Rejection::Malformed, rel));
    }
    // Both separators, so a path written on another platform cannot hide a part.
    if rel.starts_with(['/', '\\']) || has_drive(rel) {
        return Err(rejected(Rejection::Absolute, rel));
    }
    let mut parts = Vec::new();
    for part in rel.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return Err(rejected(Rejection::Traversal, rel)),
            GIT_DIR => return Err(rejected(Rejection::GitInternals, rel)),
            name => parts.push(name.to_owned()),
        }
    }
    Ok(parts)
}

/// `C:` or `C:/...`, a Windows drive.
fn has_drive(rel: &str) -> bool {
    let bytes = rel.as_bytes();
    bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes.get(2).is_none_or(|b| matches!(b, b'/' | b'\\'))
}

/// `rel` joined with `name`, as the wire writes paths.
pub(crate) fn join(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_owned()
    } else {
        format!("{rel}/{name}")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::os::unix::fs::symlink;

    use super::*;

    fn tree() -> (tempfile::TempDir, Root) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root_dir = base.join("root");
        std::fs::create_dir_all(root_dir.join("src")).unwrap();
        std::fs::create_dir_all(root_dir.join(".git/refs")).unwrap();
        std::fs::write(root_dir.join("src/a.rs"), "a").unwrap();
        std::fs::write(root_dir.join(".git/config"), "secret").unwrap();
        std::fs::write(base.join("outside.txt"), "outside").unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        // A link out of the root, one to a directory outside, one to .git, one that stays inside.
        symlink(base.join("outside.txt"), root_dir.join("out.txt")).unwrap();
        symlink(base.join("elsewhere"), root_dir.join("elsewhere")).unwrap();
        symlink(root_dir.join(".git/config"), root_dir.join("cfg")).unwrap();
        symlink("src/a.rs", root_dir.join("inside")).unwrap();
        let root = Root::open(&root_dir).unwrap();
        (dir, root)
    }

    fn why(result: Result<Resolved, FsError>) -> Rejection {
        match result.unwrap_err() {
            FsError::Rejected { why, .. } => why,
            other => panic!("expected a refusal, got {other}"),
        }
    }

    #[test]
    fn a_path_inside_resolves_and_is_normalised() {
        let (_dir, root) = tree();
        let found = root.resolve("./src//a.rs").unwrap();
        assert_eq!(found.rel, "src/a.rs");
        assert_eq!(found.real, root.path().join("src/a.rs"));
        assert_eq!(root.resolve("").unwrap().rel, "");
        assert_eq!(root.resolve(".").unwrap().real, root.path());
    }

    #[test]
    fn traversal_absolute_paths_and_nul_are_refused_before_the_file_system_is_asked() {
        let (_dir, root) = tree();
        assert_eq!(why(root.resolve("../outside.txt")), Rejection::Traversal);
        assert_eq!(why(root.resolve("src/../../x")), Rejection::Traversal);
        assert_eq!(why(root.resolve("src\\..\\..\\x")), Rejection::Traversal);
        assert_eq!(why(root.resolve("/etc/passwd")), Rejection::Absolute);
        assert_eq!(why(root.resolve("C:/x")), Rejection::Absolute);
        assert_eq!(why(root.resolve("a\0b")), Rejection::Malformed);
        // Refused even when nothing is there: the answer must not say what exists.
        assert_eq!(why(root.resolve("../nope")), Rejection::Traversal);
    }

    #[test]
    fn git_internals_are_refused_by_name_and_through_links() {
        let (_dir, root) = tree();
        assert_eq!(why(root.resolve(".git")), Rejection::GitInternals);
        assert_eq!(why(root.resolve(".git/config")), Rejection::GitInternals);
        assert_eq!(why(root.resolve("src/.git/x")), Rejection::GitInternals);
        assert_eq!(why(root.resolve("cfg")), Rejection::GitInternals);
        // The name only: a file that merely starts with it is an ordinary file.
        std::fs::write(root.path().join(".gitignore"), "x").unwrap();
        assert_eq!(root.resolve(".gitignore").unwrap().rel, ".gitignore");
    }

    #[test]
    fn a_link_that_leaves_the_root_is_refused_and_one_that_stays_is_followed() {
        let (_dir, root) = tree();
        assert_eq!(why(root.resolve("out.txt")), Rejection::SymlinkEscape);
        assert_eq!(why(root.resolve("elsewhere")), Rejection::SymlinkEscape);
        assert_eq!(why(root.resolve("elsewhere/x")), Rejection::SymlinkEscape);
        let inside = root.resolve("inside").unwrap();
        assert_eq!(inside.real, root.path().join("src/a.rs"));
    }

    #[test]
    fn a_missing_path_is_not_found_and_a_missing_root_is_unavailable() {
        let (dir, root) = tree();
        assert!(matches!(
            root.resolve("src/none.rs"),
            Err(FsError::NotFound(_))
        ));
        assert!(matches!(
            Root::open(&dir.path().join("gone")),
            Err(FsError::Unavailable(_))
        ));
        assert!(matches!(
            Root::open(&root.path().join("src/a.rs")),
            Err(FsError::Unavailable(_))
        ));
    }
}
