//! What a run changed, and what its tools printed: the facts the Run screen's Changes view and
//! its tool-output blocks draw. Pure functions from the wire types, so every state (older
//! daemon, loading, failed, removed, nothing changed, files) and the diff parser are unit tests.

use reno_proto::{
    ChangeKind, ChangedFile, ToolOutput, WorkspaceDiffResult, WorkspaceInfo, WorkspaceStatus,
};

/// How many lines of a tool's output show before "Show N more lines".
pub(crate) const OUTPUT_LINES: usize = 12;

/// An answer the app asked for and may not have yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Loaded<T> {
    Loading,
    Ready(T),
    Failed(String),
}

/// What Review compares: everything the thread did since its base, only the latest run, or what
/// is not committed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum ReviewScope {
    #[default]
    Thread,
    Run,
    /// The worktree against `HEAD`: staged, unstaged and untracked files, no commits.
    WorkingTree,
}

impl ReviewScope {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Thread => "This thread",
            Self::Run => "This run",
            Self::WorkingTree => "Uncommitted",
        }
    }

    /// What `workspace.diff` is asked for; `None` is the method's own default, which every daemon
    /// with `run.changes` knows.
    pub(crate) fn diff_scope(self) -> Option<reno_proto::DiffScope> {
        match self {
            Self::Thread => None,
            Self::Run => Some(reno_proto::DiffScope::Run),
            Self::WorkingTree => Some(reno_proto::DiffScope::WorkingTree),
        }
    }

    /// The feature a daemon needs for this scope, `None` when `run.changes` is enough.
    pub(crate) const fn needs(self) -> Option<&'static str> {
        match self {
            Self::Thread => None,
            Self::Run => Some(reno_proto::FEATURE_THREADS),
            Self::WorkingTree => Some(reno_proto::FEATURE_HISTORY),
        }
    }

    /// Why a daemon without [`Self::needs`] cannot offer it, for a tooltip.
    pub(crate) const fn needs_words(self) -> &'static str {
        match self {
            Self::Thread => "",
            Self::Run => "This daemon has no threads. Update renod to compare one run.",
            Self::WorkingTree => {
                "This daemon is too old to show uncommitted changes. Update renod."
            }
        }
    }
}

// --- tool output --------------------------------------------------------------------------------

/// The first lines of an output and how many are left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Folded<'a> {
    pub(crate) shown: &'a str,
    /// Lines not in `shown`.
    pub(crate) hidden: usize,
}

/// The first `limit` lines of `text` (its final newline does not start a line) and the count
/// of the rest.
pub(crate) fn fold(text: &str, limit: usize) -> Folded<'_> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let mut ends = text.match_indices('\n').map(|(i, _)| i);
    let Some(cut) = ends.nth(limit.saturating_sub(1)) else {
        return Folded {
            shown: text,
            hidden: 0,
        };
    };
    Folded {
        shown: &text[..cut],
        hidden: 1 + ends.count(),
    }
}

/// "Show 8 more lines" / "Show 1 more line".
pub(crate) fn more_label(hidden: usize) -> String {
    format!(
        "Show {hidden} more line{}",
        if hidden == 1 { "" } else { "s" }
    )
}

/// "482 KB", "1.5 MB", "900 B": the size of an output, in 1024s.
pub(crate) fn size_label(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.1} GB", b / (KB * KB * KB))
    }
}

/// What to say under an output the daemon cut: "Output was cut: 482 KB total".
pub(crate) fn cut_note(output: &ToolOutput) -> Option<String> {
    output
        .truncated
        .then(|| format!("Output was cut: {} total", size_label(output.bytes)))
}

// --- the list of changed files -------------------------------------------------------------------

/// The tone of a change kind's glyph; the screen maps it to a colour of the theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KindTone {
    Success,
    Warning,
    Danger,
    Accent,
}

/// One line of the file list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileRow {
    pub(crate) path: String,
    pub(crate) from: Option<String>,
    pub(crate) kind: ChangeKind,
    pub(crate) letter: char,
    pub(crate) tone: KindTone,
    /// The folder with its trailing slash; empty at the top.
    pub(crate) dir: String,
    pub(crate) name: String,
    pub(crate) additions: u32,
    pub(crate) deletions: u32,
    pub(crate) binary: bool,
}

