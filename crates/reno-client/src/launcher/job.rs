//! A LaunchAgent that runs one command once a day: the plist for the nightly live conformance
//! job (`cargo xtask conformance-live --print-plist`).
//!
//! Unlike the daemon's agent ([`super::agent`]) it has no `KeepAlive` and no `RunAtLoad`: launchd
//! starts it at the given hour and minute and lets it end. It is built from the same pieces
//! (absolute UTF-8 paths, escaped text) and, like that one, is only ever *printed* here: nothing
//! in this module writes to `~/Library/LaunchAgents` or calls `launchctl`.
//!
//! The settings, and why:
//!
//! - `StartCalendarInterval`: one start a day. If the Mac sleeps through it, launchd runs the
//!   job once at wake, which is what a nightly report wants.
//! - `ProcessType` `Background`: it is a batch job and may be throttled.
//! - `WorkingDirectory`: the repository checkout, where `cargo xtask` resolves.
//! - `EnvironmentVariables`: launchd's environment is nearly empty, so the caller passes the
//!   `PATH` (where `cargo`, `claude` and `codex` live) and `HOME` (where their logins live).

use std::path::{Path, PathBuf};

use super::LauncherError;
use super::agent::{escape, text};

/// A job launchd runs at a fixed time of day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledJob {
    label: String,
    program: Vec<String>,
    working_dir: PathBuf,
    env: Vec<(String, String)>,
    hour: u8,
    minute: u8,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl ScheduledJob {
    /// A job named `label` that runs `program` (the program, then its arguments) in
    /// `working_dir` at `hour`:`minute` local time every day, with `env` and its output in
    /// `stdout` and `stderr`. Every path must be absolute and UTF-8, `hour` below 24 and
    /// `minute` below 60. `program[0]` must be an absolute path: launchd does not search `PATH`.
    pub fn new(
        label: impl Into<String>,
        program: Vec<String>,
        working_dir: PathBuf,
        env: Vec<(String, String)>,
        (hour, minute): (u8, u8),
        stdout: PathBuf,
        stderr: PathBuf,
    ) -> Result<Self, LauncherError> {
        let first = program.first().map(PathBuf::from).unwrap_or_default();
        for path in [&first, &working_dir, &stdout, &stderr] {
            text(path)?;
        }
        if hour > 23 || minute > 59 {
            return Err(LauncherError::Time { hour, minute });
        }
        Ok(Self {
            label: label.into(),
            program,
            working_dir,
            env,
            hour,
            minute,
            stdout,
            stderr,
        })
    }

    /// The job's label, and so the stem of its plist's file name.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The plist's file name.
    pub fn file_name(&self) -> String {
        format!("{}.plist", self.label)
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
        out.push_str(&format!(
            "\t<key>Label</key>\n\t<string>{}</string>\n",
            escape(&self.label)
        ));
        out.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
        for argument in &self.program {
            out.push_str(&format!("\t\t<string>{}</string>\n", escape(argument)));
        }
        out.push_str("\t</array>\n");
        out.push_str(&format!(
            "\t<key>WorkingDirectory</key>\n\t<string>{}</string>\n",
            s(&self.working_dir)
        ));
        if !self.env.is_empty() {
            out.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
            for (key, value) in &self.env {
                out.push_str(&format!(
                    "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
                    escape(key),
                    escape(value)
                ));
            }
            out.push_str("\t</dict>\n");
        }
        out.push_str(&format!(
            "\t<key>StartCalendarInterval</key>\n\t<dict>\n\t\t<key>Hour</key>\n\t\t<integer>{}</integer>\n\t\t<key>Minute</key>\n\t\t<integer>{}</integer>\n\t</dict>\n",
            self.hour, self.minute
        ));
        out.push_str("\t<key>ProcessType</key>\n\t<string>Background</string>\n");
        out.push_str(&format!(
            "\t<key>StandardOutPath</key>\n\t<string>{}</string>\n\t<key>StandardErrorPath</key>\n\t<string>{}</string>\n",
            s(&self.stdout),
            s(&self.stderr)
        ));
        out.push_str("</dict>\n</plist>\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> ScheduledJob {
        ScheduledJob::new(
            "dev.reno.conformance-live",
            vec![
                "/Users/u/.cargo/bin/cargo".into(),
                "xtask".into(),
                "conformance-live".into(),
            ],
            "/Users/u/src/reno".into(),
            vec![("PATH".into(), "/usr/bin:/opt/a&b".into())],
            (3, 17),
            "/Users/u/Library/Application Support/Reno/conformance/nightly.out".into(),
            "/Users/u/Library/Application Support/Reno/conformance/nightly.err".into(),
        )
        .expect("job")
    }

    #[test]
    fn the_plist_runs_the_command_once_a_day_and_never_keeps_it_alive() {
        let plist = job().plist();
        for expected in [
            "<string>dev.reno.conformance-live</string>",
            "<string>/Users/u/.cargo/bin/cargo</string>\n\t\t<string>xtask</string>\n\t\t<string>conformance-live</string>",
            "<key>WorkingDirectory</key>\n\t<string>/Users/u/src/reno</string>",
            "<key>PATH</key>\n\t\t<string>/usr/bin:/opt/a&amp;b</string>",
            "<key>Hour</key>\n\t\t<integer>3</integer>\n\t\t<key>Minute</key>\n\t\t<integer>17</integer>",
            "<key>ProcessType</key>\n\t<string>Background</string>",
            "<key>StandardErrorPath</key>",
        ] {
            assert!(plist.contains(expected), "missing {expected:?} in\n{plist}");
        }
        assert!(!plist.contains("KeepAlive"));
        assert!(!plist.contains("RunAtLoad"));
        assert_eq!(job().file_name(), "dev.reno.conformance-live.plist");
    }

    #[test]
    fn relative_paths_and_impossible_times_are_refused() {
        let relative = ScheduledJob::new(
            "l",
            vec!["cargo".into()],
            "/d".into(),
            Vec::new(),
            (3, 0),
            "/o".into(),
            "/e".into(),
        );
        assert!(matches!(relative, Err(LauncherError::Path(_))));
        let late = ScheduledJob::new(
            "l",
            vec!["/bin/x".into()],
            "/d".into(),
            Vec::new(),
            (24, 0),
            "/o".into(),
            "/e".into(),
        );
        assert!(matches!(late, Err(LauncherError::Time { .. })));
        let minute = ScheduledJob::new(
            "l",
            vec!["/bin/x".into()],
            "/d".into(),
            Vec::new(),
            (0, 60),
            "/o".into(),
            "/e".into(),
        );
        assert!(matches!(minute, Err(LauncherError::Time { .. })));
    }
}
