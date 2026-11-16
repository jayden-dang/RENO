//! A log file that rotates by size, so logs never grow without bound.
//!
//! The current file is `<name>`; when a write would take it past `max_bytes` it becomes
//! `<name>.1`, the old `<name>.1` becomes `<name>.2`, and so on; the oldest of `files` files is
//! deleted. With 10 MiB and 3 files the logs take at most 30 MiB. A line is never split across
//! files: the rotation happens before a write that would not fit, and a single write larger than
//! the limit gets a file to itself.
//!
//! Rotation is by rename, so a reader that holds the old file open (`reno daemon logs -f`) keeps
//! reading it to the end and then finds the new file under the old name.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The size at which the current log file is rotated.
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;
/// How many files are kept: the current one and the rotated ones.
pub const FILES: usize = 3;

/// A size-rotated log file. See the module docs.
#[derive(Debug)]
pub struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    files: usize,
    file: File,
    written: u64,
}

fn rotated(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

impl RotatingFile {
    /// Opens `path` for appending, rotating first if it is already at the limit.
    pub fn open(path: PathBuf, max_bytes: u64, files: usize) -> io::Result<Self> {
        let file = open_append(&path)?;
        let written = file.metadata()?.len();
        let mut log = Self {
            path,
            max_bytes: max_bytes.max(1),
            files: files.max(1),
            file,
            written,
        };
        if log.written >= log.max_bytes {
            log.rotate()?;
        }
        Ok(log)
    }

    fn rotate(&mut self) -> io::Result<()> {
        if self.files <= 1 {
            // Nothing to keep: start the file over.
            self.file.set_len(0)?;
            self.written = 0;
            return Ok(());
        }
        // Shift `.n-1` to `.n` from the oldest down; the oldest falls off the end.
        for n in (1..self.files - 1).rev() {
            match std::fs::rename(rotated(&self.path, n), rotated(&self.path, n + 1)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return self.give_up(e),
            }
        }
        if let Err(e) = std::fs::rename(&self.path, rotated(&self.path, 1)) {
            return self.give_up(e);
        }
        self.file = open_append(&self.path)?;
        self.written = 0;
        Ok(())
    }

    /// A rename failed (the directory is read-only, say). The bound still holds: the current
    /// file is emptied rather than left to grow.
    fn give_up(&mut self, error: io::Error) -> io::Result<()> {
        self.file.set_len(0)?;
        self.written = 0;
        Err(error)
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        if self.written > 0 && self.written.saturating_add(len) > self.max_bytes {
            // A failed rotation has already emptied the file; the line is still written.
            let _ = self.rotate();
        }
        let n = self.file.write(buf)?;
        self.written = self
            .written
            .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn it_rotates_before_a_write_that_would_not_fit_and_keeps_a_bounded_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("renod.log");
        let mut log = RotatingFile::open(path.clone(), 10, 3).expect("open");
        for line in ["aaaa\n", "bbbb\n", "cccc\n", "dddd\n", "eeee\n", "ffff\n"] {
            log.write_all(line.as_bytes()).expect("write");
        }
        // Two lines fit in 10 bytes; the third rotates.
        assert_eq!(read(&path), "eeee\nffff\n");
        assert_eq!(read(&rotated(&path, 1)), "cccc\ndddd\n");
        assert_eq!(read(&rotated(&path, 2)), "aaaa\nbbbb\n");
        log.write_all(b"gggg\n").expect("write");
        assert_eq!(read(&path), "gggg\n");
        assert_eq!(
            read(&rotated(&path, 2)),
            "cccc\ndddd\n",
            "the oldest is gone"
        );
        assert!(!rotated(&path, 3).exists());
    }

    #[test]
    fn a_file_already_at_the_limit_is_rotated_on_open_and_a_big_line_is_kept_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("renod.log");
        std::fs::write(&path, "0123456789ab").expect("seed");
        let mut log = RotatingFile::open(path.clone(), 10, 2).expect("open");
        assert_eq!(read(&path), "");
        assert_eq!(read(&rotated(&path, 1)), "0123456789ab");
        log.write_all(b"a line longer than ten bytes\n")
            .expect("write");
        assert_eq!(read(&path), "a line longer than ten bytes\n");
        log.write_all(b"x\n").expect("write");
        assert_eq!(read(&path), "x\n");
        assert_eq!(read(&rotated(&path, 1)), "a line longer than ten bytes\n");
    }

    #[test]
    fn appending_resumes_after_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("renod.log");
        RotatingFile::open(path.clone(), 100, 3)
            .expect("open")
            .write_all(b"one\n")
            .expect("write");
        RotatingFile::open(path.clone(), 100, 3)
            .expect("reopen")
            .write_all(b"two\n")
            .expect("write");
        assert_eq!(read(&path), "one\ntwo\n");
    }
}
