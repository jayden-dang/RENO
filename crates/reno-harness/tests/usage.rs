//! Plan usage sources against a local endpoint, over temp directories. Nothing here reaches the
//! network, the real `~/.claude` / `~/.codex` or the Keychain (ARCH-16): a source built by
//! `for_tests` has no Keychain, and the keychain cases name a stand-in `security` script.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use reno_harness::exec::{Executor, MockExecutor};
use reno_harness::usage::{ClaudeUsage, CodexUsage, UsageSource};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const SECRET: &str = "sk-ant-oat-SECRET-TOKEN";

/// One request a canned endpoint saw: its request line and headers, lower-cased names.
#[derive(Debug, Clone)]
struct Seen {
    line: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A local HTTP endpoint that answers every request with `status` and `body` (and an optional
/// `Location`), and records what it saw.
struct Endpoint {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Endpoint {
    async fn serving(status: u16, body: &str, location: Option<&str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}/usage", listener.local_addr().expect("addr"));
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let (log, body, location) = (
            Arc::clone(&seen),
            body.to_owned(),
            location.map(str::to_owned),
        );
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0_u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                let mut lines = text.split("\r\n");
                let line = lines.next().unwrap_or_default().to_owned();
                let headers = lines
                    .take_while(|l| !l.is_empty())
                    .filter_map(|l| l.split_once(": "))
                    .map(|(n, v)| (n.to_ascii_lowercase(), v.to_owned()))
                    .collect();
                log.lock().expect("log").push(Seen { line, headers });
                let extra = location
                    .as_ref()
                    .map(|l| format!("Location: {l}\r\n"))
                    .unwrap_or_default();
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        Self { url, seen }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().expect("log").clone()
    }
}

const CLAUDE_BODY: &str = r#"{
  "five_hour": {"utilization": 54.0, "resets_at": "2026-11-18T16:00:00Z"},
  "seven_day": {"utilization": 21.0, "resets_at": "2026-11-24T16:00:00Z"}
}"#;

fn write_claude_creds(dir: &std::path::Path, token: &str) {
    std::fs::write(
        dir.join(".credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{token}","refreshToken":"RT","expiresAt":0}}}}"#
        ),
    )
    .expect("write creds");
}

#[tokio::test]
async fn claude_usage_is_read_with_the_stored_token_and_nothing_else_is_asked() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_claude_creds(dir.path(), SECRET);
    let endpoint = Endpoint::serving(200, CLAUDE_BODY, None).await;
    let source = ClaudeUsage::for_tests(dir.path(), &endpoint.url);

    let reading = source.read().await.expect("usage");

    let labels: Vec<_> = reading
        .windows
        .iter()
        .map(|w| (w.label.as_str(), w.used_permille))
        .collect();
    assert_eq!(labels, [("Session", 540), ("Week", 210)]);
    let seen = endpoint.requests();
    assert_eq!(
        seen.len(),
        1,
        "one GET, no refresh or profile call: {seen:?}"
    );
    assert!(seen[0].line.starts_with("GET /usage "));
    assert_eq!(
        seen[0].header("authorization"),
        Some(format!("Bearer {SECRET}").as_str())
    );
    assert_eq!(seen[0].header("anthropic-beta"), Some("oauth-2025-04-20"));
    // Read only: the credentials file is as it was.
    let after = std::fs::read_to_string(dir.path().join(".credentials.json")).expect("read");
    assert!(after.contains("\"refreshToken\":\"RT\"") && after.contains(SECRET));
}

#[tokio::test]
async fn a_rejected_token_is_unavailable_and_is_never_refreshed() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_claude_creds(dir.path(), SECRET);
    for status in [401, 403, 429, 500] {
        let endpoint = Endpoint::serving(status, "{}", None).await;
        let source = ClaudeUsage::for_tests(dir.path(), &endpoint.url);
        assert_eq!(source.read().await, None, "{status}");
        assert_eq!(
            endpoint.requests().len(),
            1,
            "{status}: one request, no retry or refresh"
        );
    }
}

#[tokio::test]
async fn no_login_means_no_request() {
    let dir = tempfile::tempdir().expect("tempdir");
    let endpoint = Endpoint::serving(200, CLAUDE_BODY, None).await;
    let source = ClaudeUsage::for_tests(dir.path(), &endpoint.url);
    assert_eq!(source.read().await, None);
    // An API-key style file with no OAuth block, and a file that is not JSON.
    std::fs::write(dir.path().join(".credentials.json"), "{}").expect("write");
    assert_eq!(source.read().await, None);
    std::fs::write(dir.path().join(".credentials.json"), "nope").expect("write");
    assert_eq!(source.read().await, None);
    assert!(endpoint.requests().is_empty());
}

#[tokio::test]
async fn a_reply_without_windows_is_unavailable() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_claude_creds(dir.path(), SECRET);
    let endpoint = Endpoint::serving(200, r#"{"extra_usage":{}}"#, None).await;
    assert_eq!(
        ClaudeUsage::for_tests(dir.path(), &endpoint.url)
            .read()
            .await,
        None
    );
    let endpoint = Endpoint::serving(200, "not json", None).await;
    assert_eq!(
        ClaudeUsage::for_tests(dir.path(), &endpoint.url)
            .read()
            .await,
        None
    );
}

