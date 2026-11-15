//! The executors the daemon registers (the `Executor` port, ARCH-7).
//!
//! - `mock` is always there: a deterministic script for the CLI, the app and the tests. By
//!   default it plays a short canned run and finishes. With `hold` it stops after its first text
//!   and waits to be steered or cancelled, which gives tests and demos a run that stays live
//!   without any timer.
//! - `claude-code` is the native [`ClaudeExecutor`] and `codex` the native [`CodexExecutor`]; both
//!   are registered when their CLIs are found. Nothing here starts them.
//! - The agents built on ACP are native [`AcpExecutor`]s, one per agent, each under its own id:
//!   `acp.grok`, `acp.devin`, `acp.hermes`, `acp.fx`, `acp.antigravity`, and `acp.<registry id>`
//!   for each agent added from the ACP registry. They are registered when found, like the
//!   others. A registry agent never replaces an executor that is already registered: the
//!   registry lists agents Reno drives itself (`devin`, ...), and its record must not take over
//!   their id.

use std::sync::Arc;

use reno_harness::acp_registry::Record;
use reno_harness::exec::{Executor, MockExecutor, MockStep};
use reno_harness::upstream_proto::RegistryAgent;
use reno_harness::{AcpExecutor, ClaudeExecutor, CodexExecutor};
use reno_kernel::ExecutorRegistry;
use reno_proto::{ActionClass, ExecEvent, ExecutorId, IdError, StopReason, ToolCall};
use serde_json::json;

/// The id of the always-present mock executor.
pub const MOCK_ID: &str = "mock";
/// The id of the Claude Code executor.
pub const CLAUDE_ID: &str = reno_harness::claude::EXECUTOR_ID;
/// The id of the Codex executor.
pub const CODEX_ID: &str = reno_harness::codex::EXECUTOR_ID;

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
    let mut candidates: Vec<Arc<dyn Executor>> = vec![
        Arc::new(ClaudeExecutor::new()),
        Arc::new(CodexExecutor::new()),
        Arc::new(AcpExecutor::grok()),
        Arc::new(AcpExecutor::devin()),
        Arc::new(AcpExecutor::hermes()),
        Arc::new(AcpExecutor::fx()),
        Arc::new(AcpExecutor::antigravity()),
    ];
    let taken: Vec<ExecutorId> = candidates
        .iter()
        .map(|executor| executor.descriptor().id)
        .chain([MOCK_ID.parse()?])
        .collect();
    candidates.extend(
        registry_executors(reno_harness::acp_registry::records(), &taken)
            .into_iter()
            .map(|executor| Arc::new(executor) as Arc<dyn Executor>),
    );
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

/// One executor for each registry agent this device has added, except those whose id is already
/// `taken` (the executors Reno drives itself, `mock`): a registry record must never replace an
/// executor that is registered under the same id.
fn registry_executors(records: Vec<Record>, taken: &[ExecutorId]) -> Vec<AcpExecutor> {
    records
        .into_iter()
        .filter_map(|record| {
            // The registry also lists agents Reno drives itself (`claude-acp`, `codex-acp`,
            // `devin`, ...): they are never added from it, so a record for one is not an agent
            // of its own.
            if reno_harness::acp_registry::built_in(&record.id).is_some() {
                tracing::warn!(agent = %record.id, "the registry id of an executor Reno drives itself; skipped");
                return None;
            }
            let Some(agent) = RegistryAgent::new(&record.id) else {
                tracing::warn!(agent = %record.id, "not a registry agent id; skipped");
                return None;
            };
            let executor = match AcpExecutor::registry(agent, &record.name) {
                Ok(executor) => executor,
                Err(error) => {
                    tracing::warn!(agent = %record.id, %error, "no executor id for the agent; skipped");
                    return None;
                }
            };
            let id = executor.descriptor().id;
            if taken.contains(&id) {
                tracing::warn!(agent = %record.id, executor = %id, "an executor already has this id; the registry agent is skipped");
                return None;
            }
            Some(executor)
        })
        .collect()
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

    fn record(id: &str) -> Record {
        serde_json::from_value(json!({
            "id": id, "name": id, "version": "1.0.0", "kind": "binary", "program": "/bin/true",
        }))
        .expect("a record")
    }

    #[test]
    fn built_in_acp_ids_are_valid_and_distinct() {
        let ids: Vec<ExecutorId> = [
            AcpExecutor::grok(),
            AcpExecutor::devin(),
            AcpExecutor::hermes(),
            AcpExecutor::fx(),
            AcpExecutor::antigravity(),
        ]
        .iter()
        .map(|executor| executor.descriptor().id)
        .collect();
        let names: Vec<&str> = ids.iter().map(ExecutorId::as_str).collect();
        assert_eq!(
            names,
            [
                "acp.grok",
                "acp.devin",
                "acp.hermes",
                "acp.fx",
                "acp.antigravity"
            ]
        );
    }

    #[test]
    fn a_registry_agent_never_replaces_an_executor_with_its_id() {
        let taken: Vec<ExecutorId> = ["mock", "claude-code", "acp.grok", "acp.devin"]
            .iter()
            .map(|id| id.parse().expect("id"))
            .collect();
        let executors = registry_executors(
            vec![
                record("grok"),
                record("devin"),
                record("gemini"),
                record("own.mine"),
            ],
            &taken,
        );
        let ids: Vec<String> = executors
            .iter()
            .map(|executor| executor.descriptor().id.to_string())
            .collect();
        assert_eq!(ids, ["acp.gemini", "acp.own.mine"]);
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
