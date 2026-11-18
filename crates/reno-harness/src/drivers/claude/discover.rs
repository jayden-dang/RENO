//! The live model list: the CLI's answer to the `initialize` control request.
//!
//! One short-lived CLI process, the same command line as a run (so the same isolation) but no
//! prompt, so no model call. It is the probe's, never a run's: it runs off the daemon's boot
//! path, inside a time budget, and is reaped before the call returns whatever happened.

use std::path::Path;
use std::time::Duration;

use reno_proto::{ExecSpec, RunId, SandboxLevel};
use tokio::io::AsyncWriteExt;

use super::ClaudeExecutor;
use crate::process::owned::Child;
use crate::wire::claude::models::{self, LiveModels};
use crate::wire::claude::wire::{self, Frame};

/// The id of the handshake's one request.
const REQUEST_ID: &str = "reno_probe_models";

/// What the CLI at `exe` lists, or `None` when it could not be asked, did not answer within
/// `budget`, or listed nothing usable.
pub(super) async fn live_models(
    exe: &Path,
    budget: Duration,
    kill_grace: Duration,
) -> Option<LiveModels> {
    let spec = ExecSpec {
        model_options: Default::default(),
        run_id: RunId::from_suffix("probe").ok()?,
        inherit_user_config: false,
        prompt: String::new(),
        // Not a project: nothing of one is loaded for a question about models.
        cwd: std::env::temp_dir(),
        model: None,
        effort: None,
        access: reno_proto::AccessMode::Supervised,
        sandbox: SandboxLevel::Unrestricted,
        resume: None,
        mcp: Vec::new(),
        env: Default::default(),
        output_schema: None,
        instructions: None,
    };
    let mut child = ClaudeExecutor::build_command(exe, &spec).spawn().ok()?;
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let mut child = Child::new(child);
    // The CLI's stderr is not read: it is dropped (closed) so the child cannot block on it.
    drop(stderr);
    let asked = async {
        let mut stdin = stdin?;
        let mut lines = crate::lossy_lines::lossy_lines(stdout?);
        let line = format!("{}\n", wire::initialize_request_line(REQUEST_ID));
        stdin.write_all(line.as_bytes()).await.ok()?;
        stdin.flush().await.ok()?;
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(Frame::ControlResponse(answer)) = wire::parse_frame(line.trim()) else {
                continue;
            };
            if answer.response.request_id != REQUEST_ID {
                continue;
            }
            return answer
                .response
                .response
                .as_ref()
                .and_then(models::parse_initialize);
        }
        None
    };
    let found = tokio::time::timeout(budget, asked).await.ok().flatten();
    child.shutdown(kill_grace).await;
    found
}
