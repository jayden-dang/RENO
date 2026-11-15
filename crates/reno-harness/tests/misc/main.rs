//! Small suites in one test binary: each tests/*.rs file is its own binary,
//! linked against the whole crate, so a handful of tests each cost more to
//! link than to run.

// Tests unwrap freely and set environment variables.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

mod acp_hardening;
mod claude_availability;
mod run_env;
