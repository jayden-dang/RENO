//! Per-run JSONL journals: `journals/<run_id>.jsonl`.
//!
//! Each line is `{"seq":n,"event":<ExecEvent>}`, with a per-run `seq` starting at 1. A journal
//! holds a run's streamed events for display and replay.
//!
//! # Not a source of truth
//!
//! **Nothing reads a journal to decide state (ARCH-8).** SQLite alone says what a run is. The
//! only flow between the two is state to journal: after a crash, recovery calls
//! [`Journals::ensure_done`] for runs that SQLite says are terminal, so the transcript ends with
//! its `Done`. A journal's end marker is written only after the run's final state commits.
//!
//! # Torn tails
//!
//! A crash can leave a final line without its newline, or cut mid-JSON. Readers skip any line
//! that does not parse (with a warning). A writer that reopens such a file first writes a newline
//! so its next append starts on a fresh line, and continues the seq after the last good line.
//!
//! These functions do blocking file I/O; call them from `spawn_blocking` or a dedicated task.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use reno_proto::{ExecEvent, RunId, Seq};
use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};

#[derive(Serialize)]
struct LineRef<'a> {
    seq: Seq,
    event: &'a ExecEvent,
}

#[derive(Deserialize)]
struct Line {
    seq: Seq,
    event: ExecEvent,
}

/// What a scan of an existing journal found.
struct Scan {
    last: Option<(Seq, ExecEvent)>,
    /// The file is empty or ends with a newline.
    ends_clean: bool,
}

fn scan(path: &Path) -> Result<Scan> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Scan {
                last: None,
                ends_clean: true,
            });
        }
        Err(e) => return Err(e.into()),
    };
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    let mut last = None;
    let mut ends_clean = true;
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        ends_clean = buf.last() == Some(&b'\n');
        if let Some(line) = parse_line(path, &buf) {
            last = Some((line.seq, line.event));
        }
    }
    Ok(Scan { last, ends_clean })
}

fn parse_line(path: &Path, raw: &[u8]) -> Option<Line> {
    let raw = raw.trim_ascii();
    if raw.is_empty() {
        return None;
    }
    match serde_json::from_slice(raw) {
        Ok(line) => Some(line),
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "skipping unparseable journal line");
            None
        }
    }
}

/// The journals directory. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Journals {
    dir: PathBuf,
}

impl Journals {
    /// Journals stored in `dir` (created on first write).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The file for `run`: `<dir>/<run_id>.jsonl`. Run ids are validated, so they are safe as
    /// file names.
    pub fn path(&self, run: &RunId) -> PathBuf {
        self.dir.join(format!("{run}.jsonl"))
    }

    /// Opens the writer for `run`, continuing its seq if the journal exists.
    pub fn writer(&self, run: &RunId) -> Result<JournalWriter> {
        JournalWriter::open(self.path(run))
    }

