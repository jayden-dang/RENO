//! Finding files by name: the list of candidate files, and the fuzzy scorer.
//!
//! The candidates are what git says is tracked or not ignored, so a gitignored tree such as
//! `target/` or `node_modules/` is never walked, and a tracked file is found even when a pattern
//! would ignore it. A folder that is no repository is walked, without `.git`, up to a cap.

use std::path::Path;

use reno_proto::FsFileMatch;

use super::sandbox::GIT_DIR;

/// The most files a folder that is no repository contributes.
const WALK_CAP: usize = 100_000;

/// A fuzzy match of `query` against `path`, or `None` when its characters do not appear in order.
/// Case-insensitive. Higher is better: a match in the file's name beats one in its directories,
/// consecutive characters and word starts count, and a shorter path wins a tie.
pub(crate) fn score(query: &str, path: &str) -> Option<u32> {
    let wanted: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if wanted.is_empty() {
        return None;
    }
    let chars: Vec<char> = path.chars().collect();
    let lower: Vec<char> = chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let name_start = chars.iter().rposition(|c| *c == '/').map_or(0, |at| at + 1);

    let mut at = 0;
    let mut total: i64 = 0;
    let mut previous: Option<usize> = None;
    for want in &wanted {
        let found = lower.get(at..)?.iter().position(|c| c == want)? + at;
        total += 10;
        if previous.is_some_and(|p| p + 1 == found) {
            total += 15;
        }
        let before = found.checked_sub(1).and_then(|i| chars.get(i));
        let starts_word = before.is_none_or(|b| matches!(b, '/' | '_' | '-' | '.' | ' '))
            || (chars[found].is_uppercase() && before.is_some_and(|b| b.is_lowercase()));
        if starts_word {
            total += 10;
        }
        if found >= name_start {
            total += 5;
        }
        previous = Some(found);
        at = found + 1;
    }
    let name: String = lower[name_start..].iter().collect();
    let needle: String = wanted.iter().collect();
    if name == needle {
        total += 400;
    } else if name.starts_with(&needle) {
        total += 250;
    } else if name.contains(&needle) {
        total += 150;
    }
    // A longer path is a slightly worse match.
    total -= i64::try_from(chars.len() / 8).unwrap_or(0);
    u32::try_from(total.max(1)).ok()
}

/// The best `limit` of `files` for `query`, best first, and whether more matched.
pub(crate) fn best(query: &str, files: &[String], limit: usize) -> (Vec<FsFileMatch>, bool) {
    let mut found: Vec<FsFileMatch> = files
        .iter()
        .filter_map(|path| {
            score(query, path).map(|score| FsFileMatch {
                path: path.clone(),
                score,
            })
        })
        .collect();
    found.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.path.len().cmp(&b.path.len()))
            .then_with(|| a.path.cmp(&b.path))
    });
    let truncated = found.len() > limit;
    found.truncate(limit);
    (found, truncated)
}

/// Every file under `dir`, relative and `/`-separated, without `.git`, links not followed, at most
/// [`WALK_CAP`].
pub(crate) fn walk(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut pending = vec![(dir.to_path_buf(), String::new())];
    while let Some((at, rel)) = pending.pop() {
        let Ok(reader) = std::fs::read_dir(&at) else {
            continue;
        };
        for item in reader.flatten() {
            let Ok(name) = item.file_name().into_string() else {
                continue;
            };
            if name == GIT_DIR {
                continue;
            }
            let Ok(kind) = item.file_type() else { continue };
            let path = super::sandbox::join(&rel, &name);
            if kind.is_dir() {
                pending.push((item.path(), path));
            } else if files.len() < WALK_CAP {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn order(query: &str, files: &[&str]) -> Vec<String> {
        let files: Vec<String> = files.iter().map(|f| (*f).to_owned()).collect();
        best(query, &files, 10)
            .0
            .into_iter()
            .map(|m| m.path)
            .collect()
    }

    #[test]
    fn characters_must_appear_in_order_and_case_is_ignored() {
        assert!(score("lrs", "src/lib.rs").is_some());
        assert!(score("LIB", "src/lib.rs").is_some());
        assert!(score("sl", "src/lib.rs").is_some());
        assert!(score("bl", "src/lib.rs").is_none());
        assert!(score("zzz", "src/lib.rs").is_none());
        assert!(score("", "src/lib.rs").is_none());
    }

    #[test]
    fn a_hit_in_the_name_beats_a_hit_in_the_directories() {
        let got = order(
            "main",
            &["main/other.rs", "src/domain_api.rs", "apps/x/src/main.rs"],
        );
        assert_eq!(got[0], "apps/x/src/main.rs");
        // The exact name beats a prefix, which beats a substring.
        let got = order("lib", &["src/mylib.rs", "src/library.rs", "src/lib.rs"]);
        assert_eq!(got, ["src/lib.rs", "src/library.rs", "src/mylib.rs"]);
    }

    #[test]
    fn ties_go_to_the_shorter_path_and_the_result_is_cut_with_a_flag() {
        let files: Vec<String> = (0..5).map(|n| format!("a{n}.rs")).collect();
        let (cut, truncated) = best("a", &files, 3);
        assert_eq!(cut.len(), 3);
        assert!(truncated);
        let (all, truncated) = best("a", &files, 5);
        assert_eq!((all.len(), truncated), (5, false));
        assert_eq!(order("x", &["bb/x.rs", "x.rs"])[0], "x.rs");
    }

    #[test]
    fn a_walk_skips_git_and_lists_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join(".git/config"), "x").unwrap();
        std::fs::write(root.join("a/b/c.txt"), "x").unwrap();
        std::fs::write(root.join("top.txt"), "x").unwrap();
        assert_eq!(walk(root), ["a/b/c.txt", "top.txt"]);
    }
}
