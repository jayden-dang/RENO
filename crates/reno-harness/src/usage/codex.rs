//! Codex's plan usage: `GET /backend-api/wham/usage` with the ChatGPT access token of
//! `$CODEX_HOME/auth.json` (default `~/.codex`). An API-key login has no ChatGPT meters, so it
//! has no usage.

use std::path::PathBuf;

use async_trait::async_trait;

use super::{UsageReading, UsageSource, http, parse};

/// Codex's usage endpoint.
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// Where Codex keeps its login, and where its usage is asked.
#[derive(Debug, Clone)]
pub struct CodexUsage {
    codex_home: PathBuf,
    endpoint: String,
}

impl CodexUsage {
    /// The production source: `CODEX_HOME` or `~/.codex`.
    pub fn from_env() -> Option<Self> {
        let codex_home = match std::env::var_os("CODEX_HOME").filter(|dir| !dir.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => std::env::home_dir()?.join(".codex"),
        };
        Some(Self {
            codex_home,
            endpoint: USAGE_URL.into(),
        })
    }

    /// A source over `codex_home` that asks `endpoint`. For tests.
    #[doc(hidden)]
    pub fn for_tests(codex_home: impl Into<PathBuf>, endpoint: impl Into<String>) -> Self {
        Self {
            codex_home: codex_home.into(),
            endpoint: endpoint.into(),
        }
    }

    /// The ChatGPT access token and account id of the live login.
    fn login(&self) -> Option<(String, String)> {
        let text = std::fs::read_to_string(self.codex_home.join("auth.json")).ok()?;
        let auth: serde_json::Value = serde_json::from_str(&text).ok()?;
        let tokens = auth.get("tokens")?;
        let field = |name: &str| {
            tokens
                .get(name)
                .and_then(serde_json::Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        Some((
            field("access_token")?,
            field("account_id").unwrap_or_default(),
        ))
    }
}

#[async_trait]
impl UsageSource for CodexUsage {
    async fn read(&self) -> Option<UsageReading> {
        let Some((token, account_id)) = self.login() else {
            tracing::debug!(
                provider = "codex",
                "plan usage unavailable: no ChatGPT login"
            );
            return None;
        };
        let client = http::client()?;
        let body = http::get_json(
            &client,
            "codex",
            &self.endpoint,
            &token,
            &[("chatgpt-account-id", &account_id)],
        )
        .await?;
        let reading = parse::codex(&body);
        if reading.is_none() {
            tracing::debug!(provider = "codex", "plan usage unavailable: no window");
        }
        reading
    }
}
