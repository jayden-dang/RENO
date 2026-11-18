//! What a terminal's shell starts with: its program and its environment.
//!
//! The shell is the person's own: a login shell from `$SHELL`, so their profile and `PATH` apply.
//! Its environment is the daemon's minus everything of Reno's own: `RENO_*` (a run's link and
//! token variables are `RENO_*`, and nothing of the daemon's own configuration is a terminal's
//! business). A secret a bot lists is put into that run's agent process only, never the daemon's
//! environment; this is the second line of defense if one ever were.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::pty::is_executable;

/// The terminal type the shell is told.
pub(super) const TERM_VALUE: &str = "xterm-256color";

/// Variables the daemon sets itself, so whatever it inherited for them is dropped.
const OWN: [&str; 4] = ["TERM", "COLORTERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION"];

/// The environment of a shell: `base` (the daemon's) without `RENO_*` and the terminal's own
/// variables, then `TERM`, `COLORTERM` and `TERM_PROGRAM` set. Order is kept.
pub(super) fn child_env(
    base: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = base
        .into_iter()
        .filter(|(name, _)| !is_reno(name) && !OWN.iter().any(|own| name == OsStr::new(own)))
        .collect();
    env.push(("TERM".into(), TERM_VALUE.into()));
    env.push(("COLORTERM".into(), "truecolor".into()));
    env.push(("TERM_PROGRAM".into(), "Reno".into()));
    env
}

fn is_reno(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| name.starts_with("RENO_"))
}

/// The shell to start: `$SHELL` in `base` when it is an executable file, else `/bin/zsh`, else
/// `/bin/sh`.
pub(super) fn select_shell(base: &[(OsString, OsString)]) -> PathBuf {
    let chosen = base
        .iter()
        .find(|(name, _)| name == OsStr::new("SHELL"))
        .map(|(_, value)| PathBuf::from(value))
        .filter(|path| path.is_absolute() && is_executable(path));
    chosen.unwrap_or_else(|| {
        ["/bin/zsh", "/bin/sh"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| is_executable(path))
            .unwrap_or_else(|| PathBuf::from("/bin/sh"))
    })
}

/// The shell's name, for the tab's label.
pub(super) fn shell_name(shell: &Path) -> String {
    shell.file_name().map_or_else(
        || shell.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(name: &str, value: &str) -> (OsString, OsString) {
        (name.into(), value.into())
    }

    #[test]
    fn reno_variables_and_the_terminal_variables_do_not_pass() {
        let env = child_env([
            pair("HOME", "/Users/p"),
            pair("RENO_RUN_TOKEN", "rnt_secret"),
            pair("RENO_SOCKET", "/x"),
            pair("TERM", "dumb"),
            pair("TERM_PROGRAM", "Other"),
            pair("PATH", "/usr/bin"),
        ]);
        let names: Vec<&str> = env.iter().filter_map(|(n, _)| n.to_str()).collect();
        assert_eq!(names, ["HOME", "PATH", "TERM", "COLORTERM", "TERM_PROGRAM"]);
        let get = |name: &str| {
            env.iter()
                .find(|(n, _)| n == OsStr::new(name))
                .and_then(|(_, v)| v.to_str())
        };
        assert_eq!(get("TERM"), Some("xterm-256color"));
        assert_eq!(get("COLORTERM"), Some("truecolor"));
        assert_eq!(get("TERM_PROGRAM"), Some("Reno"));
        assert_eq!(get("RENO_RUN_TOKEN"), None);
    }

    #[test]
    fn the_shell_is_shell_when_it_runs_and_a_fallback_when_not() {
        let sh = select_shell(&[pair("SHELL", "/bin/sh")]);
        assert_eq!(sh, Path::new("/bin/sh"));
        for bad in ["/no/such/shell", "relative-shell", ""] {
            let chosen = select_shell(&[pair("SHELL", bad)]);
            assert!(chosen == Path::new("/bin/zsh") || chosen == Path::new("/bin/sh"));
        }
        assert_eq!(shell_name(Path::new("/bin/zsh")), "zsh");
    }
}
