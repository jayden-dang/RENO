//! Review as a flat list of rows: each file is a header, then its patch lines (none while it is
//! folded), so a list can draw only the lines on screen. Pure: the rows are indices into the
//! files and their parsed patches, never copies of a line.

use super::changes::{FileRow, PatchPane, gutter_digits};

/// The most patch lines of one file that become rows; the daemon cuts a patch at 256 KiB.
pub(crate) const MAX_LINES: usize = 4_000;

/// What stands in for the lines of a patch that has none to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Note {
    Loading,
    Failed,
    Binary,
    Nothing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReviewRow {
    /// The file's bar: status letter, path, counts, open in its own tab.
    Header { file: usize },
    /// A one-line message in place of the patch.
    Note { file: usize, note: Note },
    /// Line `line` of the file's parsed patch. `digits` is the file's gutter width.
    Line {
        file: usize,
        line: usize,
        digits: usize,
    },
    /// The patch is longer than what shows: `shown` lines are here, the rest was cut.
    Cut { file: usize, shown: usize },
}

impl ReviewRow {
    pub(crate) fn file(&self) -> usize {
        match *self {
            Self::Header { file }
            | Self::Note { file, .. }
            | Self::Line { file, .. }
            | Self::Cut { file, .. } => file,
        }
    }
}

/// The rows of `files`. `folded` says which files show only their bar; `pane` is each file's
/// parsed patch, `None` while nothing was asked for it.
pub(crate) fn review_rows<'a>(
    files: &[FileRow],
    folded: impl Fn(&str) -> bool,
    pane: impl Fn(&str) -> Option<&'a PatchPane>,
) -> Vec<ReviewRow> {
    let mut out = Vec::new();
    for (file, row) in files.iter().enumerate() {
        out.push(ReviewRow::Header { file });
        if folded(&row.path) {
            continue;
        }
        let note = |note| ReviewRow::Note { file, note };
        match pane(&row.path) {
            None | Some(PatchPane::Loading) => out.push(note(Note::Loading)),
            Some(PatchPane::Failed(_)) => out.push(note(Note::Failed)),
            Some(PatchPane::Binary) => out.push(note(Note::Binary)),
            Some(PatchPane::Nothing) => out.push(note(Note::Nothing)),
            Some(PatchPane::Rows { rows, truncated }) => {
                let shown = rows.len().min(MAX_LINES);
                let digits = gutter_digits(rows);
                out.extend((0..shown).map(|line| ReviewRow::Line { file, line, digits }));
                if *truncated || rows.len() > MAX_LINES {
                    out.push(ReviewRow::Cut { file, shown });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::changes::{DiffRow, RowKind, file_rows};
    use reno_proto::{ChangeKind, ChangedFile};

    fn files(names: &[&str]) -> Vec<FileRow> {
        file_rows(
            &names
                .iter()
                .map(|n| ChangedFile {
                    path: (*n).to_owned(),
                    from: None,
                    kind: ChangeKind::Modified,
                    additions: 1,
                    deletions: 0,
                    binary: false,
                })
                .collect::<Vec<_>>(),
        )
    }

    fn lines(n: usize) -> Vec<DiffRow> {
        (0..n)
            .map(|i| DiffRow {
                kind: RowKind::Add,
                old: None,
                new: Some(i as u32 + 1),
                text: format!("line {i}"),
            })
            .collect()
    }

    fn line(file: usize, line: usize) -> ReviewRow {
        ReviewRow::Line {
            file,
            line,
            digits: 1,
        }
    }

    #[test]
    fn a_folded_file_drops_its_rows_and_an_open_one_has_a_row_per_line() {
        let fs = files(&["a.rs", "b.rs"]);
        let pane = PatchPane::Rows {
            rows: lines(3),
            truncated: false,
        };
        let rows = review_rows(&fs, |p| p == "a.rs", |_| Some(&pane));
        assert_eq!(
            rows,
            vec![
                ReviewRow::Header { file: 0 },
                ReviewRow::Header { file: 1 },
                line(1, 0),
                line(1, 1),
                line(1, 2),
            ]
        );
    }

    #[test]
    fn a_patch_without_lines_has_one_note_row() {
        let fs = files(&["a", "b", "c", "d", "e"]);
        let panes = [
            None,
            Some(PatchPane::Loading),
            Some(PatchPane::Failed("x".into())),
            Some(PatchPane::Binary),
            Some(PatchPane::Nothing),
        ];
        let rows = review_rows(
            &fs,
            |_| false,
            |p| panes[(p.as_bytes()[0] - b'a') as usize].as_ref(),
        );
        let notes: Vec<Note> = rows
            .iter()
            .filter_map(|r| match r {
                ReviewRow::Note { note, .. } => Some(*note),
                _ => None,
            })
            .collect();
        assert_eq!(
            notes,
            [
                Note::Loading,
                Note::Loading,
                Note::Failed,
                Note::Binary,
                Note::Nothing
            ]
        );
        assert_eq!(rows.len(), 10, "a bar and a note for each");
    }

    #[test]
    fn a_huge_patch_is_capped_and_says_so() {
        let fs = files(&["big.rs"]);
        let pane = PatchPane::Rows {
            rows: lines(5_000),
            truncated: false,
        };
        let rows = review_rows(&fs, |_| false, |_| Some(&pane));
        assert_eq!(rows.len(), 1 + MAX_LINES + 1);
        assert_eq!(
            rows.last(),
            Some(&ReviewRow::Cut {
                file: 0,
                shown: MAX_LINES
            })
        );
        // The gutter is as wide as the widest number in the whole patch, worked out once.
        assert!(matches!(rows[1], ReviewRow::Line { digits: 4, .. }));
        // A patch the daemon cut shows its notice too.
        let pane = PatchPane::Rows {
            rows: lines(10),
            truncated: true,
        };
        let rows = review_rows(&fs, |_| false, |_| Some(&pane));
        assert_eq!(rows.last(), Some(&ReviewRow::Cut { file: 0, shown: 10 }));
    }
}
