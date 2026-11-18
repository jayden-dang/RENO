//! The brain of the daemon: dispatcher, leases, run supervisors, recovery and the façade over
//! them. Routing, triggers, flows, boards, policy and evaluation join it in later phases.
//!
//! Owns ARCH-7: the kernel does no I/O of its own. It reaches the world only through `reno-store`
//! and its ports (`Clock`, `Executor`, `Workspace`, `Watch`, `WakeSignal`, `Definitions`, `Secrets`, `RunLink`), which are
//! implemented outside it and faked in tests. `cargo xtask check` scans these sources for file,
//! process and socket use and for clock reads. Owns ARCH-10 and ARCH-11: nothing polls, every
//! queue is bounded and every task is owned.
//!
//! # Modules
//!
//! - `clock`: the `Clock` port, `SystemClock` (the one reader of the system clock), `ManualClock`.
//! - `registry`: executors by id.
//! - `link`, `agent`: the `RunLink` port (a run's token and the MCP server that carries it) and what a
//!   run may do with its token: `report`, `request_approval` and its own board (T1.9).
//! - `kernel`: [`Kernel`], the façade, and the layout of tasks and channels.
//! - `defs`: the `Definitions` port and the validated set in force; loading, storing versions
//!   and publishing, with the last good set kept when files do not validate.
//! - `dispatcher`: claims runs and starts them.
//! - `budget`: the kill switch, `budget.status`, and the refusal of a bot out of tokens; what each
//!   limit of a bot's `budget` does and where it is enforced.
//! - `router`: the pure router: filter, rules, the decision, and the fallback step.
//! - `health`: what the last probe found about each executor, when probes run, and how the
//!   findings keep an ineligible executor from being started.
//! - `scheduler`, `schedule`: the trigger scheduler actor (timers are deadlines, definitions
//!   changes re-plan, firings are deduplicated by nonce) and its pure arithmetic.
//! - `watch`: the `Watch` port `fs` triggers use.
//! - `wake`: the wake port: the daemon tells the scheduler that the system woke.
//! - `workspace`, `workspaces`: the `Workspace` port (git worktrees for write runs), its fake, and
//!   what the kernel does with it: placement, release at a run's end, clean, start-up sweep.
//! - `secrets`: the `Secrets` port (Keychain-backed named secrets), its fake, and `SecretValue`.
//!   The dispatcher resolves a bot's `secrets` into the run's environment and refuses a run whose
//!   secret is not set; the supervisor redacts the values from everything the run produces.
//! - `supervisor`: one task per live run; owns its stream, journal, subscribers and heartbeat.
//! - `events`: `run_events`, journal replay joined to the live tail.
//! - `recovery`: what start-up repairs.
//! - `lifecycle`: how a `Done` becomes state.
//! - `approvals`: the pending list that `approval.list` and `approvals.watch` show, read from the
//!   store. What a supervisor does with a request is in `supervisor/approval.rs`.
//! - `procs`, `db`, `shared`, `ids`, `config`, `error`: support.
//!
//! # Approvals and launch settings (T1.6)
//!
//! - A `NeedsApproval` is judged by the policy in force (`reno_proto::defs::evaluate`): a rule
//!   answers at once, a repeated denial is refused unasked, anything else is stored as a pending
//!   approval with a deadline and held for a human. Every outcome is a durable record and an
//!   `ExecEvent::ApprovalResolved` in the journal. See `supervisor/approval.rs`.
//! - Recovery and shutdown expire the pending approvals no supervisor is left to answer.
//! - A run for a bot launches with its pinned bot version's `access` and `sandbox`; an executor
//!   that does not declare that access mode is not eligible, and the run fails with
//!   `NoEligibleExecutor`. See `dispatcher.rs`.
//!
//! # Limits
//!
//! - A rule's deny message is stored in the journal's `ApprovalResolved` and handed to the
//!   executor with the refusal (`ExecHandle::decide_with_message`). What reaches the agent is
//!   the driver's: Claude and ACP carry it, Codex's approval reply has no field for it.
//! - A supervisor that panics leaves its run's approvals pending until the next start's recovery.
//! - A run in `waiting_input` has no client path to an answer.
//! - A run holds one process record; a second report replaces the first.
//! - `RunSummary::stop_reason` is derived from the status (the store keeps no stop reason).
//!   `RunSummary::model` is the model the task asked for (no event updates the row's model, so
//!   the one an executor reports in `Started` is not shown), and `prompt` is the task's goal.

mod agent;
mod approvals;
mod budget;
mod clock;
mod config;
mod db;
mod defs;
mod dispatcher;
mod effort;
mod error;
mod events;
mod health;
mod ids;
mod kernel;
mod lifecycle;
mod link;
mod maintain;
mod procs;
mod recovery;
mod registry;
mod resume;
mod router;
mod schedule;
mod scheduler;
mod secrets;
mod shared;
mod supervisor;
mod wake;
mod watch;
mod workspace;
mod workspaces;

pub use agent::AgentSession;
pub use clock::{Clock, ManualClock, SystemClock};
pub use config::KernelConfig;
pub use defs::{CurrentDefs, DefSnapshot, Definitions, DefsSetup, StoredVersion};
pub use error::{KernelError, Result};
pub use kernel::{Kernel, ShutdownReport, TaskRequest};
pub use maintain::{CommandRunner, Limits, MaintenancePorts, SelfUpdateControl};
pub use link::{FakeRunLink, IssuedLink, LinkError, RunLink};
pub use registry::ExecutorRegistry;
pub use router::{
    BotPreference, Candidate, RouteRequest, fall_back, ineligibilities, ineligibility, route,
};
pub use secrets::{FakeSecrets, SecretValue, Secrets, SecretsError, check_name, check_secret};
pub use wake::WakeSignal;
pub use watch::{Watch, WatchBackoff, WatchSpec, WatchStream};
pub use workspace::{
    FAKE_WORKTREES, FakeWorkspace, InPlace, Inspection, Placement, ReleasePolicy, Released,
    Workspace, WorkspaceError,
};

#[cfg(test)]
mod tests;
