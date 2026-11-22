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
//! | `--no-approve` | a Reno attempt: the working folder's `.pi/` resources are ignored |
//! | `--session-dir <dir>` | a Reno attempt: the run's own session directory, the same for every attempt |
//! | `-e builtin:mcp -e builtin:codemode --tools +codemode` | a Reno attempt: Pi's own MCP and codemode, named, and codemode switched on (Pi registers it inactive) |
//! | `--extension <gate>` | a Reno attempt: the gate, always the last one |
//! | `--session-id <id>` | resume: opens the session of the working directory with that id, or starts one with it (Pi prints a warning to stderr) |
//! | `--append-system-prompt <file>` | the bot's standing instructions (Pi reads the file) |
//!
//! Not set: `--no-session` (a run's transcript is what a later resume opens), `--approve` (project
//! trust is the owner's decision, and RPC mode cannot ask: untrusted project resources are
//! skipped), `--no-context-files` (a project's `AGENTS.md` is part of the task). `--no-approve` is
//! set only for a Reno attempt.

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
    /// Where Pi keeps the run's session, instead of its default under the agent directory.
    pub(crate) session_dir: Option<&'a Path>,
    /// Load Pi's own MCP and codemode extensions, by name, and give the model codemode. Also
    /// stops passing `--no-mcp`: the agent directory holds no `mcp.json`, so no server loads.
    pub(crate) builtins: bool,
    /// Ignore the working folder's `.pi/` resources.
    pub(crate) no_approve: bool,
    /// The gate extension, loaded after every other one.
    pub(crate) gate: Option<&'a Path>,
}

/// The arguments for `launch`, after the program.
pub(crate) fn args(launch: Launch<'_>) -> Vec<String> {
    let mut args: Vec<String> = ["--mode", "rpc", "--no-themes"].map(String::from).into();
    if launch.isolate {
        args.extend(["--no-extensions", "--no-skills", "--no-prompt-templates"].map(String::from));
        if !launch.builtins {
            args.push("--no-mcp".into());
        }
    }
    if launch.no_approve {
        args.push("--no-approve".into());
    }
    if let Some(dir) = launch.session_dir {
        args.extend(["--session-dir".into(), dir.display().to_string()]);
    }
    if let Some(id) = launch.resume {
        args.extend(["--session-id".into(), id.into()]);
    }
    if let Some(file) = launch.instructions {
        args.extend(["--append-system-prompt".into(), file.display().to_string()]);
    }
    if launch.builtins {
        args.extend(
            [
                "-e",
                "builtin:mcp",
                "-e",
                "builtin:codemode",
                "--tools",
                "+codemode",
            ]
            .map(String::from),
        );
    }
    if let Some(file) = launch.bridge {
        args.extend(["--extension".into(), file.display().to_string()]);
    }
    if let Some(file) = launch.gate {
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
            ..Launch::default()
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
    fn a_reno_attempt_names_every_extension_and_loads_the_gate_last() {
        let instructions = PathBuf::from("/s/.reno/instructions.md");
        let bridge = PathBuf::from("/s/.reno/reno-mcp.mjs");
        let gate = PathBuf::from("/s/.reno/gate.mjs");
        let sessions = PathBuf::from("/data/.reno/pi-sessions/run_1");
        let all = args(Launch {
            resume: Some("abc-1"),
            isolate: true,
            instructions: Some(&instructions),
            bridge: Some(&bridge),
            session_dir: Some(&sessions),
            builtins: true,
            no_approve: true,
            gate: Some(&gate),
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
                "--no-approve",
                "--session-dir",
                "/data/.reno/pi-sessions/run_1",
                "--session-id",
                "abc-1",
                "--append-system-prompt",
                "/s/.reno/instructions.md",
                "-e",
                "builtin:mcp",
                "-e",
                "builtin:codemode",
                "--tools",
                "+codemode",
                "--extension",
                "/s/.reno/reno-mcp.mjs",
                "--extension",
                "/s/.reno/gate.mjs"
            ]
        );
        assert!(!all.iter().any(|f| f == "--no-mcp"));
    }

    #[test]
    fn a_reno_attempt_without_a_bridge_still_ends_with_the_gate() {
        let gate = PathBuf::from("/s/.reno/gate.mjs");
        let all = args(Launch {
            isolate: true,
            builtins: true,
            no_approve: true,
            gate: Some(&gate),
            ..Launch::default()
        });
        let extensions: Vec<_> = all
            .windows(2)
            .filter(|pair| pair[0] == "--extension")
            .map(|pair| pair[1].as_str())
            .collect();
        assert_eq!(extensions, ["/s/.reno/gate.mjs"]);
        assert_eq!(all.last().map(String::as_str), Some("/s/.reno/gate.mjs"));
    }

    #[test]
    fn the_owners_command_line_has_no_session_dir_no_builtins_and_no_gate() {
        let all = args(Launch {
            isolate: true,
            ..Launch::default()
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
                "--no-mcp"
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
            ..Launch::default()
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
