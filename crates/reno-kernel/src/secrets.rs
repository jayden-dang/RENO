//! The `Secrets` port (ARCH-7, T1.13): named secrets that live in the Keychain.
//!
//! The kernel never talks to the Keychain. It asks the port to [`resolve`](Secrets::resolve) the
//! names a bot lists, and puts what comes back into the run's environment (`ExecSpec::env`),
//! where the harness applies it to the agent child. The port is implemented outside the kernel
//! (`renod`, over the macOS Security framework) and faked in tests ([`FakeSecrets`]).
//!
//! # What may never happen
//!
//! A value is never logged, never put in an error, never stored in SQLite and never written to a
//! journal. [`SecretValue`] makes the first of these hard: its `Debug` shows nothing. The run
//! that receives values has them redacted from everything it produces (see
//! `reno_harness::ValueRedactor`).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use reno_proto::{SECRET_VALUE_MAX, SecretNameError, check_secret_name};

/// A secret's value. `Debug` never shows it; read it with [`expose`](Self::expose).
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    /// Wraps `value`.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value. Call it only where it is handed on: never to log, format or compare for
    /// display.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the value is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue([redacted])")
    }
}

/// Why the `Secrets` port refused or failed. No variant carries a value.
#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    /// The name is not a secret name.
    #[error("invalid secret name {name:?}: {reason}")]
    InvalidName {
        /// The name asked for.
        name: String,
        /// What is wrong with it.
        reason: SecretNameError,
    },
    /// The value cannot be stored: it is empty or larger than the limit.
    #[error("invalid secret value: it must be 1 to {SECRET_VALUE_MAX} bytes")]
    InvalidValue,
    /// The secret store cannot be reached or refuses access right now: a locked or missing
    /// Keychain, a denied prompt. Retrying after the user acts may succeed.
    #[error("the secret store is unavailable: {0}")]
    Unavailable(String),
    /// The secret store failed.
    #[error("the secret store failed: {0}")]
    Failed(String),
}

/// Checks a name and a value the way every implementation must before it touches the store.
pub fn check_secret(name: &str, value: &SecretValue) -> Result<(), SecretsError> {
    check_name(name)?;
    if value.is_empty() || value.expose().len() > SECRET_VALUE_MAX {
        return Err(SecretsError::InvalidValue);
    }
    Ok(())
}

/// Checks a name.
pub fn check_name(name: &str) -> Result<(), SecretsError> {
    check_secret_name(name).map_err(|reason| SecretsError::InvalidName {
        name: name.to_owned(),
        reason,
    })
}

/// The `Secrets` port: named secrets held outside the daemon's own state.
#[async_trait]
pub trait Secrets: Send + Sync + fmt::Debug {
    /// The values of those of `names` that are set. A name that is not set is left out of the
    /// map; it is not an error. A name that is not well formed is
    /// [`SecretsError::InvalidName`].
    async fn resolve(
        &self,
        names: &[String],
    ) -> Result<BTreeMap<String, SecretValue>, SecretsError>;

    /// The names that are set, sorted. Never a value.
    async fn names(&self) -> Result<Vec<String>, SecretsError>;

    /// Stores `value` under `name`, replacing any. Whether one was replaced.
    async fn set(&self, name: &str, value: SecretValue) -> Result<bool, SecretsError>;

    /// Removes `name`. Whether it was there.
    async fn delete(&self, name: &str) -> Result<bool, SecretsError>;
}

/// An in-memory `Secrets`, for tests.
#[derive(Default)]
pub struct FakeSecrets {
    inner: Mutex<Fake>,
}

#[derive(Default)]
struct Fake {
    values: BTreeMap<String, SecretValue>,
    /// Fails every call with `Unavailable` while set.
    unavailable: Option<String>,
    /// The names `resolve` has been asked for, in order, for tests of what was and was not asked.
    asked: Vec<Vec<String>>,
}

impl fmt::Debug for FakeSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeSecrets").finish_non_exhaustive()
    }
}