pub(crate) fn kind_letter(kind: ChangeKind) -> (char, KindTone) {
    match kind {
        ChangeKind::Added => ('A', KindTone::Success),
        ChangeKind::Modified => ('M', KindTone::Warning),
        ChangeKind::Deleted => ('D', KindTone::Danger),
        ChangeKind::Renamed => ('R', KindTone::Accent),
    }
}

/// "crates/a/b.rs" is ("crates/a/", "b.rs").
pub(crate) fn split_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').map_or(("", path), |(_, name)| {
        (&path[..path.len() - name.len()], name)
    })
}

/// The files as rows, folder by folder, each folder's files by name.
pub(crate) fn file_rows(files: &[ChangedFile]) -> Vec<FileRow> {
    let mut rows: Vec<FileRow> = files
        .iter()
        .map(|f| {
            let (dir, name) = split_path(&f.path);
            let (letter, tone) = kind_letter(f.kind);
            FileRow {
                path: f.path.clone(),
                from: f.from.clone(),
                kind: f.kind,
                letter,
                tone,
                dir: dir.to_owned(),
                name: name.to_owned(),
                additions: f.additions,
                deletions: f.deletions,
                binary: f.binary,
            }
        })
        .collect();
    rows.sort_by(|a, b| (&a.dir, &a.name).cmp(&(&b.dir, &b.name)));
    rows
}

/// "+12", "−3" in the list's own words; binary files have none.
pub(crate) fn stat_labels(row: &FileRow) -> Option<(String, String)> {
    (!row.binary).then(|| {
        (
            format!("+{}", row.additions),
            format!("\u{2212}{}", row.deletions),
        )
    })
}

/// Files, additions and deletions over the whole list.
pub(crate) fn totals(files: &[ChangedFile]) -> (usize, u32, u32) {
    files.iter().fold((0, 0, 0), |(n, a, d), f| {
        (n + 1, a + f.additions, d + f.deletions)
    })
}

fn plural(n: impl Into<u64>, one: &str, many: &str) -> String {
    let n = n.into();
    format!("{n} {}", if n == 1 { one } else { many })
}

/// "6 files changed (+15 −8)".
pub(crate) fn stat_line(files: &[ChangedFile]) -> String {
    let (n, a, d) = totals(files);
    format!(
        "{} changed (+{a} \u{2212}{d})",
        plural(n as u64, "file", "files")
    )
}

/// The result line near the Done row: "Branch reno/x has 3 commits, 6 files changed (+15 −8)".
/// `None` when there is nothing to say (no commits and no changed files).
pub(crate) fn result_line(diff: &WorkspaceDiffResult) -> Option<String> {
    if diff.commits == 0 && diff.files.is_empty() {
        return None;
    }
    let stats = stat_line(&diff.files);
    Some(if diff.commits == 0 {
        format!("Branch {} has {stats}, not committed", diff.branch)
    } else if diff.files.is_empty() {
        format!(
            "Branch {} has {}",
            diff.branch,
            plural(diff.commits, "commit", "commits")
        )
    } else {
        format!(
            "Branch {} has {}, {stats}",
            diff.branch,
            plural(diff.commits, "commit", "commits")
        )
    })
}

// --- the Changes pane ----------------------------------------------------------------------------

/// What the Changes view and the details section show for a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Pane {
    /// The run has no workspace (a read-only run), or none is known yet: show nothing.
    Hidden,
    /// The daemon predates `workspace.diff`.
    Unsupported,
    Loading,
    Failed(String),
    /// The workspace was removed: nothing is left to compare.
    Removed {
        branch: String,
    },
    /// A workspace in which nothing changed.
    Empty {
        branch: String,
    },
    Files(Overview),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Overview {
    pub(crate) branch: String,
    /// "3 commits", "no commits yet".
    pub(crate) commits: String,
    /// "6 files changed (+15 −8)".
    pub(crate) stats: String,
    pub(crate) rows: Vec<FileRow>,
    /// Where the worktree is, for "Copy path".
    pub(crate) path: Option<String>,
}

