//! The `Secrets` port over the macOS Keychain (T1.13, ARCH-7).
//!
//! A secret is a generic password: service [`SERVICE`] (`dev.reno.secret`), account = the secret's
//! name, password = its value (UTF-8). The login keychain holds it, so it is encrypted at rest by
//! the system, survives a reinstall of Reno, and can be looked at or removed with Keychain Access
//! or `security find-generic-password -s dev.reno.secret -a NAME`. Nothing about a secret is in
//! SQLite, a definition file, a journal or a log.
//!
//! # Layers
//!
//! - [`Keychain`] is the narrow, blocking trait over the platform store: get, set, delete and the
//!   accounts under the service. [`MacKeychain`] implements it with `security-framework`;
//!   [`MemoryKeychain`] is the in-memory fake the tests use. No test touches the login keychain.
//! - [`KeychainSecrets`] is the kernel's `Secrets` port over any [`Keychain`]: it checks names and
//!   values, runs the blocking calls off the async threads, and maps failures to
//!   `SecretsError`. It is the one place values become `SecretValue`s.
//!
//! The first read of a secret by an unsigned or newly built `renod` may make macOS ask the user
//! to allow access; a locked keychain is reported as unavailable, which a run's trigger treats as
//! a failure only a person can fix.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use reno_kernel::{SecretValue, Secrets, SecretsError, check_name, check_secret};

/// The Keychain service every secret is stored under.
pub const SERVICE: &str = "dev.reno.secret";

/// Why the platform store refused.
#[derive(Debug, thiserror::Error)]
pub enum KeychainError {
    /// Locked, missing, or access denied: a person has to act, and retrying may then succeed.
    #[error("{0}")]
    Unavailable(String),
    /// Anything else the store reported.
    #[error("{0}")]
    Failed(String),
}

/// The platform's secret store, narrowed to what Reno uses. Blocking: call it off the async
/// threads.
pub trait Keychain: Send + Sync + fmt::Debug + 'static {
    /// The stored bytes of `name`, or `None` when it is not there.
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, KeychainError>;
    /// Stores `value` under `name`, replacing any. Whether one was replaced.
    fn set(&self, name: &str, value: &[u8]) -> Result<bool, KeychainError>;
    /// Removes `name`. Whether it was there.
    fn delete(&self, name: &str) -> Result<bool, KeychainError>;
    /// Every account stored under [`SERVICE`], in any order.
    fn accounts(&self) -> Result<Vec<String>, KeychainError>;
}

/// An in-memory [`Keychain`] for tests and for `renod --memory-secrets`.
#[derive(Default)]
pub struct MemoryKeychain {
    inner: Mutex<MemoryState>,
}

#[derive(Default)]
struct MemoryState {
    items: BTreeMap<String, Vec<u8>>,
    locked: bool,
}

impl fmt::Debug for MemoryKeychain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryKeychain").finish_non_exhaustive()
    }
}

impl MemoryKeychain {
    /// An empty keychain.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same, holding `name` = `value`.
    #[must_use]
    pub fn with(self, name: &str, value: &str) -> Self {
        self.state()
            .items
            .insert(name.to_owned(), value.as_bytes().to_vec());
        self
    }

    /// Makes every later call fail as a locked keychain does; `false` unlocks it.
    pub fn set_locked(&self, locked: bool) {
        self.state().locked = locked;
    }

