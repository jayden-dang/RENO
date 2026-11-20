//! First-run setup (ROAD-38): a full-window, centred wizard that opens on a first run (app-local
//! pref `onboarded`). It states what is on this Mac (which agents can take runs and what blocks
//! the others) and lets the person fix it, pick a look and bring in a first project, one thing
//! per screen. "Skip setup" ends it at any step. The rules are in `vm::first_run`.
//!
//! - `flow`: the setup entity: where a person is, and what each click does.
//! - `parts`: the agent's control, the look controls, the project rows.
//! - `wizard`: the five (or, without a way to bring in a project, four) steps.

pub(crate) mod flow;
pub(crate) mod parts;
pub(crate) mod wizard;