/// The pane for a run from what the daemon said: `supported` is the `run.changes` feature,
/// `workspace` is `workspace.get` (`Ready(None)`: the run never had one) and `diff` is
/// `workspace.diff`.
pub(crate) fn pane(
    supported: bool,
    workspace: &Loaded<Option<WorkspaceInfo>>,
    diff: &Loaded<WorkspaceDiffResult>,
) -> Pane {
    if !supported {
        return Pane::Unsupported;
    }
    let info = match workspace {
        Loaded::Ready(None) => return Pane::Hidden,
        Loaded::Ready(Some(info)) => Some(info),
        Loaded::Loading | Loaded::Failed(_) => None,
    };
    if let Some(info) = info.filter(|i| i.status == WorkspaceStatus::Removed) {
        return Pane::Removed {
            branch: info.branch.clone(),
        };
    }
    match diff {
        Loaded::Loading => Pane::Loading,
        Loaded::Failed(why) => Pane::Failed(why.clone()),
        Loaded::Ready(d) if d.files.is_empty() => Pane::Empty {
            branch: d.branch.clone(),
        },
        Loaded::Ready(d) => Pane::Files(Overview {
            branch: d.branch.clone(),
            commits: match d.commits {
                0 => "no commits yet".to_owned(),
                n => plural(n, "commit", "commits"),
            },
            stats: stat_line(&d.files),
            rows: file_rows(&d.files),
            path: info.map(|i| i.cwd().display().to_string()),
        }),
    }
}

// --- one file's patch ----------------------------------------------------------------------------

/// What the diff area shows for the selected file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PatchPane {
    Loading,
    Failed(String),
    /// Git treats the file as binary: no patch.
    Binary,
    /// The file has no patch (for instance it changed back since the list was read).
    Nothing,
    Rows {
        rows: Vec<DiffRow>,
        truncated: bool,
    },
}

/// What `workspace.diff` said about one file: the patch and whether it was cut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Patch {
    pub(crate) text: Option<String>,
    pub(crate) truncated: bool,
    /// The digest of the diff this patch came with: it names the patch a file's syntax colours
    /// were made for. Empty from a daemon without feature `history`.
    pub(crate) checksum: String,
}

