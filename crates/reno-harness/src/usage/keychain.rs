//! A read of one macOS Keychain item through the `security` program. Read only: there is no
//! `add-generic-password` here.
//!
//! The first read of an item another program created makes macOS ask the person whether this
//! program may use it. `security` blocks until they answer, so every call is bounded; a
//! question nobody answered is "unavailable", not "no login".

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

/// An unanswered consent dialog blocks `security` indefinitely.
const BUDGET: Duration = Duration::from_secs(15);

/// `security`'s exit status for `errSecItemNotFound`.
const ITEM_NOT_FOUND: i32 = 44;

/// What reading one item found.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Read {
    /// The item's JSON.
    Found(Value),
    /// The Keychain answered: there is no such item. Only then may a file be believed instead.
    Absent,
    /// No answer (refused, timed out, unparseable): the item may well exist.
    Unavailable,
}

/// The item `service` of `account`, as JSON. Two steps, as zeron does: a metadata lookup (it
/// needs no authorisation) tells "no item" from "access refused", then `-w` reads the secret.
/// `security` is named by its absolute path so a planted program on `PATH` never sees a secret.
pub(super) async fn read_json(security: &Path, service: &str, account: &str) -> Read {
    match run(security, &["find-generic-password", "-s", service]).await {
        Some((Some(0), _)) => {}
        Some((Some(ITEM_NOT_FOUND), _)) => return Read::Absent,
        _ => return Read::Unavailable,
    }
    let Some((Some(0), secret)) = run(
        security,
        &["find-generic-password", "-a", account, "-s", service, "-w"],
    )
    .await
    else {
        return Read::Unavailable;
    };
    match serde_json::from_str(secret.trim()) {
        Ok(value) => Read::Found(value),
        Err(_) => Read::Unavailable,
    }
}

/// Runs `security` and returns its exit code and stdout; `None` when it could not run or did
/// not finish in time.
async fn run(security: &Path, args: &[&str]) -> Option<(Option<i32>, String)> {
    let output = Command::new(security)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(BUDGET, output).await.ok()?.ok()?;
    Some((
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}
