//! Opening a command in Terminal: the Sign in… and Run in Terminal buttons of the Executors
//! screen (UI brief, Sign in). Interactive steps and logins always go to a terminal; Reno never
//! types a password.
//!
//! [`TerminalOpener`] is injected (ARCH-16). The real one writes a `.command` file and asks macOS
//! to open it in Terminal. Tests install a recording fake, and with none installed a test that
//! reaches [`open`] panics, so a test can never reach `/usr/bin/open` by accident.

use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{App, Global};

/// Opens a command in a terminal window for the person to run.
pub(crate) trait TerminalOpener {
    /// `label` names what the command is for ("Sign in to Codex"); `command` is run as is by the
    /// person's login shell.
    fn open(&self, label: &str, command: &str) -> Result<(), String>;
}

/// The contents of the `.command` file: the command, then a note, so the window does not just
/// close on the result.
pub(crate) fn command_file_contents(label: &str, command: &str) -> String {
    format!(
        "#!/bin/zsh -l\n# {label}\nclear\n{command}\nstatus=$?\necho\necho \"Done. Reno checks again when you are back in the app; you can close this window.\"\nexit $status\n"
    )
}

/// A file name part made of the label's letters and digits only.
fn slug(label: &str) -> String {
    let slug: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    slug.trim_matches('-').to_owned()
}

/// Writes the `.command` file for `command` into `dir`. Terminal only runs a file that is
/// executable, so it is created `0700`: the person's own, and nobody else's.
pub(crate) fn write_command_file(dir: &Path, label: &str, command: &str) -> io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        "reno-{}-{}-{}.command",
        slug(label),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let path = dir.join(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o700);
    }
    let mut file = options.open(&path)?;
    io::Write::write_all(&mut file, command_file_contents(label, command).as_bytes())?;
    Ok(path)
}

/// The real opener: `open -a Terminal <file>`.
#[derive(Clone, Copy, Debug, Default)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) struct MacTerminal;

impl TerminalOpener for MacTerminal {
    fn open(&self, label: &str, command: &str) -> Result<(), String> {
        let path = write_command_file(&std::env::temp_dir(), label, command)
            .map_err(|e| format!("could not write the command file: {e}"))?;
        let status = std::process::Command::new("/usr/bin/open")
            .args(["-a", "Terminal"])
            .arg(&path)
            .status()
            .map_err(|e| format!("could not open Terminal: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("Terminal did not open ({status})"))
        }
    }
}

struct Handle(Rc<dyn TerminalOpener>);

impl Global for Handle {}

/// Installs the opener the screens use. The app installs none: it uses [`MacTerminal`].
#[cfg(test)]
pub(crate) fn install(cx: &mut App, opener: Rc<dyn TerminalOpener>) {
    cx.set_global(Handle(opener));
}

/// Opens `command` in Terminal for the person. In a test with no opener installed this panics.
pub(crate) fn open(cx: &App, label: &str, command: &str) -> Result<(), String> {
    match cx.try_global::<Handle>() {
        Some(handle) => handle.0.open(label, command),
        #[cfg(test)]
        None => panic!("a test reached the real Terminal opener: install a fake with `install`"),
        #[cfg(not(test))]
        None => MacTerminal.open(label, command),
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;

    use super::*;

    /// Records what would have been opened.
    #[derive(Default)]
    pub(crate) struct Recording {
        pub(crate) opened: RefCell<Vec<(String, String)>>,
    }

    impl TerminalOpener for Recording {
        fn open(&self, label: &str, command: &str) -> Result<(), String> {
            self.opened
                .borrow_mut()
                .push((label.to_owned(), command.to_owned()));
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_command_file_runs_the_command_and_is_the_owners_alone() {
        let dir = std::env::temp_dir().join(format!("reno-terminal-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_command_file(&dir, "Sign in to Codex", "codex login").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("#!/bin/zsh -l\n"));
        assert!(text.contains("\ncodex login\n"));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("command"));
        assert!(
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap()
                .starts_with("reno-sign-in-to-codex-")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "Terminal needs it executable; nobody else");
        }
        // Two files for the same command never collide.
        let again = write_command_file(&dir, "Sign in to Codex", "codex login").unwrap();
        assert_ne!(path, again);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
