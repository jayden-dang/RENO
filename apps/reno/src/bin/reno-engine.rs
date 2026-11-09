//! `reno-engine` — the desktop app's background engine, as a process of its
//! own ("Reno Engine" in Activity Monitor): the app starts it and attaches;
//! sessions keep running with the app closed. Without the UI linked in.
//!
//! It also answers what the engine runs its own executable for: `mcp` (the
//! Reno MCP server injected into agents) and `--noop-browser` (browser
//! suppression for agents' sign-in flows). And it is the engine helper of an
//! interface that doesn't link the engine (the zgx app): every `reno`
//! subcommand that runs the engine's code (`headless`, `login`, `status`,
//! `plugin`, …) is handed here with the same arguments, and parsed by the
//! same command line (`reno_cli`).

#![cfg_attr(windows, windows_subsystem = "windows")]

use reno_cli::{Engines, Launch};

fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--noop-browser") => Ok(()),
        Some("--version" | "-V") => {
            println!("reno-engine {}", reno_update::current_version());
            Ok(())
        }
        // Which build this binary is (`EngineInfo.build` of an engine it
        // starts): a client compares it with the one running.
        Some("--build-id") => {
            println!("{}", reno_engine::build_id());
            Ok(())
        }
        // How the app starts it: the background engine.
        None => reno_app::background::run(),
        Some(_) => match reno_cli::run(reno_update::current_version(), Engines::InProcess) {
            Launch::Done(result) => result,
            Launch::Engine(command) => reno_app::run_engine_command(command),
            Launch::Window { .. } => {
                anyhow::bail!("reno-engine has no window: open the Reno app")
            }
            #[cfg(target_os = "linux")]
            Launch::Appshot => anyhow::bail!("reno-engine has no window: open the Reno app"),
        },
    }
}