#[tokio::test]
async fn a_redirect_is_not_followed_so_the_token_goes_nowhere_else() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_claude_creds(dir.path(), SECRET);
    let elsewhere = Endpoint::serving(200, CLAUDE_BODY, None).await;
    let endpoint = Endpoint::serving(302, "{}", Some(&elsewhere.url)).await;
    let source = ClaudeUsage::for_tests(dir.path(), &endpoint.url);
    assert_eq!(source.read().await, None);
    assert!(
        elsewhere.requests().is_empty(),
        "the redirect target saw a request"
    );
}

#[cfg(unix)]
mod keychain {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in `security`: the metadata lookup exits `lookup`, the `-w` read prints `secret`.
    fn fake_security(dir: &std::path::Path, lookup: i32, secret: &str) -> std::path::PathBuf {
        let path = dir.join("security");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\" -w\"*) printf '%s' '{secret}';;\n  *) exit {lookup};;\nesac\n"
            ),
        )
        .expect("script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn keychain_login(token: &str) -> String {
        format!(r#"{{"claudeAiOauth":{{"accessToken":"{token}"}}}}"#)
    }

    #[tokio::test]
    async fn the_keychain_login_beats_a_stale_credentials_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_claude_creds(dir.path(), "STALE-FILE-TOKEN");
        let security = fake_security(dir.path(), 0, &keychain_login("LIVE-KEYCHAIN-TOKEN"));
        let endpoint = Endpoint::serving(200, CLAUDE_BODY, None).await;
        let source = ClaudeUsage::for_tests(dir.path(), &endpoint.url)
            .with_keychain_for_tests("Claude Code-credentials", security);

        assert!(source.read().await.is_some());
        assert_eq!(
            endpoint.requests()[0].header("authorization"),
            Some("Bearer LIVE-KEYCHAIN-TOKEN")
        );
    }

    #[tokio::test]
    async fn an_absent_keychain_item_falls_back_to_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_claude_creds(dir.path(), "FILE-TOKEN");
        let security = fake_security(dir.path(), 44, "");
        let endpoint = Endpoint::serving(200, CLAUDE_BODY, None).await;
        let source = ClaudeUsage::for_tests(dir.path(), &endpoint.url)
            .with_keychain_for_tests("Claude Code-credentials", security);
        assert!(source.read().await.is_some());
        assert_eq!(
            endpoint.requests()[0].header("authorization"),
            Some("Bearer FILE-TOKEN")
        );
    }
}

fn write_codex_auth(dir: &std::path::Path, body: &str) {
    std::fs::write(dir.join("auth.json"), body).expect("write auth");
}

#[tokio::test]
async fn codex_usage_uses_the_chatgpt_token_and_account_and_the_live_plan() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_codex_auth(
        dir.path(),
        r#"{"tokens":{"access_token":"CODEX-TOKEN","account_id":"acct-1","refresh_token":"RT"}}"#,
    );
    let endpoint = Endpoint::serving(
        200,
        r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":12.0,"limit_window_seconds":18000,"reset_at":1800000000}}}"#,
        None,
    )
    .await;
    let reading = CodexUsage::for_tests(dir.path(), &endpoint.url)
        .read()
        .await
        .expect("usage");

    assert_eq!(reading.plan_label.as_deref(), Some("ChatGPT Plus"));
    assert_eq!(reading.windows.len(), 1);
    assert_eq!(reading.windows[0].used_permille, 120);
    let seen = endpoint.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].header("authorization"), Some("Bearer CODEX-TOKEN"));
    assert_eq!(seen[0].header("chatgpt-account-id"), Some("acct-1"));
}

#[tokio::test]
async fn a_codex_api_key_login_has_no_usage_and_asks_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_codex_auth(dir.path(), r#"{"OPENAI_API_KEY":"sk-key"}"#);
    let endpoint = Endpoint::serving(200, "{}", None).await;
    assert_eq!(
        CodexUsage::for_tests(dir.path(), &endpoint.url)
            .read()
            .await,
        None
    );
    assert!(endpoint.requests().is_empty());
}

#[tokio::test]
async fn an_executor_without_a_source_has_no_usage_and_one_with_a_source_reads_it() {
    let bare = MockExecutor::new("mock".parse().expect("id"));
    assert_eq!(bare.plan_usage().await, None);

    let dir = tempfile::tempdir().expect("tempdir");
    write_claude_creds(dir.path(), SECRET);
    let endpoint = Endpoint::serving(200, CLAUDE_BODY, None).await;
    let wired = MockExecutor::new("mock".parse().expect("id"))
        .with_usage_source(Arc::new(ClaudeUsage::for_tests(dir.path(), &endpoint.url)));
    assert!(wired.plan_usage().await.is_some());
}
