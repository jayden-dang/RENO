//! Whether the SDK has a credential, from what Reno may read of it.
//!
//! The SDK's credentials are separate from `cursor-agent login` (verified live by zeron). It
//! resolves, in this order, `CURSOR_API_KEY` and then the login the SDK itself stored at
//! `~/.cursor/sdk/auth.json` (`StoredSdkCredentials`: `version`, `backendUrl`, `apiKey`,
//! `apiKeyExpiresAtMs?`, `email?`, `createdAtMs`). That file holds a minted user API key, not a
//! session token, and the SDK never refreshes it: a key past `apiKeyExpiresAtMs` is simply
//! unusable, and a corrupt or foreign-shaped file loads as "logged out".
//!
//! This module only judges bytes Reno has read. Reno never writes that file, never refreshes
//! it and never starts the SDK's browser login: it is a person's step.

use serde_json::Value;

/// What the stored login file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Login {
    /// A key that has not expired (or never does), with the account's email when it names one.
    Valid { email: Option<String> },
    /// A key past its expiry.
    Expired,
    /// Not a stored login: empty, not JSON, or without a key.
    Unusable,
}

/// Judge the bytes of `auth.json` at `now_ms` (Unix milliseconds).
pub(crate) fn stored_login(bytes: &[u8], now_ms: u64) -> Login {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return Login::Unusable;
    };
    if value
        .get("apiKey")
        .and_then(Value::as_str)
        .is_none_or(|key| key.trim().is_empty())
    {
        return Login::Unusable;
    }
    if value
        .get("apiKeyExpiresAtMs")
        .and_then(Value::as_u64)
        .is_some_and(|at| now_ms >= at)
    {
        return Login::Expired;
    }
    Login::Valid {
        email: value
            .get("email")
            .and_then(Value::as_str)
            .filter(|email| !email.is_empty())
            .map(str::to_owned),
    }
}

/// Which credential a run would sign in with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Credential {
    /// `CURSOR_API_KEY`.
    ApiKey,
    /// The SDK's stored login.
    Login { email: Option<String> },
    /// The stored login has expired and nothing else is set.
    ExpiredLogin,
    /// Nothing.
    None,
}

impl Credential {
    /// Whether a run can start on it.
    pub(crate) fn usable(&self) -> bool {
        matches!(self, Self::ApiKey | Self::Login { .. })
    }
}

/// The credential the SDK would find: the environment's key first, then the stored login.
/// `file` is the content of `auth.json`, `None` when it does not exist.
pub(crate) fn credential(api_key_set: bool, file: Option<&[u8]>, now_ms: u64) -> Credential {
    if api_key_set {
        return Credential::ApiKey;
    }
    match file.map(|bytes| stored_login(bytes, now_ms)) {
        Some(Login::Valid { email }) => Credential::Login { email },
        Some(Login::Expired) => Credential::ExpiredLogin,
        Some(Login::Unusable) | None => Credential::None,
    }
}

/// What to tell a person whose Cursor is not signed in: the fix, naming the variable.
pub(crate) fn sign_in_hint(credential: &Credential) -> String {
    let why = match credential {
        Credential::ExpiredLogin => "The Cursor SDK's stored login (~/.cursor/sdk/auth.json) has expired",
        _ => "Cursor has no API key for its SDK",
    };
    format!(
        "{why}. Set CURSOR_API_KEY (create a key at cursor.com/settings) in the environment \
         the Reno daemon starts with, or sign in with the SDK's own browser login yourself. \
         `cursor-agent login` is a separate sign-in that the SDK does not use."
    )
}

/// Whether an error message from the SDK or the shim says the credential was refused.
pub(crate) fn is_auth_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    ["api key", "not authenticated", "unauthorized", "unauthenticated", "not logged in", "not connected"]
        .iter()
        .any(|needle| lower.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    #[test]
    fn a_stored_key_is_valid_until_its_expiry_and_names_its_account() {
        let file = br#"{"version":1,"backendUrl":"https://api2.cursor.sh","apiKey":"key_x","email":"a@b.test","createdAtMs":1,"apiKeyExpiresAtMs":1800000000001}"#;
        assert_eq!(
            stored_login(file, NOW),
            Login::Valid {
                email: Some("a@b.test".into())
            }
        );
        assert_eq!(stored_login(file, NOW + 1), Login::Expired);
        // No expiry: it never does.
        let forever = br#"{"apiKey":"key_x"}"#;
        assert_eq!(stored_login(forever, NOW), Login::Valid { email: None });
    }

    #[test]
    fn a_file_that_is_not_a_stored_login_is_logged_out() {
        for bytes in [
            &b""[..],
            b"not json",
            b"[]",
            b"{}",
            br#"{"apiKey":""}"#,
            br#"{"apiKey":"  "}"#,
            br#"{"apiKey":7}"#,
        ] {
            assert_eq!(stored_login(bytes, NOW), Login::Unusable);
        }
    }

    #[test]
    fn the_environment_key_wins_and_a_missing_file_is_nothing() {
        let file = br#"{"apiKey":"k"}"#;
        assert_eq!(credential(true, Some(file), NOW), Credential::ApiKey);
        assert_eq!(credential(true, None, NOW), Credential::ApiKey);
        assert_eq!(
            credential(false, Some(file), NOW),
            Credential::Login { email: None }
        );
        assert_eq!(credential(false, None, NOW), Credential::None);
        assert_eq!(credential(false, Some(b"{"), NOW), Credential::None);
        let expired = br#"{"apiKey":"k","apiKeyExpiresAtMs":5}"#;
        assert_eq!(
            credential(false, Some(expired), NOW),
            Credential::ExpiredLogin
        );
        assert!(!Credential::ExpiredLogin.usable() && !Credential::None.usable());
        assert!(Credential::ApiKey.usable());
    }

    #[test]
    fn the_hint_names_the_variable_and_the_sign_in_that_does_not_count() {
        let hint = sign_in_hint(&Credential::None);
        assert!(hint.contains("CURSOR_API_KEY"));
        assert!(hint.contains("cursor-agent login"));
        assert!(sign_in_hint(&Credential::ExpiredLogin).contains("expired"));
    }

    #[test]
    fn an_auth_failure_is_told_from_a_network_one() {
        assert!(is_auth_failure("[unknown] Invalid User API Key"));
        assert!(is_auth_failure("ERROR_NOT_LOGGED_IN: Not logged in"));
        assert!(!is_auth_failure("fetch failed: ECONNRESET"));
    }
}
