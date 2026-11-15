//! The executors the daemon registers (the `Executor` port, ARCH-7).
//!
//! - `mock` is always there: a deterministic script for the CLI, the app and the tests. By
//!   default it plays a short canned run and finishes. With `hold` it stops after its first text
//!   and waits to be steered or cancelled, which gives tests and demos a run that stays live
//!   without any timer.
//! - `claude-code` is the native [`ClaudeExecutor`] and `codex` still runs through the stage A
//!   [`HarnessExecutor`] shim (until its driver goes native, T1.0b); both are registered when
//!   their CLIs are found. Nothing here starts them.

use std::sync::Arc;

use reno_harness::exec::{Executor, HarnessExecutor, MockExecutor, MockStep};
use reno_harness::{ClaudeExecutor, CodexHarness};
use reno_kernel::ExecutorRegistry;
use reno_proto::{ActionClass, ExecEvent, IdError, StopReason, ToolCall};
use serde_json::json;

/// The id of the always-present mock executor.
pub const MOCK_ID: &str = "mock";
/// The id of the Claude Code executor.
pub const CLAUDE_ID: &str = reno_harness::claude::EXECUTOR_ID;
/// The id of the Codex executor.
pub const CODEX_ID: &str = "codex";

/// The model the mock reports.
pub const MOCK_MODEL: &str = "mock-1";

/// The registry the kernel gets, and the harness executors the daemon must wait for at shutdown.
#[derive(Default)]
pub struct ExecutorSet {
    /// Executors by id.
    pub registry: ExecutorRegistry,
    /// The harness executors in the registry, so shutdown can wait for their tasks.
    pub harnesses: Vec<Arc<dyn Executor>>,
}

impl std::fmt::Debug for ExecutorSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutorSet")
            .field("registry", &self.registry)
            .field("harnesses", &self.harnesses.len())
            .finish()
    }
}

impl ExecutorSet {
    /// A set holding only `registry`, for tests that bring their own executors.
    pub fn from_registry(registry: ExecutorRegistry) -> Self {
        Self {
            registry,
            harnesses: Vec::new(),
        }
    }
}

/// The mock executor: the canned script, or the hold script when `hold` is set.
pub fn mock_executor(hold: bool) -> Result<MockExecutor, IdError> {
    Ok(MockExecutor::new(MOCK_ID.parse()?).with_steps(mock_script(hold)))
}

/// The mock's script.
///
/// Canned: `Started`, three text deltas, a tool call and its result, a last text, `Done`.
/// Hold: `Started` and one text, then wait for a steering prompt (or a cancel, which ends the
/// run `interrupted`), then the same ending.
pub fn mock_script(hold: bool) -> Vec<MockStep> {
    let mut steps = vec![
        MockStep::started("mock-session", MOCK_MODEL),
        MockStep::text("Hello from the mock executor. "),
    ];
    if hold {
        steps.push(MockStep::AwaitSteer);
    }
    steps.extend([
        MockStep::text("I am reading the task "),
        MockStep::text("and pretending to work on it.\n"),
        MockStep::Emit(ExecEvent::Tool {
            id: "mock-call-1".into(),
            call: ToolCall::new("mock_tool", json!({ "note": "canned" }), ActionClass::Other),
        }),
        MockStep::Emit(ExecEvent::ToolResult {
            id: "mock-call-1".into(),
            ok: true,
            summary: Some("the mock tool finished".into()),
        }),
        MockStep::text("All done."),
        MockStep::Emit(ExecEvent::Done {
            stop: StopReason::Completed,
            result: Some("mock run complete".into()),
            diagnostic: None,
        }),
    ]);
    steps
}

/// Builds the registry: `mock`, plus each harness whose CLI is installed. With `harnesses` false
/// only `mock` is registered and no CLI is looked for, let alone started.
pub async fn discover(hold_mock: bool, harnesses: bool) -> Result<ExecutorSet, IdError> {
    let mut set = ExecutorSet {
        registry: ExecutorRegistry::new().with(Arc::new(mock_executor(hold_mock)?)),
        harnesses: Vec::new(),
    };
    if !harnesses {
        return Ok(set);
    }
    let candidates: [Arc<dyn Executor>; 2] = [
        Arc::new(ClaudeExecutor::new()),
        Arc::new(HarnessExecutor::new(CodexHarness::new(), CODEX_ID.parse()?)),
    ];
    for executor in candidates {
        let id = executor.descriptor().id;
        let health = executor.probe().await;
        if health.installed {
            tracing::info!(
                executor = %id,
                version = health.version.as_deref().unwrap_or("unknown"),
                "executor found"
            );
            set.registry.insert(id, executor.clone());
            set.harnesses.push(executor);
        } else {
            tracing::info!(
                executor = %id,
                detail = health.detail.as_deref().unwrap_or(""),
                "executor not installed"
            );
        }
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use reno_proto::ExecutorId;

    use super::*;

    #[test]
    fn built_in_ids_are_valid() {
        for name in [MOCK_ID, CLAUDE_ID, CODEX_ID] {
            assert_eq!(name.parse::<ExecutorId>().expect("valid").as_str(), name);
        }
    }

    #[tokio::test]
    async fn without_harnesses_only_the_mock_is_registered_and_nothing_is_probed() {
        let set = discover(false, false).await.expect("discover");
        let ids: Vec<_> = set.registry.ids().map(ExecutorId::as_str).collect();
        assert_eq!(ids, [MOCK_ID]);
        assert!(set.harnesses.is_empty());
    }

    #[test]
    fn both_scripts_end_with_exactly_one_done() {
        for hold in [false, true] {
            let script = mock_script(hold);
            let dones = script
                .iter()
                .filter(|s| matches!(s, MockStep::Emit(ExecEvent::Done { .. })))
                .count();
            assert_eq!(dones, 1);
            assert!(matches!(
                script.last(),
                Some(MockStep::Emit(ExecEvent::Done { .. }))
            ));
            assert_eq!(
                script.iter().any(|s| matches!(s, MockStep::AwaitSteer)),
                hold
            );
        }
    }
}