    /// Iterates the events of `run` with `seq >= from_seq`. A run with no journal yields
    /// nothing. Lines that do not parse (a torn tail) are skipped.
    pub fn replay(&self, run: &RunId, from_seq: Seq) -> Result<Replay> {
        let path = self.path(run);
        let reader = match File::open(&path) {
            Ok(f) => Some(BufReader::new(f)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        Ok(Replay {
            path,
            reader,
            from: from_seq,
            buf: Vec::new(),
        })
    }

    /// The last parseable event of `run`, with its seq.
    pub fn last_event(&self, run: &RunId) -> Result<Option<(Seq, ExecEvent)>> {
        Ok(scan(&self.path(run))?.last)
    }

    /// Journal repair, state to journal: appends `done` unless the journal's last event is
    /// already a `Done`. Returns whether it appended. Recovery calls this for runs that SQLite
    /// records as terminal; it is the only way a journal is "fixed", and nothing reads the
    /// result to decide state. `done` must be an [`ExecEvent::Done`].
    pub fn ensure_done(&self, run: &RunId, done: &ExecEvent) -> Result<bool> {
        if !matches!(done, ExecEvent::Done { .. }) {
            return Err(StoreError::InvalidArgument(
                "ensure_done needs a Done event".into(),
            ));
        }
        let mut writer = self.writer(run)?;
        if matches!(writer.last, Some((_, ExecEvent::Done { .. }))) {
            return Ok(false);
        }
        writer.append(done)?;
        writer.flush()?;
        Ok(true)
    }
}

/// Appends events to one run's journal. Appends are buffered; call [`flush`](Self::flush) to push
/// them to the file. Dropping the writer flushes on a best-effort basis.
#[derive(Debug)]
pub struct JournalWriter {
    out: BufWriter<File>,
    next: Seq,
    last: Option<(Seq, ExecEvent)>,
}

impl JournalWriter {
    fn open(path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let scanned = scan(&path)?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut out = BufWriter::new(file);
        if !scanned.ends_clean {
            // Cut off a torn final line so the next append starts on a fresh one.
            out.write_all(b"\n")?;
        }
        let next = match &scanned.last {
            Some((seq, _)) => seq
                .next()
                .ok_or_else(|| StoreError::Corrupt("journal seq overflow".into()))?,
            None => Seq(1),
        };
        Ok(Self {
            out,
            next,
            last: scanned.last,
        })
    }

    /// Appends `event` and returns the seq it was given (1 for the first event of a run).
    pub fn append(&mut self, event: &ExecEvent) -> Result<Seq> {
        let seq = self.next;
        let after = seq
            .next()
            .ok_or_else(|| StoreError::Corrupt("journal seq overflow".into()))?;
        let mut line = serde_json::to_vec(&LineRef { seq, event })?;
        line.push(b'\n');
        self.out.write_all(&line)?;
        self.next = after;
        self.last = Some((seq, event.clone()));
        Ok(seq)
    }

    /// The seq the next [`append`](Self::append) will give: one past the last event, 1 for an
    /// empty journal. A reader that registers while it holds the writer knows exactly which
    /// events the journal file already contains.
    pub fn next_seq(&self) -> Seq {
        self.next
    }

    /// Flushes buffered appends to the file.
    pub fn flush(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

/// Iterator returned by [`Journals::replay`].
#[derive(Debug)]
pub struct Replay {
    path: PathBuf,
    reader: Option<BufReader<File>>,
    from: Seq,
    buf: Vec<u8>,
}

impl Iterator for Replay {
    type Item = Result<(Seq, ExecEvent)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let reader = self.reader.as_mut()?;
            self.buf.clear();
            match reader.read_until(b'\n', &mut self.buf) {
                Ok(0) => {
                    self.reader = None;
                    return None;
                }
                Ok(_) => {}
                Err(e) => {
                    self.reader = None;
                    return Some(Err(e.into()));
                }
            }
            if let Some(line) = parse_line(&self.path, &self.buf)
                && line.seq >= self.from
            {
                return Some(Ok((line.seq, line.event)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use reno_proto::StopReason;

    use super::*;

    fn text(s: &str) -> ExecEvent {
        ExecEvent::Text { text: s.into() }
    }

    fn done() -> ExecEvent {
        ExecEvent::Done {
            stop: StopReason::Completed,
            result: None,
            diagnostic: None,
        }
    }

    fn run() -> RunId {
        RunId::from_suffix("j1").unwrap()
    }

    fn collect(j: &Journals, from: u64) -> Vec<(u64, ExecEvent)> {
        j.replay(&run(), Seq(from))
            .unwrap()
            .map(|r| {
                let (s, e) = r.unwrap();
                (s.0, e)
            })
            .collect()
    }

    #[test]
    fn append_and_replay() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journals::new(dir.path().join("journals"));
        let mut w = j.writer(&run()).unwrap();
        assert_eq!(w.append(&text("a")).unwrap(), Seq(1));
        assert_eq!(w.append(&text("b")).unwrap(), Seq(2));
        w.flush().unwrap();
        assert_eq!(collect(&j, 1), vec![(1, text("a")), (2, text("b"))]);
        assert_eq!(j.last_event(&run()).unwrap(), Some((Seq(2), text("b"))));
        let first = std::fs::read_to_string(j.path(&run())).unwrap();
        assert!(first.starts_with("{\"seq\":1,\"event\":{\"type\":\"text\""));
    }

    #[test]
    fn replay_from_mid_seq_and_missing_journal() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journals::new(dir.path());
        assert!(collect(&j, 1).is_empty());
        assert_eq!(j.last_event(&run()).unwrap(), None);
        let mut w = j.writer(&run()).unwrap();
        for i in 0..5 {
            w.append(&text(&i.to_string())).unwrap();
        }
        w.flush().unwrap();
        let got = collect(&j, 3);
        assert_eq!(
            got.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        assert!(collect(&j, 6).is_empty());
    }

    #[test]
    fn seq_continues_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journals::new(dir.path());
        let mut w = j.writer(&run()).unwrap();
        w.append(&text("a")).unwrap();
        w.append(&text("b")).unwrap();
        drop(w);
        let mut w = j.writer(&run()).unwrap();
        assert_eq!(w.append(&text("c")).unwrap(), Seq(3));
        w.flush().unwrap();
        assert_eq!(collect(&j, 1).len(), 3);
    }

    #[test]
    fn torn_tail_is_skipped_and_next_append_is_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journals::new(dir.path());
        let mut w = j.writer(&run()).unwrap();
        w.append(&text("a")).unwrap();
        w.append(&text("b")).unwrap();
        w.flush().unwrap();
        drop(w);
        let mut f = OpenOptions::new()
            .append(true)
            .open(j.path(&run()))
            .unwrap();
        f.write_all(b"{\"seq\":3,\"event\":{\"type\":\"te").unwrap();
        drop(f);

        assert_eq!(collect(&j, 1).len(), 2, "replay skips the torn line");
        assert_eq!(j.last_event(&run()).unwrap(), Some((Seq(2), text("b"))));

        let mut w = j.writer(&run()).unwrap();
        assert_eq!(w.append(&text("c")).unwrap(), Seq(3));
        w.flush().unwrap();
        drop(w);
        let got = collect(&j, 1);
        assert_eq!(
            got,
            vec![(1, text("a")), (2, text("b")), (3, text("c"))],
            "the new line is intact"
        );
    }

    #[test]
    fn complete_line_missing_only_its_newline_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journals::new(dir.path());
        let mut w = j.writer(&run()).unwrap();
        w.append(&text("a")).unwrap();
        w.flush().unwrap();
        drop(w);
        let path = j.path(&run());
        let mut raw = std::fs::read(&path).unwrap();
        raw.pop();
        std::fs::write(&path, raw).unwrap();
        let mut w = j.writer(&run()).unwrap();
        assert_eq!(w.append(&text("b")).unwrap(), Seq(2));
        w.flush().unwrap();
        assert_eq!(collect(&j, 1).len(), 2);
    }

    #[test]
    fn ensure_done_appends_only_when_needed() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journals::new(dir.path());
        // No journal at all: creates it.
        assert!(j.ensure_done(&run(), &done()).unwrap());
        assert_eq!(j.last_event(&run()).unwrap(), Some((Seq(1), done())));
        // Already ends in Done: untouched.
        assert!(!j.ensure_done(&run(), &done()).unwrap());
        assert_eq!(collect(&j, 1).len(), 1);

        // Ends in something else: appended after it.
        let other = RunId::from_suffix("j2").unwrap();
        let mut w = j.writer(&other).unwrap();
        w.append(&text("a")).unwrap();
        w.flush().unwrap();
        drop(w);
        assert!(j.ensure_done(&other, &done()).unwrap());
        assert_eq!(j.last_event(&other).unwrap(), Some((Seq(2), done())));

        // Only a Done is accepted.
        assert!(matches!(
            j.ensure_done(&other, &text("x")),
            Err(StoreError::InvalidArgument(_))
        ));
    }
}