    /// What is stored under `name`, for a test to look at.
    pub fn peek(&self, name: &str) -> Option<Vec<u8>> {
        self.state().items.get(name).cloned()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn open(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>, KeychainError> {
        let state = self.state();
        if state.locked {
            return Err(KeychainError::Unavailable("the keychain is locked".into()));
        }
        Ok(state)
    }
}

impl Keychain for MemoryKeychain {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, KeychainError> {
        Ok(self.open()?.items.get(name).cloned())
    }

    fn set(&self, name: &str, value: &[u8]) -> Result<bool, KeychainError> {
        Ok(self
            .open()?
            .items
            .insert(name.to_owned(), value.to_vec())
            .is_some())
    }

    fn delete(&self, name: &str) -> Result<bool, KeychainError> {
        Ok(self.open()?.items.remove(name).is_some())
    }

    fn accounts(&self) -> Result<Vec<String>, KeychainError> {
        Ok(self.open()?.items.keys().cloned().collect())
    }
}

/// A [`Keychain`] for a platform with no secret store: every call says so.
#[derive(Debug, Default)]
pub struct NoKeychain;

impl NoKeychain {
    fn refuse<T>() -> Result<T, KeychainError> {
        Err(KeychainError::Unavailable(
            "this platform has no secret store".into(),
        ))
    }
}

impl Keychain for NoKeychain {
    fn get(&self, _: &str) -> Result<Option<Vec<u8>>, KeychainError> {
        Self::refuse()
    }
    fn set(&self, _: &str, _: &[u8]) -> Result<bool, KeychainError> {
        Self::refuse()
    }
    fn delete(&self, _: &str) -> Result<bool, KeychainError> {
        Self::refuse()
    }
    fn accounts(&self) -> Result<Vec<String>, KeychainError> {
        Self::refuse()
    }
}

#[cfg(target_os = "macos")]
pub use mac::MacKeychain;

#[cfg(target_os = "macos")]
mod mac {
    use std::path::PathBuf;

    use security_framework::base::Error;
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit, SearchResult};
    use security_framework::os::macos::keychain::SecKeychain;
    use security_framework::os::macos::passwords::find_generic_password;

    use super::{Keychain, KeychainError, SERVICE};

    /// `errSecItemNotFound`.
    const ITEM_NOT_FOUND: i32 = -25300;
    /// `errSecUserCanceled`.
    const USER_CANCELED: i32 = -128;
    /// `errSecAuthFailed`, `errSecNoSuchKeychain`, `errSecInvalidKeychain`,
    /// `errSecNoDefaultKeychain`, `errSecNotAvailable` and `errSecInteractionNotAllowed`: the
    /// keychain is locked, absent or not allowed to ask: a person has to act.
    const UNAVAILABLE: [i32; 6] = [-25293, -25294, -25295, -25307, -25291, -25308];

    /// The macOS Keychain, through the Security framework's generic passwords.
    #[derive(Debug, Clone)]
    pub struct MacKeychain {
        /// A keychain file, for the manual test against a throwaway one; `None` is the user's
        /// default (login) keychain.
        path: Option<PathBuf>,
    }

    impl MacKeychain {
        /// The user's login keychain.
        pub fn login() -> Self {
            Self { path: None }
        }

        /// The keychain file at `path`. For tests against a throwaway keychain only.
        pub fn at(path: PathBuf) -> Self {
            Self { path: Some(path) }
        }

        fn open(&self) -> Result<SecKeychain, KeychainError> {
            match &self.path {
                Some(path) => SecKeychain::open(path),
                None => SecKeychain::default(),
            }
            .map_err(map_error)
        }
    }

    fn map_error(error: Error) -> KeychainError {
        let code = error.code();
        let message = error.message().unwrap_or_else(|| "no message".to_owned());
        let text = format!("keychain error {code}: {message}");
        if code == USER_CANCELED || UNAVAILABLE.contains(&code) {
            KeychainError::Unavailable(text)
        } else {
            KeychainError::Failed(text)
        }
    }

    impl Keychain for MacKeychain {
        fn get(&self, name: &str) -> Result<Option<Vec<u8>>, KeychainError> {
            let keychain = self.open()?;
            match keychain.find_generic_password(SERVICE, name) {
                Ok((password, _item)) => Ok(Some(password.to_vec())),
                Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
                Err(error) => Err(map_error(error)),
            }
        }

        fn set(&self, name: &str, value: &[u8]) -> Result<bool, KeychainError> {
            let keychain = self.open()?;
            match keychain.find_generic_password(SERVICE, name) {
                Ok((_, mut item)) => {
                    item.set_password(value).map_err(map_error)?;
                    Ok(true)
                }
                Err(error) if error.code() == ITEM_NOT_FOUND => {
                    keychain
                        .add_generic_password(SERVICE, name, value)
                        .map_err(map_error)?;
                    Ok(false)
                }
                Err(error) => Err(map_error(error)),
            }
        }

        fn delete(&self, name: &str) -> Result<bool, KeychainError> {
            let keychain = self.open()?;
            match find_generic_password(Some(std::slice::from_ref(&keychain)), SERVICE, name) {
                Ok((_, item)) => {
                    item.delete();
                    Ok(true)
                }
                Err(error) if error.code() == ITEM_NOT_FOUND => Ok(false),
                Err(error) => Err(map_error(error)),
            }
        }

        fn accounts(&self) -> Result<Vec<String>, KeychainError> {
            let keychain = self.open()?;
            let found = ItemSearchOptions::new()
                .keychains(std::slice::from_ref(&keychain))
                .class(ItemClass::generic_password())
                .service(SERVICE)
                .load_attributes(true)
                .limit(Limit::All)
                .search();
            match found {
                Ok(results) => Ok(results
                    .iter()
                    .filter_map(SearchResult::simplify_dict)
                    .filter_map(|attributes| attributes.get("acct").cloned())
                    .collect()),
                Err(error) if error.code() == ITEM_NOT_FOUND => Ok(Vec::new()),
                Err(error) => Err(map_error(error)),
            }
        }
    }
}

/// The login keychain on macOS; a store that refuses everything elsewhere.
pub fn login_keychain() -> Arc<dyn Keychain> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(MacKeychain::login())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(NoKeychain)
    }
}