impl FakeSecrets {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same, holding `name` = `value`.
    #[must_use]
    pub fn with(self, name: &str, value: &str) -> Self {
        self.lock()
            .values
            .insert(name.to_owned(), SecretValue::new(value));
        self
    }

    /// Makes every later call fail with [`SecretsError::Unavailable`]; `None` heals it.
    pub fn set_unavailable(&self, why: Option<&str>) {
        self.lock().unavailable = why.map(str::to_owned);
    }

    /// The name lists `resolve` was called with.
    pub fn asked(&self) -> Vec<Vec<String>> {
        self.lock().asked.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Fake> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn available(fake: &Fake) -> Result<(), SecretsError> {
        match &fake.unavailable {
            Some(why) => Err(SecretsError::Unavailable(why.clone())),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl Secrets for FakeSecrets {
    async fn resolve(
        &self,
        names: &[String],
    ) -> Result<BTreeMap<String, SecretValue>, SecretsError> {
        let mut fake = self.lock();
        fake.asked.push(names.to_vec());
        Self::available(&fake)?;
        for name in names {
            check_name(name)?;
        }
        Ok(names
            .iter()
            .filter_map(|name| fake.values.get(name).map(|v| (name.clone(), v.clone())))
            .collect())
    }

    async fn names(&self) -> Result<Vec<String>, SecretsError> {
        let fake = self.lock();
        Self::available(&fake)?;
        Ok(fake.values.keys().cloned().collect())
    }

    async fn set(&self, name: &str, value: SecretValue) -> Result<bool, SecretsError> {
        check_secret(name, &value)?;
        let mut fake = self.lock();
        Self::available(&fake)?;
        Ok(fake.values.insert(name.to_owned(), value).is_some())
    }

    async fn delete(&self, name: &str) -> Result<bool, SecretsError> {
        check_name(name)?;
        let mut fake = self.lock();
        Self::available(&fake)?;
        Ok(fake.values.remove(name).is_some())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_fake_resolves_what_is_set_and_leaves_out_the_rest() {
        let secrets = FakeSecrets::new().with("A_TOKEN", "value-a");
        let found = secrets
            .resolve(&["A_TOKEN".to_owned(), "B_TOKEN".to_owned()])
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found["A_TOKEN"].expose(), "value-a");
    }

    #[tokio::test]
    async fn set_reports_a_replacement_and_delete_reports_a_removal() {
        let secrets = FakeSecrets::new();
        assert!(
            !secrets
                .set("A_TOKEN", SecretValue::new("one"))
                .await
                .unwrap()
        );
        assert!(
            secrets
                .set("A_TOKEN", SecretValue::new("two"))
                .await
                .unwrap()
        );
        assert_eq!(secrets.names().await.unwrap(), ["A_TOKEN"]);
        assert!(secrets.delete("A_TOKEN").await.unwrap());
        assert!(!secrets.delete("A_TOKEN").await.unwrap());
    }

    #[tokio::test]
    async fn names_and_values_are_checked_before_the_store_is_touched() {
        let secrets = FakeSecrets::new();
        assert!(matches!(
            secrets.set("lower", SecretValue::new("v")).await,
            Err(SecretsError::InvalidName { .. })
        ));
        assert!(matches!(
            secrets.set("RENO_X", SecretValue::new("v")).await,
            Err(SecretsError::InvalidName { .. })
        ));
        assert!(matches!(
            secrets.set("OK", SecretValue::new("")).await,
            Err(SecretsError::InvalidValue)
        ));
        assert!(secrets.names().await.unwrap().is_empty());
    }

    #[test]
    fn a_value_and_its_errors_never_show_it() {
        let value = SecretValue::new("hunter2-hunter2");
        assert!(!format!("{value:?}").contains("hunter2"));
        let error = SecretsError::Failed("status -25300".into());
        assert!(!error.to_string().contains("hunter2"));
    }
}
