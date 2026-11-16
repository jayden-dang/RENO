//! Following a log file that is appended to and rotated: `reno daemon logs -f`.
//!
//! [`Follower`] reads what has been appended since it last looked; the caller decides when to
//! look, and `reno daemon logs -f` does so when the file system says the log directory changed
//! (`notify`), never on a timer. The daemon rotates its log by rename, so a follower notices that
//! the name now points at another file (by device and inode), reads the rest of the old one first,
//! and continues in the new one from its start. A file that shrank under the same name was
//! emptied, and is read again from the start.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// A reader that keeps its place in a growing, rotating file.
#[derive(Debug)]
pub(crate) struct Follower {
    path: PathBuf,
    file: Option<File>,
    /// Bytes of the open file already consumed.
    offset: u64,
    /// The start of a line that has not ended yet.
    partial: Vec<u8>,
}

impl Follower {
    /// Follows `path` from its current end, so that only new lines are returned. The file need
    /// not exist yet.
    pub(crate) fn from_end(path: &Path) -> io::Result<Self> {
        let mut follower = Self {
            path: path.to_path_buf(),
            file: None,
            offset: 0,
            partial: Vec::new(),
        };
        if let Some(file) = open(path)? {
            follower.offset = file.metadata()?.len();
            follower.file = Some(file);
        }
        Ok(follower)
    }

    /// Bytes of the file already consumed: where the next read starts.
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    /// The complete lines appended since the last call.
    pub(crate) fn read_new(&mut self) -> io::Result<Vec<String>> {
        let mut lines = Vec::new();
        self.drain(&mut lines)?;
        // Rotated away, or emptied in place: move to what the name points at now.
        let current = open(&self.path)?;
        let replaced = match (&self.file, &current) {
            (Some(old), Some(new)) => same_file(old, new).map(|same| !same)?,
            (None, Some(_)) => true,
            (_, None) => false,
        };
        if replaced {
            self.file = current;
            self.offset = 0;
            self.partial.clear();
            self.drain(&mut lines)?;
        } else if let (Some(file), Some(_)) = (&self.file, &current)
            && file.metadata()?.len() < self.offset
        {
            self.offset = 0;
            self.partial.clear();
            self.drain(&mut lines)?;
        }
        Ok(lines)
    }

    fn drain(&mut self, lines: &mut Vec<String>) -> io::Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        self.offset += u64::try_from(bytes.len()).unwrap_or(0);
        self.partial.extend_from_slice(&bytes);
        while let Some(end) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            if !text.trim().is_empty() {
                lines.push(text);
            }
        }
        Ok(())
    }
}

fn open(path: &Path) -> io::Result<Option<File>> {
    match File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn same_file(a: &File, b: &File) -> io::Result<bool> {
    let (a, b) = (a.metadata()?, b.metadata()?);
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn append(path: &Path, text: &str) {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open")
            .write_all(text.as_bytes())
            .expect("write");
    }

    #[test]
    fn only_lines_after_the_start_are_returned_and_a_cut_line_waits_for_its_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("renod.log");
        append(&path, "old\n");
        let mut follower = Follower::from_end(&path).expect("follow");
        assert!(follower.read_new().expect("read").is_empty());
        append(&path, "one\ntw");
        assert_eq!(follower.read_new().expect("read"), ["one"]);
        append(&path, "o\n\nthree\n");
        assert_eq!(follower.read_new().expect("read"), ["two", "three"]);
    }

    #[test]
    fn a_rotated_file_is_finished_before_the_new_one_is_started() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("renod.log");
        append(&path, "a\n");
        let mut follower = Follower::from_end(&path).expect("follow");
        // Lines written to the old file just before it was renamed, then a line in the new one.
        append(&path, "b\n");
        std::fs::rename(&path, dir.path().join("renod.log.1")).expect("rotate");
        append(&path, "c\n");
        assert_eq!(follower.read_new().expect("read"), ["b", "c"]);
        append(&path, "d\n");
        assert_eq!(follower.read_new().expect("read"), ["d"]);
    }

    #[test]
    fn a_file_that_appears_later_is_read_from_its_start_and_an_emptied_one_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("renod.log");
        let mut follower = Follower::from_end(&path).expect("follow");
        assert!(follower.read_new().expect("read").is_empty());
        append(&path, "first\nsecond\n");
        assert_eq!(follower.read_new().expect("read"), ["first", "second"]);
        std::fs::write(&path, "").expect("empty");
        append(&path, "again\n");
        assert_eq!(follower.read_new().expect("read"), ["again"]);
    }
}
