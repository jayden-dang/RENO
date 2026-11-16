//! How the Claude driver tells whether the CLI is signed in, without a model call.
//!
//! In order:
//!
//! 1. An API key or token in the daemon's own environment (`ANTHROPIC_API_KEY`,
//!    `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`) is a login: the CLI uses it.
//! 2. Otherwise the CLI's own `claude auth status`, which reads the stored login (the Keychain
//!    on macOS) and answers JSON with `loggedIn`. A parsed `loggedIn: false` is the one case
//!    that says "not signed in".
//! 3. Anything else (the command failed, timed out or printed something unexpected) is
//!    `Unknown`, which stays eligible: the run itself will say.
//!
//! `claude auth status` needs the CLI at or above [`super::MIN_CLI_VERSION`], which the caller
//! checks first: an older CLI is already ineligible and is not asked.

use std::path::Path;

use reno_proto::SignedIn;
use serde_json::Value;

use crate::exec::probe;

/// Environment variables that are a login by themselves.
const CREDENTIAL_ENV: [&str; 3] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
];

/// What to tell a person whose CLI is signed out.
const SIGN_IN_HINT: &str = "run `claude auth login`";

/// Reads an environment variable; the process's own unless a test says otherwise.
pub(super) type EnvLookup =
    std::sync::Arc<dyn Fn(&str) -> Option<std::ffi::OsString> + Send + Sync>;

/// The process environment.
pub(super) fn process_env() -> EnvLookup {
    std::sync::Arc::new(|name| std::env::var_os(name))
}

/// Whether the CLI at `exe` is signed in.
pub(super) async fn sign_in(exe: &Path, budget: std::time::Duration, env: &EnvLookup) -> SignedIn {
    if CREDENTIAL_ENV
        .iter()
        .any(|name| env(name).is_some_and(|value| !value.is_empty()))
    {
        return SignedIn::Yes { account: None };
    }
    match probe::status_and_stdout(exe, &["auth", "status"], budget).await {
        Some((_, stdout)) => parse_auth_status(&stdout).unwrap_or(SignedIn::Unknown),
        None => SignedIn::Unknown,
    }
}

/// `claude auth status`'s JSON as a sign-in state; `None` when it is not what we expect.
pub(super) fn parse_auth_status(stdout: &[u8]) -> Option<SignedIn> {
    let status: Value = serde_json::from_slice(stdout).ok()?;
    match status.get("loggedIn")?.as_bool()? {
        true => Some(SignedIn::Yes {
            account: status
                .get("email")
                .and_then(Value::as_str)
                .filter(|email| !email.is_empty())
                .map(str::to_owned),
        }),
        false => Some(SignedIn::No {
            hint: SIGN_IN_HINT.into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_status_json_maps_to_a_sign_in_state() {
        assert_eq!(
            parse_auth_status(br#"{"loggedIn":true,"authMethod":"claude.ai","email":"a@b.c"}"#),
            Some(SignedIn::Yes {
                account: Some("a@b.c".into())
            })
        );
        assert_eq!(
            parse_auth_status(br#"{"loggedIn":true}"#),
            Some(SignedIn::Yes { account: None })
        );
        assert_eq!(
            parse_auth_status(br#"{"loggedIn":false}"#),
            Some(SignedIn::No {
                hint: SIGN_IN_HINT.into()
            })
        );
    }

    #[test]
    fn anything_unexpected_is_not_an_answer() {
        for text in [
            &b""[..],
            b"Not logged in",
            b"{}",
            br#"{"loggedIn":"yes"}"#,
            b"[1]",
        ] {
            assert_eq!(parse_auth_status(text), None, "{text:?}");
        }
    }
}
