//! The command line of one `pi --mode rpc` child, as data.
//!
//! Everything a run asks of Pi at launch is a flag or an environment variable, never a write to
//! Pi's own configuration (ARCHITECTURE section 5; the owner's rule that Reno does not touch a
//! harness's settings, credentials or Keychain entries). The files a flag names live in a scratch
//! directory that belongs to the run.
//!
//! | Flag | Why |
//! |---|---|
//! | `--mode rpc --no-themes` | the RPC protocol, without loading a theme nobody sees |
//! | `--no-extensions --no-skills --no-prompt-templates --no-mcp` | isolation from the owner's personal tools: their extensions, skills, prompt templates and `mcp.json` servers (unless `inherit_user_config`). Checked on Pi 1.5.0 with `get_commands`: 0 commands with the flags, the owner's whole skill list without. |
//! | `--extension <file>` | Reno's MCP bridge; still loaded under `--no-extensions`, which only stops discovery |
//! | `--session-id <id>` | resume: opens the session of the working directory with that id, or starts one with it (Pi prints a warning to stderr) |
//! | `--append-system-prompt <file>` | the bot's standing instructions (Pi reads the file) |
//!
//! Not set: `--no-session` (a run's transcript is what a later resume opens), `--approve` (project
//! trust is the owner's decision, and RPC mode cannot ask: untrusted project resources are
//! skipped), `--no-context-files` (a project's `AGENTS.md` is part of the task).

use std::path::Path;

/// What a run asks of Pi at launch.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Launch<'a> {
    /// The session to open, when resuming.
    pub(crate) resume: Option<&'a str>,
    /// Leave the owner's extensions, skills, prompt templates and MCP servers out.
    pub(crate) isolate: bool,
    /// A file holding the instructions to append to the system prompt.
    pub(crate) instructions: Option<&'a Path>,
    /// The MCP bridge extension.
    pub(crate) bridge: Option<&'a Path>,
}

/// The arguments for `launch`, after the program.
pub(crate) fn args(launch: Launch<'_>) -> Vec<String> {
    let mut args: Vec<String> = ["--mode", "rpc", "--no-themes"].map(String::from).into();
    if launch.isolate {
        args.extend(
            [
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-mcp",
            ]
            .map(String::from),
        );
    }
    if let Some(id) = launch.resume {
        args.extend(["--session-id".into(), id.into()]);
    }
    if let Some(file) = launch.instructions {
        args.extend(["--append-system-prompt".into(), file.display().to_string()]);
    }
    if let Some(file) = launch.bridge {
        args.extend(["--extension".into(), file.display().to_string()]);
    }
    args
}

/// Whether `id` is a session id Pi accepts for `--session-id`: alphanumeric characters, `-`, `_`
/// and `.`, starting and ending with an alphanumeric one (`assertValidSessionId` in Pi's
/// `session-manager.js`).
pub(crate) fn valid_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    let edge = |b: &u8| b.is_ascii_alphanumeric();
    !bytes.is_empty()
        && bytes.first().is_some_and(edge)
        && bytes.last().is_some_and(edge)
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn a_bare_run_is_the_rpc_flags_and_nothing_about_settings() {
        assert_eq!(args(Launch::default()), ["--mode", "rpc", "--no-themes"]);
    }

    #[test]
    fn isolation_resume_instructions_and_the_bridge_each_add_their_flags() {
        let instructions = PathBuf::from("/scratch/instructions.md");
        let bridge = PathBuf::from("/scratch/reno-mcp.mjs");
        let all = args(Launch {
            resume: Some("abc-1"),
            isolate: true,
            instructions: Some(&instructions),
            bridge: Some(&bridge),
        });
        assert_eq!(
            all,
            [
                "--mode",
                "rpc",
                "--no-themes",
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-mcp",
                "--session-id",
                "abc-1",
                "--append-system-prompt",
                "/scratch/instructions.md",
                "--extension",
                "/scratch/reno-mcp.mjs"
            ]
        );
    }

    #[test]
    fn no_flag_ever_trusts_a_project_or_hides_one_from_pi() {
        let flags = args(Launch {
            resume: Some("x"),
            isolate: true,
            instructions: Some(Path::new("/i")),
            bridge: Some(Path::new("/b")),
        });
        for refused in [
            "--approve",
            "-a",
            "--no-approve",
            "--no-session",
            "--no-context-files",
        ] {
            assert!(!flags.iter().any(|f| f == refused), "{refused}");
        }
    }

    #[test]
    fn session_ids_follow_pis_rule() {
        for ok in ["a", "01a1ee3c-f4ce-72b1-b5fe-d4ec98e14122", "th.conf_1"] {
            assert!(valid_session_id(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", "--help", "a b", "a/b", "../x", "é"] {
            assert!(!valid_session_id(bad), "{bad}");
        }
    }
}
