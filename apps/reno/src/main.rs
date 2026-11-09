//! reno — headed by default; `reno headless` runs the engine alone. Both start
//! local-only without credentials. `reno login` and `reno logout` select the
//! profile used by the next engine start without mutating a live runtime.
//!
//! The command line is `reno_cli`'s (shared with the engine helper and the
//! zgx interface). This binary links the engine, so it runs the engine's
//! subcommands itself; with no subcommand it opens the window.

#![cfg_attr(windows, windows_subsystem = "windows")]

use reno_app::{edge_url_from_env, paths, workos_client_id_from_env};
use reno_cli::{Engines, Launch};

fn main() -> anyhow::Result<()> {
    match reno_cli::run(reno_update::current_version(), Engines::InProcess) {
        Launch::Done(result) => result,
        Launch::Engine(command) => reno_app::run_engine_command(command),
        #[cfg(target_os = "linux")]
        Launch::Appshot => {
            reno_ui::appshots::request_running_appshot(&reno_app::engine_config_from_env().data_dir)
                .map_err(anyhow::Error::msg)
        }
        Launch::Window { open_url } => {
            let edge_token = std::env::var("RENO_EDGE_TOKEN").ok();
            // Headed: the UI probes RENO_IPC_PORT and connects to a running
            // daemon, or embeds the engine in-process (ARCHITECTURE §1).
            reno_ui::run_app(reno_ui::UiConfig {
                data_dir: paths::data_dir(),
                ipc_port: reno_cli::ipc_port_from_env(),
                edge_url: edge_url_from_env(),
                workos_client_id: workos_client_id_from_env(&edge_token),
                edge_token,
                org_id: std::env::var("RENO_ORG_ID").ok(),
                default_harness: reno_ui::HarnessId::ClaudeCode,
                initial_url: open_url,
            });
            Ok(())
        }
    }
}
