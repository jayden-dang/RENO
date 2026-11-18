//! Claude Code's plan usage: `GET /api/oauth/usage` with the CLI's access token.
//!
//! The token is read from where Claude Code itself reads it, in its order: on macOS the
//! Keychain item `Claude Code-credentials` first, then `$CLAUDE_CONFIG_DIR/.credentials.json`
//! (default `~/.claude`) only when the Keychain says it holds nothing. The file is a fallback the
//! CLI leaves behind, so a stale one routinely sits next to the live Keychain login; reading it
//! first would send a token that expired days ago.
//!
//! The first Keychain read from the daemon may make macOS show an access prompt for the item
//! (answer "Always Allow" to stop it asking).

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;

use super::{UsageReading, UsageSource, http, parse};

/// Claude's usage endpoint.
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// The beta flag the OAuth endpoints want.
const OAUTH_BETA: &str = "oauth-2025-04-20";

#[cfg(unix)]
const SECURITY: &str = "/usr/bin/security";

/// Where Claude Code keeps its login, and where its usage is asked.
#[derive(Debug, Clone)]
pub struct ClaudeUsage {
    config_dir: PathBuf,
    /// The Keychain service, `None` to read the credentials file only.
    keychain_service: Option<String>,
    #[cfg(unix)]
    security: PathBuf,
    endpoint: String,
}

impl ClaudeUsage {
    /// The production source: `CLAUDE_CONFIG_DIR` or `~/.claude`, and on macOS the Keychain.
    pub fn from_env() -> Option<Self> {
        let relocated = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from);
        let config_dir = match &relocated {
            Some(dir) => dir.clone(),
            None => std::env::home_dir()?.join(".claude"),
        };
        Some(Self {
            keychain_service: cfg!(target_os = "macos")
                .then(|| keychain_service(relocated.as_deref())),
            config_dir,
            #[cfg(unix)]
            security: PathBuf::from(SECURITY),
            endpoint: USAGE_URL.into(),
        })
    }

    /// A source over `config_dir` that asks `endpoint`, with no Keychain. For tests.
    #[doc(hidden)]
    pub fn for_tests(config_dir: impl Into<PathBuf>, endpoint: impl Into<String>) -> Self {
        Self {
            config_dir: config_dir.into(),
            keychain_service: None,
            #[cfg(unix)]
            security: PathBuf::from("/nonexistent/security"),
            endpoint: endpoint.into(),
        }
    }

    /// Reads the Keychain item `service` through the program at `security` first. For tests,
    /// with a stand-in program: a test must never name the real one.
    #[doc(hidden)]
    #[cfg(unix)]
    pub fn with_keychain_for_tests(
        mut self,
        service: impl Into<String>,
        security: impl Into<PathBuf>,
    ) -> Self {
        self.keychain_service = Some(service.into());
        self.security = security.into();
        self
    }

    /// The OAuth access token of the live login, or `None`.
    async fn access_token(&self) -> Option<String> {
        #[cfg(unix)]
        if let Some(service) = &self.keychain_service {
            let account = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
            match super::keychain::read_json(&self.security, service, &account).await {
                super::keychain::Read::Found(creds) => return token_of(&creds),
                // Like Claude Code, fall back to the file when the Keychain gives nothing. The
                // file may be stale; its token is then rejected and the usage is unavailable.
                super::keychain::Read::Unavailable | super::keychain::Read::Absent => {}
            }
        }
        let text = std::fs::read_to_string(self.config_dir.join(".credentials.json")).ok()?;
        token_of(&serde_json::from_str(&text).ok()?)
    }
}

#[async_trait]
impl UsageSource for ClaudeUsage {
    async fn read(&self) -> Option<UsageReading> {
        let Some(token) = self.access_token().await else {
            tracing::debug!(
                provider = "claude-code",
                "plan usage unavailable: no credentials"
            );
            return None;
        };
        let client = http::client()?;
        let body = http::get_json(
            &client,
            "claude-code",
            &self.endpoint,
            &token,
            &[("anthropic-beta", OAUTH_BETA)],
        )
        .await?;
        let reading = parse::claude(&body);
        if reading.is_none() {
            tracing::debug!(
                provider = "claude-code",
                "plan usage unavailable: no window"
            );
        }
        reading
    }
}

fn token_of(creds: &Value) -> Option<String> {
    creds
        .get("claudeAiOauth")?
        .get("accessToken")?
        .as_str()
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

/// Claude Code's Keychain service name: `Claude Code-credentials`, suffixed with the first 8 hex
/// of sha256(config dir) when `CLAUDE_CONFIG_DIR` relocates the config, since each config dir
/// is its own login.
fn keychain_service(config_dir: Option<&std::path::Path>) -> String {
    use sha2::{Digest, Sha256};
    match config_dir {
        None => "Claude Code-credentials".to_owned(),
        Some(dir) => {
            let digest = Sha256::digest(dir.to_string_lossy().as_bytes());
            let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
            format!("Claude Code-credentials-{hex}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keychain_service_is_per_config_dir() {
        assert_eq!(keychain_service(None), "Claude Code-credentials");
        let relocated = keychain_service(Some(std::path::Path::new("/tmp/claude-alt")));
        assert!(relocated.starts_with("Claude Code-credentials-"));
        assert_eq!(relocated.len(), "Claude Code-credentials-".len() + 8);
        assert_eq!(
            relocated,
            keychain_service(Some(std::path::Path::new("/tmp/claude-alt")))
        );
    }
}
