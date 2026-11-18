//! A sign-in a person started from a client, as the daemon runs it: [`AcpExecutor::sign_in_registry`]
//! with its pages and lines turned into what the client is told.
//!
//! Nothing here opens a page. The agent's own `$BROWSER`, and the system's `open` and `xdg-open`
//! it may call instead, are replaced for this one child with scripts that write the address to a
//! named pipe in a folder only this process can read; the address then goes to the client, which
//! opens it on the device the person is at. The pipe is read as it is written (nothing polls) and
//! the folder is removed when the sign-in ends, whichever way it ends.
//!
//! The agent's own login is what runs: its home, its credentials, its Keychain access. Reno
//! changes none of them (it overrides no `HOME`) and writes none.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use reno_proto::SigninEvent;

use super::AcpExecutor;
use super::signin::{NEEDS_TERMINAL, SignInOptions, SignInProgress};
use crate::HarnessError;
use crate::exec::SignInEvents;

/// A line longer than this is cut: a device code and a page fit in far less.
const MAX_LINE: usize = 1024;

/// Pages one sign-in may report; a loop that prints a new address forever is cut off.
const MAX_PAGES: usize = 16;

/// Output lines one sign-in may report.
const MAX_LINES: usize = 200;

/// The sign-in's own folder, removed when it is dropped.
struct Folder(PathBuf);

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A named pipe at `path`, readable and writable by this user alone.
#[cfg(unix)]
#[expect(
    unsafe_code,
    reason = "mkfifo(3) has no safe std equivalent, and rustix has none on macOS"
)]
fn make_fifo(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("a path with a NUL in it"))?;
    // SAFETY: `c_path` is a valid NUL-terminated path; the call only creates a pipe there.
    if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
pub(super) async fn run(
    harness: &AcpExecutor,
    method: Option<String>,
    events: SignInEvents,
) -> Result<(), HarnessError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    use tokio::io::AsyncBufReadExt;
    use tokio::net::unix::pipe;

    let folder = std::env::temp_dir().join(format!("reno-signin-{}", uuid::Uuid::new_v4()));
    std::fs::DirBuilder::new().mode(0o700).create(&folder)?;
    let folder = Folder(folder);
    let pages = folder.0.join("pages");
    make_fifo(&pages)?;
    // `$BROWSER`: the address, written to the pipe.
    let browser = folder.0.join("browser");
    std::fs::write(
        &browser,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\n",
            pages.display().to_string().replace('\'', "'\\''")
        ),
    )?;
    std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o700))?;

    // The reading end first; a writing end of our own keeps the pipe from ending whenever the
    // agent's last writer closes (and a writer's `open` from waiting for a reader).
    let reader = pipe::OpenOptions::new().open_receiver(&pages)?;
    let _keep_open = pipe::OpenOptions::new().open_sender(&pages)?;

    let seen: Arc<Mutex<HashSet<String>>> = Arc::default();
    let report_page = {
        let (events, seen) = (events.clone(), seen.clone());
        Arc::new(move |url: String| {
            let mut seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
            if seen.len() < MAX_PAGES && seen.insert(url.clone()) {
                events(SigninEvent::Page { url });
            }
        })
    };
    let reading = {
        let report_page = report_page.clone();
        harness.tracker.spawn(async move {
            let mut lines = tokio::io::BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim();
                if line.starts_with("http://") || line.starts_with("https://") {
                    report_page(line.to_owned());
                }
            }
        })
    };
    let _reading = AbortOnDrop(reading.abort_handle());

    let lines = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let options = SignInOptions {
        browser: Some(browser),
        method,
        env: vec![("RENO_LOGIN_URL_FILE".into(), pages.clone().into_os_string())],
    };
    let outcome = harness
        .sign_in_registry(options, {
            let (events, report_page) = (events.clone(), report_page.clone());
            move |progress| match progress {
                SignInProgress::OpenBrowser(url) => report_page(url),
                SignInProgress::Output(line) => {
                    if lines.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < MAX_LINES {
                        // For the person at the device: a device code has to stay readable, so
                        // this is the credential-marker redaction, not the log one.
                        let mut line = crate::redact_secrets(&crate::redact::strip_ansi(&line));
                        line.truncate(line.floor_char_boundary(MAX_LINE));
                        if !line.trim().is_empty() {
                            events(SigninEvent::Output { line });
                        }
                    }
                }
            }
        })
        .await;
    match outcome {
        Err(HarnessError::Protocol(why)) if why.starts_with(NEEDS_TERMINAL) => {
            let command = why[NEEDS_TERMINAL.len()..].to_owned();
            events(SigninEvent::NeedsTerminal { command });
            Err(HarnessError::Protocol(format!(
                "{} signs in in a terminal: run the command shown there",
                harness.spec.display_name
            )))
        }
        other => other,
    }
}

#[cfg(not(unix))]
pub(super) async fn run(
    _: &AcpExecutor,
    _: Option<String>,
    _: SignInEvents,
) -> Result<(), HarnessError> {
    Err(HarnessError::Protocol(
        "signing in from a client is not supported on this platform".into(),
    ))
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
