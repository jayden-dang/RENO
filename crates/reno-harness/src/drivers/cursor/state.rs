//! Exclusive ownership of one Cursor conversation store while its shim is alive. The OS
//! releases the lock when a process dies, so recovery only ever runs after acquiring it.
//!
//! The stores live under Reno's own state root (`cursor-state`, beside the managed adapters),
//! never under the owner's `~/.cursor`.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::HarnessError;
use crate::wire::cursor::state::{AGENTS, BY_AGENT, OWNER_LOCK, STATE_DIR_NAME, valid_agent_id};

/// How long a run waits for the previous owner of a conversation to let go.
const WAIT: Duration = Duration::from_secs(5);

/// The directory of the conversation stores for managed adapters under `adapters`: a sibling
/// of it, so `RENO_ADAPTERS_DIR` and `RENO_DATA_DIR` move both together.
pub(super) fn state_root(adapters: &Path) -> PathBuf {
    match adapters.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(STATE_DIR_NAME),
        _ => adapters.join(format!(".{STATE_DIR_NAME}")),
    }
}

/// A held conversation store.
#[derive(Debug)]
pub(super) struct Lease {
    _file: File,
    /// The store directory the shim opens.
    pub(super) store_dir: PathBuf,
}

impl Lease {
    /// Take the store of a new conversation, or of `resume` (found through the index the shim
    /// keeps). An agent the index does not know cannot be resumed: the SDK's default store is
    /// never opened in its place.
    pub(super) async fn acquire(root: &Path, resume: Option<&str>) -> Result<Self, HarnessError> {
        let store_dir = match resume {
            Some(id) => {
                if !valid_agent_id(id) {
                    return Err(HarnessError::Protocol("Invalid Cursor session id".into()));
                }
                let marker = root.join(BY_AGENT).join(id);
                let found = match std::fs::read_to_string(&marker) {
                    Ok(path) => PathBuf::from(path.trim()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Err(HarnessError::Protocol(format!(
                            "Cursor conversation {id} is not in Reno's state directory, so it \
                             cannot be resumed"
                        )));
                    }
                    Err(e) => return Err(e.into()),
                };
                if found.as_os_str().is_empty() || !found.is_dir() {
                    return Err(HarnessError::Protocol(
                        "Cursor conversation storage is missing; restore its state directory \
                         before resuming"
                            .into(),
                    ));
                }
                found
            }
            None => root.join(AGENTS).join(uuid::Uuid::new_v4().to_string()),
        };
        std::fs::create_dir_all(&store_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&store_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(store_dir.join(OWNER_LOCK))?;
        let lock = async {
            loop {
                match file.try_lock() {
                    Ok(()) => return Ok(()),
                    Err(std::fs::TryLockError::WouldBlock) => {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Err(std::fs::TryLockError::Error(e)) => return Err(HarnessError::Io(e)),
                }
            }
        };
        tokio::time::timeout(WAIT, lock).await.map_err(|_| {
            HarnessError::Protocol(
                "This Cursor conversation is still running. Stop the current run before \
                 retrying."
                    .into(),
            )
        })??;
        Ok(Self {
            _file: file,
            store_dir,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_state_root_sits_beside_the_adapters() {
        assert_eq!(
            state_root(Path::new("/data/Reno/adapters")),
            Path::new("/data/Reno/cursor-state")
        );
        assert_eq!(
            state_root(Path::new("adapters")),
            Path::new("adapters/.cursor-state")
        );
    }

    #[tokio::test]
    async fn overlapping_resume_waits_for_the_previous_owner_to_release() {
        let root = tempfile::tempdir().unwrap();
        let first = Lease::acquire(root.path(), None).await.unwrap();
        std::fs::create_dir_all(root.path().join(BY_AGENT)).unwrap();
        std::fs::write(
            root.path().join(BY_AGENT).join("agent-test"),
            first.store_dir.to_str().unwrap(),
        )
        .unwrap();
        let second = Lease::acquire(root.path(), Some("agent-test"));
        tokio::pin!(second);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut second)
                .await
                .is_err()
        );
        drop(first);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), second)
                .await
                .unwrap()
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_conversation_the_index_does_not_know_is_refused_not_opened_elsewhere() {
        let root = tempfile::tempdir().unwrap();
        let error = Lease::acquire(root.path(), Some("agent-unknown"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cannot be resumed"), "{error}");
        // The marker points at a directory that is gone.
        std::fs::create_dir_all(root.path().join(BY_AGENT)).unwrap();
        std::fs::write(
            root.path().join(BY_AGENT).join("agent-gone"),
            root.path().join("nowhere").to_str().unwrap(),
        )
        .unwrap();
        let error = Lease::acquire(root.path(), Some("agent-gone"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("storage is missing"), "{error}");
        // An id that is not an SDK id never reaches the file system.
        let error = Lease::acquire(root.path(), Some("../etc"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Invalid"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_new_store_is_private_and_new_each_time() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let one = Lease::acquire(root.path(), None).await.unwrap();
        let two = Lease::acquire(root.path(), None).await.unwrap();
        assert_ne!(one.store_dir, two.store_dir);
        assert!(one.store_dir.starts_with(root.path().join(AGENTS)));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&one.store_dir), 0o700);
        assert_eq!(mode(&one.store_dir.join(OWNER_LOCK)), 0o600);
    }
}