/// The kernel's `Secrets` port over a [`Keychain`].
#[derive(Debug, Clone)]
pub struct KeychainSecrets {
    keychain: Arc<dyn Keychain>,
}

impl KeychainSecrets {
    /// Secrets kept in `keychain`.
    pub fn new(keychain: Arc<dyn Keychain>) -> Self {
        Self { keychain }
    }

    /// Runs a blocking Keychain call off the async threads.
    async fn blocking<T: Send + 'static>(
        &self,
        call: impl FnOnce(&dyn Keychain) -> Result<T, KeychainError> + Send + 'static,
    ) -> Result<T, SecretsError> {
        let keychain = Arc::clone(&self.keychain);
        match tokio::task::spawn_blocking(move || call(keychain.as_ref())).await {
            Ok(result) => result.map_err(secrets_error),
            Err(error) => Err(SecretsError::Failed(format!(
                "the keychain call did not finish: {error}"
            ))),
        }
    }
}

fn secrets_error(error: KeychainError) -> SecretsError {
    match error {
        KeychainError::Unavailable(why) => SecretsError::Unavailable(why),
        KeychainError::Failed(why) => SecretsError::Failed(why),
    }
}

#[async_trait]
impl Secrets for KeychainSecrets {
    async fn resolve(
        &self,
        names: &[String],
    ) -> Result<BTreeMap<String, SecretValue>, SecretsError> {
        for name in names {
            check_name(name)?;
        }
        let names = names.to_vec();
        self.blocking(move |keychain| {
            let mut found = BTreeMap::new();
            for name in names {
                if let Some(bytes) = keychain.get(&name)? {
                    // The message names the secret, never its bytes.
                    let value = String::from_utf8(bytes).map_err(|_| {
                        KeychainError::Failed(format!("the value of {name} is not valid UTF-8"))
                    })?;
                    found.insert(name, SecretValue::new(value));
                }
            }
            Ok(found)
        })
        .await
    }

    async fn names(&self) -> Result<Vec<String>, SecretsError> {
        let mut names = self.blocking(|keychain| keychain.accounts()).await?;
        // A stray item under the service that is not a secret name is not one of ours.
        names.retain(|name| check_name(name).is_ok());
        names.sort();
        names.dedup();
        Ok(names)
    }

    async fn set(&self, name: &str, value: SecretValue) -> Result<bool, SecretsError> {
        check_secret(name, &value)?;
        let name = name.to_owned();
        self.blocking(move |keychain| keychain.set(&name, value.expose().as_bytes()))
            .await
    }

    async fn delete(&self, name: &str) -> Result<bool, SecretsError> {
        check_name(name)?;
        let name = name.to_owned();
        self.blocking(move |keychain| keychain.delete(&name)).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const VALUE: &str = "ghp_exampleExampleExample1234";

    fn secrets() -> (KeychainSecrets, Arc<MemoryKeychain>) {
        let memory = Arc::new(MemoryKeychain::new());
        (KeychainSecrets::new(memory.clone()), memory)
    }

    #[tokio::test]
    async fn a_value_goes_straight_into_the_keychain_and_comes_back() {
        let (secrets, memory) = secrets();
        assert!(
            !secrets
                .set("GITHUB_TOKEN", SecretValue::new(VALUE))
                .await
                .unwrap()
        );
        assert_eq!(memory.peek("GITHUB_TOKEN").unwrap(), VALUE.as_bytes());
        assert!(
            secrets
                .set("GITHUB_TOKEN", SecretValue::new("other-value"))
                .await
                .unwrap()
        );
        let found = secrets
            .resolve(&["GITHUB_TOKEN".into(), "NOPE".into()])
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found["GITHUB_TOKEN"].expose(), "other-value");
    }

