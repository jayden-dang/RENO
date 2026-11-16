//! The LaunchAgent that keeps `renod` running: its plist, where it goes, and how to tell that an
//! installed one is ours.
//!
//! The settings, and why:
//!
//! - `RunAtLoad`: start at login, so schedules fire after a reboot with no app open.
//! - `KeepAlive` with `SuccessfulExit` false: launchd restarts a crash (a non-zero exit or a
//!   signal) but not a clean exit. `reno daemon stop`, `daemon.shutdown` and launchd's own
//!   SIGTERM all end in exit status 0, so a stopped daemon stays stopped until `reno daemon
//!   start` (which kickstarts it).
//! - `ThrottleInterval` 10: launchd's own minimum between starts, stated so it is visible.
//! - `ExitTimeOut` 30: after SIGTERM launchd waits this long before SIGKILL. The daemon's drain
//!   takes at most 15 s for the kernel plus 5 s for the server, so 30 s never cuts it short.
//! - `ProcessType` `Interactive`: with none, launchd throttles an agent's CPU and I/O, and a
//!   daemon that runs coding agents on the user's behalf is not background work.
//! - `StandardErrorPath`: `logs/renod.stderr`, the same file `reno daemon start` uses.
//!
//! No `--detach`: launchd is the parent and wants the process to stay in the job.

use std::path::{Path, PathBuf};

use super::LauncherError;

/// The agent's label, and the stem of its plist's file name.
pub const LABEL: &str = "dev.reno.renod";

/// The plist's file name.
pub fn file_name() -> String {
    format!("{LABEL}.plist")
}

/// What goes into the plist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchAgent {
    renod: PathBuf,
    data_dir: PathBuf,
    socket: PathBuf,
    stderr: PathBuf,
    defs_dir: Option<PathBuf>,
}

pub(super) fn text(path: &Path) -> Result<&str, LauncherError> {
    match path.to_str() {
        Some(text) if path.is_absolute() => Ok(text),
        _ => Err(LauncherError::Path(path.to_path_buf())),
    }
}

impl LaunchAgent {
    /// An agent that runs `renod` on `data_dir` and `socket`, with its stderr in `stderr`, and
    /// the definitions at `defs_dir` when that is not the default. Every path must be absolute
    /// and UTF-8: launchd does not expand `~` or a relative path.
    pub fn new(
        renod: PathBuf,
        data_dir: PathBuf,
        socket: PathBuf,
        stderr: PathBuf,
        defs_dir: Option<PathBuf>,
    ) -> Result<Self, LauncherError> {
        for path in [&renod, &data_dir, &socket, &stderr]
            .into_iter()
            .chain(defs_dir.as_ref())
        {
            text(path)?;
        }
        Ok(Self {
            renod,
            data_dir,
            socket,
            stderr,
            defs_dir,
        })
    }

    /// The data directory the agent serves.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The `renod` it runs.
    pub fn renod(&self) -> &Path {
        &self.renod
    }

    /// The property list, as XML.
    pub fn plist(&self) -> String {
        let s = |path: &Path| escape(path.to_str().unwrap_or_default());
        let mut out = String::new();
        out.push_str(concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
            "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
            "<plist version=\"1.0\">\n<dict>\n",
        ));
        out.push_str(&format!("\t<key>Label</key>\n\t<string>{LABEL}</string>\n"));
        out.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
        for argument in [
            self.renod.as_path(),
            Path::new("--data-dir"),
            &self.data_dir,
            Path::new("--socket"),
            &self.socket,
        ] {
            out.push_str(&format!("\t\t<string>{}</string>\n", s(argument)));
        }
        out.push_str("\t</array>\n");
        if let Some(defs) = &self.defs_dir {
            out.push_str(&format!(
                "\t<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>RENO_DEFS_DIR</key>\n\t\t<string>{}</string>\n\t</dict>\n",
                s(defs)
            ));
        }
        out.push_str(concat!(
            "\t<key>RunAtLoad</key>\n\t<true/>\n",
            "\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n",
            "\t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n",
            "\t<key>ExitTimeOut</key>\n\t<integer>30</integer>\n",
            "\t<key>ProcessType</key>\n\t<string>Interactive</string>\n",
        ));
        out.push_str(&format!(
            "\t<key>StandardOutPath</key>\n\t<string>/dev/null</string>\n\t<key>StandardErrorPath</key>\n\t<string>{}</string>\n",
            s(&self.stderr)
        ));
        out.push_str("</dict>\n</plist>\n");
        out
    }
}

pub(super) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The data directory a plist written by [`LaunchAgent::plist`] serves: the argument after
/// `--data-dir`. `None` for anything else, so a foreign file with our name is not ours.
pub fn data_dir_of(plist: &str) -> Option<PathBuf> {
    let after = plist.split("<string>--data-dir</string>").nth(1)?;
    let value = after.split("<string>").nth(1)?.split("</string>").next()?;
    Some(PathBuf::from(unescape(value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> LaunchAgent {
        LaunchAgent::new(
            "/opt/reno/bin/renod".into(),
            "/Users/u/Library/Application Support/Reno".into(),
            "/Users/u/Library/Application Support/Reno/reno.sock".into(),
            "/Users/u/Library/Application Support/Reno/logs/renod.stderr".into(),
            None,
        )
        .expect("agent")
    }

    #[test]
    fn the_plist_names_the_job_its_arguments_and_how_launchd_treats_it() {
        let plist = agent().plist();
        for expected in [
            "<string>dev.reno.renod</string>",
            "<string>/opt/reno/bin/renod</string>",
            "<string>--data-dir</string>\n\t\t<string>/Users/u/Library/Application Support/Reno</string>",
            "<key>RunAtLoad</key>\n\t<true/>",
            "<key>SuccessfulExit</key>\n\t\t<false/>",
            "<key>ThrottleInterval</key>\n\t<integer>10</integer>",
            "<key>ExitTimeOut</key>\n\t<integer>30</integer>",
            "<key>StandardErrorPath</key>\n\t<string>/Users/u/Library/Application Support/Reno/logs/renod.stderr</string>",
        ] {
            assert!(plist.contains(expected), "missing {expected:?} in\n{plist}");
        }
        assert!(!plist.contains("--detach"));
        assert!(!plist.contains("EnvironmentVariables"));
    }

    #[test]
    fn a_definitions_directory_goes_in_the_environment_and_markup_is_escaped() {
        let agent = LaunchAgent::new(
            "/bin/renod".into(),
            "/d/a&b <c>".into(),
            "/d/s.sock".into(),
            "/d/e".into(),
            Some("/home/u/.reno".into()),
        )
        .expect("agent");
        let plist = agent.plist();
        assert!(plist.contains("<key>RENO_DEFS_DIR</key>\n\t\t<string>/home/u/.reno</string>"));
        assert!(plist.contains("<string>/d/a&amp;b &lt;c&gt;</string>"));
        assert_eq!(data_dir_of(&plist), Some(PathBuf::from("/d/a&b <c>")));
    }

    #[test]
    fn a_relative_path_is_refused_and_a_foreign_plist_is_not_ours() {
        let relative = LaunchAgent::new(
            "renod".into(),
            "/d".into(),
            "/d/s".into(),
            "/d/e".into(),
            None,
        );
        assert!(matches!(relative, Err(LauncherError::Path(_))));
        assert_eq!(data_dir_of("<plist><dict></dict></plist>"), None);
        assert_eq!(
            data_dir_of(&agent().plist()),
            Some(agent().data_dir().to_path_buf())
        );
    }
}
