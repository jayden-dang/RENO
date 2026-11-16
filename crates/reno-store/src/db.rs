//! Opening the database: connections, PRAGMAs, migrations, and the [`Store`] handle.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use rusqlite::{Connection, OpenFlags};

use crate::error::{DbResult, Result, StoreError};
use crate::journal::Journals;
use crate::migrations;
use crate::writer::{self, Command, Durability, StoreConfig, WriterStats};

/// Number of read-only connections.
const READERS: usize = 2;
/// Page cache per connection, in KiB (a negative `cache_size` is KiB).
const CACHE_KIB: i64 = 2048;

/// A cheap, cloneable handle to the database.
///
/// One write connection lives on a dedicated writer thread (see [`StoreConfig`] for its tuning);
/// two read-only connections serve reads on `spawn_blocking`. Writes are group-committed and
/// each is atomic with the event row that records it (ARCH-8). The store never reads the clock:
/// every time-dependent method takes `now` in milliseconds since the epoch.
///
/// The writer thread stops when the last handle is dropped.
#[derive(Debug, Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    /// `None` only while dropping.
    tx: Option<SyncSender<Command>>,
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicUsize,
    writer: Mutex<Option<JoinHandle<()>>>,
    #[cfg_attr(not(test), allow(dead_code))]
    stats: Arc<WriterStats>,
    root: PathBuf,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Closing the channel ends the writer loop; joining makes the file lock release
        // deterministic for a reopen.
        self.tx.take();
        let handle = self
            .writer
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle
            && handle.join().is_err()
        {
            tracing::error!("store writer thread panicked");
        }
    }
}

fn apply_pragmas(conn: &Connection, read_only: bool) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", true).db()?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))
        .db()?;
    conn.pragma_update(None, "cache_size", -CACHE_KIB).db()?;
    if read_only {
        conn.pragma_update(None, "query_only", true).db()?;
    } else {
        let mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))
            .db()?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::Database(
                format!("journal_mode is {mode}, expected wal").into(),
            ));
        }
        conn.pragma_update(None, "synchronous", "NORMAL").db()?;
    }
    Ok(())
}

impl Store {
    /// Opens (creating if needed) the database at `path` with the default [`StoreConfig`].
    ///
    /// Blocks while it migrates, so call it before the async runtime is busy or from
    /// `spawn_blocking`. Journals live in a `journals/` directory next to the database file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, StoreConfig::default())
    }

    /// Like [`open`](Self::open) with explicit tuning.
    pub fn open_with(path: impl AsRef<Path>, config: StoreConfig) -> Result<Self> {
        let path = path.as_ref();
        let root = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        std::fs::create_dir_all(&root)?;

        let mut write_conn = Connection::open(path).db()?;
        apply_pragmas(&write_conn, false)?;
        migrations::run(&mut write_conn)?;

        let mut readers = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            let conn = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .db()?;
            apply_pragmas(&conn, true)?;
            readers.push(Mutex::new(conn));
        }

        let (tx, rx) = std::sync::mpsc::sync_channel(config.channel_capacity.max(1));
        let stats = Arc::new(WriterStats::default());
        let thread_stats = Arc::clone(&stats);
        let handle = std::thread::Builder::new()
            .name("reno-store-writer".into())
            .spawn(move || writer::run(write_conn, rx, &config, &thread_stats))?;

        Ok(Self {
            inner: Arc::new(Inner {
                tx: Some(tx),
                readers,
                next_reader: AtomicUsize::new(0),
                writer: Mutex::new(Some(handle)),
                stats,
                root,
            }),
        })
    }

    /// The journals directory (`journals/` next to the database file).
    pub fn journals(&self) -> Journals {
        Journals::new(self.inner.root.join("journals"))
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> &WriterStats {
        &self.inner.stats
    }

    fn sender(&self) -> Result<&SyncSender<Command>> {
        self.inner.tx.as_ref().ok_or(StoreError::Closed)
    }

    /// Queues a command without waiting. `Err(cmd)` hands the command back when the channel is
    /// full.
    pub(crate) fn try_submit(&self, cmd: Command) -> Result<std::result::Result<(), Command>> {
        match self.sender()?.try_send(cmd) {
            Ok(()) => Ok(Ok(())),
            Err(TrySendError::Full(cmd)) => Ok(Err(cmd)),
            Err(TrySendError::Disconnected(_)) => Err(StoreError::Closed),
        }
    }

    /// Queues a command, waiting off the executor when the channel is full.
    async fn submit(&self, cmd: Command) -> Result<()> {
        let Err(cmd) = self.try_submit(cmd)? else {
            return Ok(());
        };
        let tx = self.sender()?.clone();
        tokio::task::spawn_blocking(move || tx.send(cmd))
            .await
            .map_err(|_| StoreError::Closed)?
            .map_err(|_| StoreError::Closed)
    }

    /// Runs `f` on the writer thread inside its own savepoint, then waits for the group commit.
    pub(crate) async fn write<T, F>(&self, durability: Durability, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let (cmd, rx) = writer::command(durability, f);
        self.submit(cmd).await?;
        rx.await.map_err(|_| StoreError::Closed)?
    }

    /// Runs `f` on a read-only connection on the blocking pool.
    pub(crate) async fn read<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let n = inner.readers.len();
            let start = inner.next_reader.fetch_add(1, Ordering::Relaxed);
            let guard: MutexGuard<'_, Connection> = (0..n)
                .find_map(|i| inner.readers[(start + i) % n].try_lock().ok())
                .unwrap_or_else(|| {
                    inner.readers[start % n]
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                });
            f(&guard)
        })
        .await
        .map_err(|_| StoreError::Closed)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_sets_pragmas_on_every_connection() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("reno.db")).unwrap();
        let q = |conn: &Connection| -> Result<(i64, i64, String)> {
            let fk = conn
                .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
                .db()?;
            let bt = conn
                .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
                .db()?;
            let jm = conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .db()?;
            Ok((fk, bt, jm))
        };
        assert_eq!(
            store.write(Durability::Normal, q).await.unwrap(),
            (1, 5000, "wal".into())
        );
        for _ in 0..4 {
            assert_eq!(store.read(q).await.unwrap(), (1, 5000, "wal".into()));
        }
        let sync = store
            .write(Durability::Normal, |c| {
                c.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))
                    .db()
            })
            .await
            .unwrap();
        assert_eq!(sync, 1, "NORMAL");
    }

    #[tokio::test]
    async fn open_twice_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reno.db");
        drop(Store::open(&path).unwrap());
        let store = Store::open(&path).unwrap();
        let v = store
            .read(|c| {
                c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                    .db()
            })
            .await
            .unwrap();
        assert_eq!(v, 9);
    }
}
