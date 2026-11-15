//! Small suites in one test binary: each tests/*.rs file is its own binary,
//! linked against the whole crate, so a handful of tests each cost more to
//! link than to run.

mod acp_hardening;
mod claude_availability;
mod model_catalog;
mod run_env;
