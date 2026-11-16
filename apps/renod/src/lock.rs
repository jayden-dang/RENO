//! The data-directory lock: one daemon per data directory (ARCHITECTURE 3.2).
//!
//! An exclusive advisory `flock` on `<data-dir>/engine.lock`, taken before the store is opened
//! and held until the lock is dropped. The kernel releases it when the process dies, however it
//! dies, so a crash never leaves a stale lock behind. The file holds the owner's pid for the
//! message a second daemon prints; the pid is information, the `flock` is the lock.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;

/// Why the lock could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another process holds the lock.
    #[error(
        "another renod is already running on this data directory (lock {} is held{})",
        path.display(),
        holder.map_or_else(String::new, |pid| format!(" by pid {pid}"))
    )]
    Held {
        /// The lock file.
        path: PathBuf,
        /// The pid the holder wrote into it, when it could be read.
        holder: Option<u32>,
    },
    /// The lock file could not be opened or locked.
    #[error("cannot lock {}: {source}", path.display())]
    Io {
        /// The lock file.
        path: PathBuf,
        /// What failed.
        source: io::Error,
    },
}

/// An exclusive lock on the data directory. Dropping it releases the lock.
#[derive(Debug)]
pub struct DataDirLock {
    // Held for its descriptor: closing the file releases the `flock`.
    _file: File,
    path: PathBuf,
}

impl DataDirLock {
    /// Takes the lock without waiting, or says who holds it.
    pub fn acquire(path: &Path) -> Result<Self, LockError> {
        let io_error = |source| LockError::Io {
            path: path.to_path_buf(),
            source,
        };
        // No `truncate`: a loser must not wipe the holder's pid before it finds the lock held.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(io_error)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(Errno::WOULDBLOCK) => {
                return Err(LockError::Held {
                    path: path.to_path_buf(),
                    holder: read_pid(&mut file),
                });
            }
            Err(errno) => return Err(io_error(errno.into())),
        }
        file.set_len(0).map_err(io_error)?;
        file.write_all(std::process::id().to_string().as_bytes())
            .map_err(io_error)?;
        Ok(Self {
            _file: file,
            path: path.to_path_buf(),
        })
    }

    /// The lock file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn read_pid(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_lock_is_refused_and_names_the_holder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("engine.lock");
        let first = DataDirLock::acquire(&path).expect("first");
        match DataDirLock::acquire(&path) {
            Err(LockError::Held { holder, .. }) => {
                assert_eq!(holder, Some(std::process::id()));
            }
            other => panic!("expected Held, got {other:?}"),
        }
        let message = DataDirLock::acquire(&path)
            .expect_err("still held")
            .to_string();
        assert!(message.contains("already running"), "{message}");
        assert!(message.contains("engine.lock"), "{message}");
        drop(first);
    }

    #[test]
    fn the_lock_is_released_when_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("engine.lock");
        drop(DataDirLock::acquire(&path).expect("first"));
        // A `flock` belongs to the open file description. Another test in this process may fork
        // a child between our open and its `exec`; until that `exec` closes the inherited
        // descriptor (it is close-on-exec), the child shares the lock. So the lock is free again
        // within that window, not necessarily at the instant of the drop.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let again = loop {
            match DataDirLock::acquire(&path) {
                Ok(lock) => break lock,
                Err(LockError::Held { .. }) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("free again: {error}"),
            }
        };
        assert_eq!(again.path(), path);
    }
}
