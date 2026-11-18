//! Signing in to an agent added from the ACP registry, started by a person from a client
//! (feature `executor.signin`).
//!
//! The agent signs in itself (ACP's `authenticate`) or hands Reno a command to run in a terminal.
//! Either way the daemon never opens a page: it **tells the client** the address (or the code to
//! type there), and the app opens it on the device the person is at. A client starts a sign-in with
//! the `executor.signin.start` subscription, reads its [`SigninEvent`]s until [`SigninEvent::Done`],
//! and may end it early with `executor.signin.cancel` (or by dropping the subscription). The
//! daemon finishes with a full probe, so the executor's health shows the result.
//!
//! `ExecutorInfo::auth_methods` lists what the agent offers, taken from the last full probe, so a
//! client can say "Sign in in browser", "Terminal: <command>" or "needs a key: VAR" before it starts
//! anything. The daemon never starts the agent to find out.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::ExecutorId;

/// Feature: `executor.signin.start` (a subscription) and `executor.signin.cancel` exist, and
/// `ExecutorInfo` may carry `authMethods`.
pub const FEATURE_EXECUTOR_SIGNIN: &str = "executor.signin";

/// How an agent signs in with one of its methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethodKind {
    /// The agent signs in itself, usually in the browser: `executor.signin.start` does it.
    Agent,
    /// A command run in a terminal on this device. `executor.signin.start` tries it headless and
    /// ends with [`SigninEvent::NeedsTerminal`] when it needs typed input.
    Terminal,
    /// A key the agent reads from its environment (`vars`): it is set in the agent's own
    /// environment (`executor.registry.env_set`), not signed in to.
    EnvVar,
}

/// One way an agent offers to sign in, from its `initialize` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuthMethod {
    /// What `executor.signin.start`'s `method` takes.
    pub id: String,
    /// The agent's name for it ("Log in with Google").
    pub name: String,
    /// What the agent says about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// How it signs in.
    pub kind: AuthMethodKind,
    /// The environment variables an `env_var` method reads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vars: Vec<String>,
    /// Where to get the key, when the agent says (an https address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// For a `terminal` method: the exact command to run in a terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Why Reno cannot use this method, when it cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unusable: Option<String>,
}

/// Params of the `executor.signin.start` subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SigninStartParams {
    /// The executor to sign in (`acp.gemini`).
    pub executor: ExecutorId,
    /// The method to use (an `authMethods` id); the agent's own first one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

/// Params of `executor.signin.cancel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SigninCancelParams {
    /// The executor whose sign-in to end.
    pub executor: ExecutorId,
}

/// Result of `executor.signin.cancel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SigninCancelResult {
    /// A sign-in was running and was told to stop; its subscription ends with `done`.
    pub cancelled: bool,
}

/// One item of the `executor.signin.start` subscription. The last one is always `done`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SigninEvent {
    /// A page for the person to open (the client opens it), where a device code may be typed.
    Page {
        /// The address, https or http.
        url: String,
    },
    /// A line the sign-in printed (a device code shows here), with secrets removed.
    Output {
        /// The line.
        line: String,
    },
    /// The agent signs in only in a terminal: run this command in one. Followed by `done`.
    NeedsTerminal {
        /// The exact command, shell-quoted.
        command: String,
    },
    /// The sign-in ended.
    Done {
        /// The executor is signed in (the full probe at the end said so).
        signed_in: bool,
        /// In words: why it failed, or what is left to do. Absent on success.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}
