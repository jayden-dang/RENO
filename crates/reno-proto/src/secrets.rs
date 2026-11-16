//! Named secrets (T1.13): what `secret.list`, `secret.set` and `secret.delete` carry, and the
//! rule for a secret's name.
//!
//! A secret lives in the macOS Keychain under its name and nowhere else: never in SQLite, never in
//! the journal, never in a definition file. A bot names the secrets its runs receive
//! (`secrets = ["GITHUB_TOKEN"]`), and each is handed to the harness as an environment variable of
//! the same name. The wire carries a value in exactly one direction: `secret.set`, over the local
//! socket. No method returns one.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The longest secret name, in bytes.
pub const SECRET_NAME_MAX: usize = 64;
/// The longest secret value `secret.set` accepts, in bytes.
pub const SECRET_VALUE_MAX: usize = 64 * 1024;
/// The prefix Reno keeps for the variables it sets itself (`RENO_RUN_ID`): no secret may use it,
/// so a secret can never replace one.
pub const RESERVED_PREFIX: &str = "RENO_";

/// The rule a secret name must satisfy, in words.
pub const SECRET_NAME_RULE: &str = "an upper-case letter, then upper-case letters, digits or `_`, at most 64 characters in all (`[A-Z][A-Z0-9_]{0,63}`)";

/// Why a name is not a secret name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SecretNameError {
    /// Empty, too long, a character outside `[A-Z0-9_]`, or not starting with a letter.
    #[error("not a valid secret name: {SECRET_NAME_RULE}")]
    Malformed,
    /// Starts with `RENO_`, which Reno keeps for its own variables.
    #[error("secret names may not start with RENO_: Reno keeps that prefix")]
    Reserved,
}

/// Checks `name` against `[A-Z][A-Z0-9_]{0,63}` and the reserved prefix.
pub fn check_secret_name(name: &str) -> Result<(), SecretNameError> {
    let bytes = name.as_bytes();
    let well_formed = !bytes.is_empty()
        && bytes.len() <= SECRET_NAME_MAX
        && bytes[0].is_ascii_uppercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_');
    if !well_formed {
        return Err(SecretNameError::Malformed);
    }
    if name.starts_with(RESERVED_PREFIX) {
        return Err(SecretNameError::Reserved);
    }
    Ok(())
}

/// The tag a missing-secret diagnostic starts with.
pub const SECRET_MISSING_DIAGNOSTIC_TAG: &str = "SecretMissing:";

/// Why a run did not start: secrets its bot lists are not set. It is the `diagnostic` of the run's
/// `Done { stop: error }`, as [`SecretsMissing::to_diagnostic`] writes it: the tag
/// [`SECRET_MISSING_DIAGNOSTIC_TAG`], a space and this value as JSON. A client reads it with
/// [`SecretsMissing::from_diagnostic`], the one decoder; nobody matches the text. It names the
/// secrets and never carries a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretsMissing {
    /// The bot whose run was refused.
    pub bot: String,
    /// The names that are not set, in the order the bot lists them.
    pub missing: Vec<String>,
}

impl SecretsMissing {
    /// The diagnostic of a run refused for these.
    pub fn to_diagnostic(&self) -> String {
        let body = serde_json::to_string(self).unwrap_or_default();
        format!("{SECRET_MISSING_DIAGNOSTIC_TAG} {body}")
    }

    /// The finding a `Done` diagnostic carries, or `None` for any other diagnostic.
    pub fn from_diagnostic(diagnostic: &str) -> Option<Self> {
        let body = diagnostic.strip_prefix(SECRET_MISSING_DIAGNOSTIC_TAG)?;
        serde_json::from_str(body.trim()).ok()
    }
}

impl fmt::Display for SecretsMissing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (noun, verb) = if self.missing.len() == 1 {
            ("secret", "is")
        } else {
            ("secrets", "are")
        };
        write!(
            f,
            "{} needs {noun} {}, which {verb} not set; set each with `reno secret set <NAME>`",
            self.bot,
            self.missing.join(", ")
        )
    }
}

/// One secret `secret.list` knows of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretInfo {
    /// The secret's name.
    pub name: String,
    /// Whether the Keychain holds a value for it. A name that a bot asks for and nobody has set
    /// is listed with `false`.
    pub set: bool,
    /// The bots whose definitions ask for it, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub used_by: Vec<String>,
}

/// Result of `secret.list`: names and whether each is set, never a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretListResult {
    /// Every secret in the Keychain or asked for by a bot, sorted by name.
    pub secrets: Vec<SecretInfo>,
}

/// Params of `secret.set`. The value is stored straight into the Keychain; it never appears in a
/// log, an error or this type's `Debug`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretSetParams {
    /// The secret's name.
    pub name: String,
    /// The value.
    pub value: String,
}

impl fmt::Debug for SecretSetParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretSetParams")
            .field("name", &self.name)
            .field("value", &"[redacted]")
            .finish()
    }
}

/// Result of `secret.set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretSetResult {
    /// The secret already had a value, which is now replaced.
    pub replaced: bool,
}

/// Params of `secret.delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretDeleteParams {
    /// The secret's name.
    pub name: String,
}

/// Result of `secret.delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretDeleteResult {
    /// The Keychain had it. Deleting a name that is not there is not an error.
    pub deleted: bool,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_rule() {
        let max = "X".repeat(64);
        for ok in ["A", "GITHUB_TOKEN", "A1_B2", max.as_str(), "RENOX"] {
            assert_eq!(check_secret_name(ok), Ok(()), "{ok}");
        }
        let long = "X".repeat(65);
        for bad in [
            "",
            "github_token",
            "1ABC",
            "_ABC",
            "A-B",
            "A B",
            "É",
            long.as_str(),
        ] {
            assert_eq!(
                check_secret_name(bad),
                Err(SecretNameError::Malformed),
                "{bad}"
            );
        }
        assert_eq!(
            check_secret_name("RENO_RUN_ID"),
            Err(SecretNameError::Reserved)
        );
    }

    #[test]
    fn a_missing_secret_diagnostic_round_trips_and_other_text_is_not_one() {
        let missing = SecretsMissing {
            bot: "keeper".into(),
            missing: vec!["GITHUB_TOKEN".into(), "NPM_TOKEN".into()],
        };
        let text = missing.to_diagnostic();
        assert!(text.starts_with("SecretMissing: {"), "{text}");
        assert_eq!(
            SecretsMissing::from_diagnostic(&text),
            Some(missing.clone())
        );
        assert_eq!(
            SecretsMissing::from_diagnostic("NoEligibleExecutor: x"),
            None
        );
        assert_eq!(
            SecretsMissing::from_diagnostic("SecretMissing: not json"),
            None
        );
        assert_eq!(
            missing.to_string(),
            "keeper needs secrets GITHUB_TOKEN, NPM_TOKEN, which are not set; set each with `reno secret set <NAME>`"
        );
    }

    #[test]
    fn set_params_debug_hides_the_value() {
        let params = SecretSetParams {
            name: "TOKEN".into(),
            value: "hunter2-hunter2".into(),
        };
        assert!(!format!("{params:?}").contains("hunter2"));
    }
}