pub(crate) fn patch_pane(file: &FileRow, patch: &Loaded<Patch>) -> PatchPane {
    if file.binary {
        return PatchPane::Binary;
    }
    match patch {
        Loaded::Loading => PatchPane::Loading,
        Loaded::Failed(why) => PatchPane::Failed(why.clone()),
        Loaded::Ready(Patch { text: None, .. }) => PatchPane::Nothing,
        Loaded::Ready(Patch {
            text: Some(text),
            truncated,
            ..
        }) => {
            let rows = parse_patch(text);
            if rows.iter().any(|r| r.kind == RowKind::Binary) {
                PatchPane::Binary
            } else if rows.is_empty() {
                PatchPane::Nothing
            } else {
                PatchPane::Rows {
                    rows,
                    truncated: *truncated,
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowKind {
    /// `@@ -a,b +c,d @@ context`.
    Hunk,
    Context,
    Add,
    Del,
    /// `\ No newline at end of file`.
    NoNewline,
    /// `Binary files a and b differ`.
    Binary,
}

/// One line of a unified diff with the line numbers it has on each side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiffRow {
    pub(crate) kind: RowKind,
    pub(crate) old: Option<u32>,
    pub(crate) new: Option<u32>,
    /// The line without its `+`, `-` or space; for a hunk, the whole header.
    pub(crate) text: String,
}

/// `@@ -10,5 +12,8 @@` is `(10, 5, 12, 8)`; a missing count is 1.
fn hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |r: &str| -> Option<(u32, u32)> {
        match r.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let (os, oc) = range(old)?;
    let (ns, nc) = range(new)?;
    Some((os, oc, ns, nc))
}

/// A unified diff of one file as rows. The file header (`diff --git`, `index`, `---`, `+++`,
/// rename and mode lines) is skipped: the list says what kind of change it was. Line counts
/// from each hunk header tell a body line from a header, so a deleted line that reads `-- x` is
/// not mistaken for `--- a/file`.
pub(crate) fn parse_patch(patch: &str) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    let (mut old, mut new) = (0u32, 0u32);
    let (mut left_old, mut left_new) = (0u32, 0u32);
    for line in patch.lines() {
        let in_hunk = left_old > 0 || left_new > 0;
        if in_hunk {
            let (kind, o, n, text) = match line.chars().next() {
                Some('+') => (RowKind::Add, None, Some(new), &line[1..]),
                Some('-') => (RowKind::Del, Some(old), None, &line[1..]),
                Some(' ') => (RowKind::Context, Some(old), Some(new), &line[1..]),
                // A context line whose trailing space was stripped.
                None => (RowKind::Context, Some(old), Some(new), ""),
                Some('\\') => {
                    rows.push(note(line));
                    continue;
                }
                // Not a body line: the hunk was cut short; read it as a header.
                Some(_) => {
                    left_old = 0;
                    left_new = 0;
                    continue;
                }
            };
            if o.is_some() {
                old += 1;
                left_old = left_old.saturating_sub(1);
            }
            if n.is_some() {
                new += 1;
                left_new = left_new.saturating_sub(1);
            }
            rows.push(DiffRow {
                kind,
                old: o,
                new: n,
                text: text.to_owned(),
            });
        } else if let Some((os, oc, ns, nc)) = hunk_header(line) {
            (old, new, left_old, left_new) = (os, ns, oc, nc);
            rows.push(DiffRow {
                kind: RowKind::Hunk,
                old: None,
                new: None,
                text: line.to_owned(),
            });
        } else if line.starts_with('\\') {
            rows.push(note(line));
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            rows.push(DiffRow {
                kind: RowKind::Binary,
                old: None,
                new: None,
                text: line.to_owned(),
            });
        }
    }
    rows
}

fn note(line: &str) -> DiffRow {
    DiffRow {
        kind: RowKind::NoNewline,
        old: None,
        new: None,
        text: line.trim_start_matches('\\').trim().to_owned(),
    }
}

/// How many digits the widest line number of `rows` has, for the gutters.
pub(crate) fn gutter_digits(rows: &[DiffRow]) -> usize {
    rows.iter()
        .flat_map(|r| [r.old, r.new])
        .flatten()
        .max()
        .map_or(1, |n| n.to_string().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, kind: ChangeKind, a: u32, d: u32, binary: bool) -> ChangedFile {
        ChangedFile {
            path: path.to_owned(),
            from: None,
            kind,
            additions: a,
            deletions: d,
            binary,
        }
    }

    fn diff(commits: u32, files: Vec<ChangedFile>) -> WorkspaceDiffResult {
        WorkspaceDiffResult {
            run_id: reno_proto::RunId::from_suffix("x").unwrap(),
            branch: "reno/x".to_owned(),
            base_sha: "abc".to_owned(),
            commits,
            files,
            patch: None,
            patch_truncated: false,
            checksum: String::new(),
        }
    }

    fn info(status: WorkspaceStatus) -> WorkspaceInfo {
        WorkspaceInfo {
            run_id: reno_proto::RunId::from_suffix("x").unwrap(),
            repo: "/r".into(),
            repo_key: "r".to_owned(),
            path: "/w/x".into(),
            subdir: String::new(),
            branch: "reno/x".to_owned(),
            base_sha: "abc".to_owned(),
            status,
            changes: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn output_folds_to_its_first_lines_and_counts_the_rest() {
        let cases: [(&str, usize, &str, usize); 7] = [
            ("", 3, "", 0),
            ("a", 3, "a", 0),
            ("a\nb\nc\n", 3, "a\nb\nc", 0),
            ("a\nb\nc\nd\n", 3, "a\nb\nc", 1),
            ("a\nb\nc\nd\ne", 3, "a\nb\nc", 2),
            ("a\n\nb\n\n\n", 2, "a\n", 3),
            ("one line only", 12, "one line only", 0),
        ];
        for (text, limit, shown, hidden) in cases {
            assert_eq!(fold(text, limit), Folded { shown, hidden }, "{text:?}");
        }
    }

    #[test]
    fn labels_for_hidden_lines_and_sizes() {
        assert_eq!(more_label(1), "Show 1 more line");
        assert_eq!(more_label(8), "Show 8 more lines");
        assert_eq!(size_label(900), "900 B");
        assert_eq!(size_label(493_568), "482 KB");
        assert_eq!(size_label(1_572_864), "1.5 MB");
    }

    #[test]
    fn a_cut_output_says_how_big_it_was() {
        let mut out = ToolOutput {
            text: "x".to_owned(),
            bytes: 493_568,
            truncated: false,
        };
        assert_eq!(cut_note(&out), None);
        out.truncated = true;
        assert_eq!(
            cut_note(&out).as_deref(),
            Some("Output was cut: 482 KB total")
        );
    }

    #[test]
    fn files_are_grouped_by_folder_and_named_by_kind() {
        let rows = file_rows(&[
            file("src/z.rs", ChangeKind::Added, 5, 0, false),
            file("Cargo.toml", ChangeKind::Modified, 1, 1, false),
            file("src/a.rs", ChangeKind::Deleted, 0, 4, false),
            file("assets/logo.png", ChangeKind::Renamed, 0, 0, true),
        ]);
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r.dir.as_str(), r.name.as_str(), r.letter, r.tone))
            .collect();
        assert_eq!(
            got,
            [
                ("", "Cargo.toml", 'M', KindTone::Warning),
                ("assets/", "logo.png", 'R', KindTone::Accent),
                ("src/", "a.rs", 'D', KindTone::Danger),
                ("src/", "z.rs", 'A', KindTone::Success),
            ]
        );
        assert_eq!(
            stat_labels(&rows[2]),
            Some(("+0".into(), "\u{2212}4".into()))
        );
        assert_eq!(stat_labels(&rows[1]), None, "a binary file has no counts");
    }

    #[test]
    fn the_result_line_reads_in_words() {
        let files = vec![
            file("a", ChangeKind::Added, 100, 0, false),
            file("b", ChangeKind::Modified, 20, 30, false),
        ];
        let cases = [
            (
                diff(3, files.clone()),
                Some("Branch reno/x has 3 commits, 2 files changed (+120 \u{2212}30)"),
            ),
            (
                diff(1, files[..1].to_vec()),
                Some("Branch reno/x has 1 commit, 1 file changed (+100 \u{2212}0)"),
            ),
            (
                diff(0, files),
                Some("Branch reno/x has 2 files changed (+120 \u{2212}30), not committed"),
            ),
            (diff(2, vec![]), Some("Branch reno/x has 2 commits")),
            (diff(0, vec![]), None),
        ];
        for (d, line) in cases {
            assert_eq!(result_line(&d).as_deref(), line);
        }
    }

    #[test]
    fn the_pane_has_plain_states() {
        let ready = Loaded::Ready(diff(
            2,
            vec![file("a/b.rs", ChangeKind::Added, 1, 0, false)],
        ));
        let ws = |s| Loaded::Ready(Some(info(s)));
        // An older daemon, whatever else is known.
        assert_eq!(
            pane(false, &ws(WorkspaceStatus::Active), &ready),
            Pane::Unsupported
        );
        // A run that never had a workspace shows nothing.
        assert_eq!(
            pane(true, &Loaded::Ready(None), &Loaded::Loading),
            Pane::Hidden
        );
        assert_eq!(
            pane(true, &ws(WorkspaceStatus::Removed), &ready),
            Pane::Removed {
                branch: "reno/x".into()
            }
        );
        assert_eq!(
            pane(true, &ws(WorkspaceStatus::Active), &Loaded::Loading),
            Pane::Loading
        );
        assert_eq!(
            pane(true, &Loaded::Loading, &Loaded::Loading),
            Pane::Loading
        );
        assert_eq!(
            pane(
                true,
                &ws(WorkspaceStatus::Kept),
                &Loaded::Failed("boom".into())
            ),
            Pane::Failed("boom".into())
        );
        assert_eq!(
            pane(
                true,
                &ws(WorkspaceStatus::Kept),
                &Loaded::Ready(diff(0, vec![]))
            ),
            Pane::Empty {
                branch: "reno/x".into()
            }
        );
        let Pane::Files(o) = pane(true, &ws(WorkspaceStatus::Kept), &ready) else {
            panic!("files");
        };
        assert_eq!(o.commits, "2 commits");
        assert_eq!(o.stats, "1 file changed (+1 \u{2212}0)");
        assert_eq!(o.path.as_deref(), Some("/w/x"));
        // The list comes even when the workspace could not be read.
        let Pane::Files(o) = pane(true, &Loaded::Failed("x".into()), &ready) else {
            panic!("files");
        };
        assert_eq!(o.path, None);
    }

    fn rows_of(patch: &str) -> Vec<(RowKind, Option<u32>, Option<u32>, String)> {
        parse_patch(patch)
            .into_iter()
            .map(|r| (r.kind, r.old, r.new, r.text))
            .collect()
    }

    #[test]
    fn a_patch_becomes_rows_with_line_numbers() {
        let patch = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n\
@@ -3,4 +3,5 @@ fn main() {\n keep\n-old\n+new\n+more\n tail\n";
        let got = rows_of(patch);
        use RowKind::*;
        assert_eq!(
            got,
            [
                (Hunk, None, None, "@@ -3,4 +3,5 @@ fn main() {".to_owned()),
                (Context, Some(3), Some(3), "keep".to_owned()),
                (Del, Some(4), None, "old".to_owned()),
                (Add, None, Some(4), "new".to_owned()),
                (Add, None, Some(5), "more".to_owned()),
                (Context, Some(5), Some(6), "tail".to_owned()),
            ]
        );
    }

    #[test]
    fn two_hunks_restart_their_numbers() {
        let patch = "@@ -1,2 +1,2 @@\n-a\n+b\n c\n@@ -20 +20,2 @@\n z\n+y\n";
        let got = rows_of(patch);
        assert_eq!(got.len(), 7);
        assert_eq!((got[4].1, got[4].2), (None, None));
        assert_eq!((got[5].1, got[5].2), (Some(20), Some(20)));
        assert_eq!((got[6].1, got[6].2), (None, Some(21)));
    }

    #[test]
    fn no_newline_markers_and_blank_context_are_kept() {
        let patch = "@@ -1,2 +1,2 @@\n-a\n\\ No newline at end of file\n+a\n\n\\ No newline at end of file\n";
        let got = rows_of(patch);
        use RowKind::*;
        assert_eq!(
            got[2],
            (
                NoNewline,
                None,
                None,
                "No newline at end of file".to_owned()
            )
        );
        assert_eq!(got[3], (Add, None, Some(1), "a".to_owned()));
        // The blank line is the context line whose trailing space was stripped.
        assert_eq!(got[4], (Context, Some(2), Some(2), String::new()));
        assert_eq!(got[5].0, NoNewline);
    }

    #[test]
    fn a_deleted_line_that_looks_like_a_header_is_still_a_line() {
        let patch = "--- a/f\n+++ b/f\n@@ -1,2 +1 @@\n--- not a header\n x\n";
        let got = rows_of(patch);
        assert_eq!(
            got[1],
            (RowKind::Del, Some(1), None, "-- not a header".to_owned())
        );
        assert_eq!(got[2].0, RowKind::Context);
    }

    #[test]
    fn renames_skip_their_header_and_binary_files_say_so() {
        let patch = "diff --git a/o b/n\nsimilarity index 91%\nrename from o\nrename to n\n\
index 1..2 100644\n--- a/o\n+++ b/n\n@@ -1 +1 @@\n-x\n+y\n";
        assert_eq!(rows_of(patch).len(), 3);
        let binary = "diff --git a/p.png b/p.png\nindex 1..2 100644\nBinary files a/p.png and b/p.png differ\n";
        assert_eq!(parse_patch(binary)[0].kind, RowKind::Binary);
        let f = FileRow {
            path: "p.png".into(),
            from: None,
            kind: ChangeKind::Modified,
            letter: 'M',
            tone: KindTone::Warning,
            dir: String::new(),
            name: "p.png".into(),
            additions: 0,
            deletions: 0,
            binary: false,
        };
        let loaded = Loaded::Ready(Patch {
            text: Some(binary.to_owned()),
            truncated: false,
            checksum: String::new(),
        });
        assert_eq!(patch_pane(&f, &loaded), PatchPane::Binary);
    }

    #[test]
    fn the_patch_pane_has_its_states() {
        let mut f = FileRow {
            path: "a.rs".into(),
            from: None,
            kind: ChangeKind::Modified,
            letter: 'M',
            tone: KindTone::Warning,
            dir: String::new(),
            name: "a.rs".into(),
            additions: 1,
            deletions: 0,
            binary: false,
        };
        assert_eq!(patch_pane(&f, &Loaded::Loading), PatchPane::Loading);
        assert_eq!(
            patch_pane(&f, &Loaded::Failed("no".into())),
            PatchPane::Failed("no".into())
        );
        assert_eq!(
            patch_pane(
                &f,
                &Loaded::Ready(Patch {
                    text: None,
                    truncated: false,
                    checksum: String::new(),
                })
            ),
            PatchPane::Nothing
        );
        let cut = Loaded::Ready(Patch {
            text: Some("@@ -1 +1 @@\n-a\n+b\n".to_owned()),
            truncated: true,
            checksum: String::new(),
        });
        match patch_pane(&f, &cut) {
            PatchPane::Rows { rows, truncated } => assert!(truncated && rows.len() == 3),
            other => panic!("{other:?}"),
        }
        f.binary = true;
        assert_eq!(patch_pane(&f, &Loaded::Loading), PatchPane::Binary);
    }

    #[test]
    fn a_patch_cut_mid_hunk_still_reads() {
        let got = rows_of("@@ -1,9 +1,9 @@\n a\n-b\n");
        assert_eq!(got.len(), 3);
        assert_eq!(
            gutter_digits(&parse_patch("@@ -98,3 +98,3 @@\n a\n b\n c\n")),
            3
        );
    }
}