    #[tokio::test]
    async fn names_lists_only_well_formed_accounts_sorted() {
        let memory = Arc::new(
            MemoryKeychain::new()
                .with("B_TOKEN", "x")
                .with("A_TOKEN", "x")
                .with("stray lower", "x"),
        );
        let secrets = KeychainSecrets::new(memory);
        assert_eq!(secrets.names().await.unwrap(), ["A_TOKEN", "B_TOKEN"]);
    }

    #[tokio::test]
    async fn delete_says_whether_it_was_there() {
        let (secrets, _) = secrets();
        secrets.set("A_TOKEN", SecretValue::new("v")).await.unwrap();
        assert!(secrets.delete("A_TOKEN").await.unwrap());
        assert!(!secrets.delete("A_TOKEN").await.unwrap());
    }

    #[tokio::test]
    async fn a_locked_keychain_is_unavailable_and_says_nothing_of_values() {
        let (secrets, memory) = secrets();
        secrets
            .set("A_TOKEN", SecretValue::new(VALUE))
            .await
            .unwrap();
        memory.set_locked(true);
        let error = secrets.resolve(&["A_TOKEN".into()]).await.unwrap_err();
        assert!(matches!(error, SecretsError::Unavailable(_)), "{error}");
        assert!(!error.to_string().contains(VALUE));
        memory.set_locked(false);
        assert!(secrets.resolve(&["A_TOKEN".into()]).await.is_ok());
    }

    #[tokio::test]
    async fn bad_names_and_values_never_reach_the_keychain() {
        let (secrets, memory) = secrets();
        assert!(matches!(
            secrets.set("lower", SecretValue::new("v")).await,
            Err(SecretsError::InvalidName { .. })
        ));
        assert!(matches!(
            secrets.set("A_TOKEN", SecretValue::new("")).await,
            Err(SecretsError::InvalidValue)
        ));
        assert!(matches!(
            secrets.resolve(&["bad name".into()]).await,
            Err(SecretsError::InvalidName { .. })
        ));
        assert!(memory.accounts().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_value_that_is_not_utf8_fails_without_showing_it() {
        let memory = Arc::new(MemoryKeychain::new());
        memory.set("A_TOKEN", &[0xff, 0xfe, 0xfd]).unwrap();
        let secrets = KeychainSecrets::new(memory);
        let error = secrets.resolve(&["A_TOKEN".into()]).await.unwrap_err();
        assert!(matches!(error, SecretsError::Failed(_)));
        assert!(error.to_string().contains("A_TOKEN"));
    }

    /// Against a throwaway keychain file, never the login keychain. Run by hand:
    ///
    /// ```text
    /// cargo test -p renod --lib secrets::tests::throwaway_keychain -- --ignored
    /// ```
    ///
    /// It creates `reno-test.keychain` in a temporary directory with `SecKeychainCreate` (password
    /// `reno-test`), stores, replaces, reads, lists and deletes a secret in it, and removes the
    /// directory with it. The login keychain and the keychain search list are not touched; macOS
    /// may ask once whether the test binary may use the new keychain.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "uses the macOS Keychain APIs (a throwaway keychain file); run by hand"]
    fn throwaway_keychain_roundtrip() {
        use security_framework::os::macos::keychain::CreateOptions;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reno-test.keychain");
        CreateOptions::new()
            .password("reno-test")
            .create(&path)
            .unwrap();
        let keychain = MacKeychain::at(path);

        assert_eq!(keychain.get("T_TOKEN").unwrap(), None);
        assert!(!keychain.set("T_TOKEN", b"value-one-1234").unwrap());
        assert!(keychain.set("T_TOKEN", b"value-two-5678").unwrap());
        assert_eq!(keychain.get("T_TOKEN").unwrap().unwrap(), b"value-two-5678");
        assert_eq!(keychain.accounts().unwrap(), ["T_TOKEN"]);
        assert!(keychain.delete("T_TOKEN").unwrap());
        assert!(!keychain.delete("T_TOKEN").unwrap());
        assert!(keychain.accounts().unwrap().is_empty());
    }
}
