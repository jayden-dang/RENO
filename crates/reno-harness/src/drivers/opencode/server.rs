//! The `opencode serve` process and the HTTP calls to it.
//!
//! Every call goes through [`Api::send`], which asks [`requests::permitted`] first: the one
//! refusal guard for routes that write opencode's configuration or credentials. The server binds
//! 127.0.0.1 only, on a port picked free just before, behind HTTP Basic auth with a password
//! minted per server and held only here (never logged: nothing prints a request, and the
//! `Debug` of [`Api`] omits it).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::StatusCode;
use serde_json::Value;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::process::owned::Child as OwnedChild;
use crate::process::{Command, Stdio};
use crate::wire::opencode::requests::{self, Method, Request};
use crate::wire::opencode::version::{self, PROBES, Protocol, ServerVersion};
use crate::{HarnessError, StderrTail};

/// Health-poll cadence while the server boots.
const HEALTH_POLL: Duration = Duration::from_millis(150);

/// A bound on ordinary HTTP calls: everything is loopback, the one slow route is a cold catalog.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// A health probe while the server boots. opencode's boot window ACCEPTS connections but parks
/// the request until the app is ready, and a request parked early enough is never answered
/// (observed by zeron on 1.18.18): abandon and poll again.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Startup retries for a server that exited on a transient fault, backing off linearly.
const SPAWN_RETRIES: u32 = 4;
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(250);

/// The user name of the server's HTTP Basic auth.
const USER: &str = "opencode";

/// What the server needs to be started.
pub(super) struct SpawnSpec<'a> {
    pub(super) exe: &'a Path,
    pub(super) cwd: &'a Path,
    /// The wire generation the CLI's version says the server speaks.
    pub(super) expected: Protocol,
    /// The inline config (`OPENCODE_CONFIG_CONTENT`), as JSON text.
    pub(super) inline_config: Option<String>,
    /// The host-resolved environment of the run (`ExecSpec::env`).
    pub(super) run_env: &'a std::collections::BTreeMap<String, String>,
    /// Offline model catalog (no models.dev fetch): the probe.
    pub(super) offline_catalog: bool,
    pub(super) startup: Duration,
}

/// An HTTP client for one server. Cheap to clone, so a detached task can hold one.
#[derive(Clone)]
pub(super) struct Api {
    pub(super) base: String,
    password: String,
    client: reqwest::Client,
    pub(super) protocol: Protocol,
    pub(super) version: ServerVersion,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("base", &self.base)
            .field("protocol", &self.protocol)
            .field("version", &self.version.raw)
            .finish_non_exhaustive()
    }
}

/// The client every call uses. The server is our own loopback child and every call carries its
/// password: reqwest's system proxy has no loopback exemption, so a machine-wide proxy would
/// receive it; a redirect would carry it elsewhere. No total timeout: the bus lives for the
/// whole run, and each call is bounded where it is made.
pub(super) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_default()
}

/// The `x-opencode-directory` value: percent-encoded like the official SDK's, `/` kept.
fn encode_directory(dir: &str) -> String {
    let mut out = String::with_capacity(dir.len());
    for b in dir.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn truncate(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= 300 {
        trimmed.to_owned()
    } else {
        let head: String = trimmed.chars().take(300).collect();
        format!("{head}…")
    }
}

impl Api {
    fn new(base: String, password: String, protocol: Protocol, version: ServerVersion) -> Self {
        Self {
            base,
            password,
            client: http_client(),
            protocol,
            version,
        }
    }

    /// A request builder for `request`, scoped to `dir`. 1.x scopes a request by the directory as
    /// a query parameter and a header (like the official SDK); 2.x rejects unknown query
    /// parameters, and there the header alone scopes it.
    pub(super) fn builder(
        &self,
        request: &Request,
        dir: Option<&str>,
    ) -> Result<reqwest::RequestBuilder, HarnessError> {
        if !requests::permitted(request) {
            return Err(HarnessError::Protocol(requests::refusal(request)));
        }
        let method = match request.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
        };
        let mut builder = self
            .client
            .request(method, format!("{}{}", self.base, request.path))
            .basic_auth(USER, Some(&self.password));
        if let Some(dir) = dir {
            builder = builder.header("x-opencode-directory", encode_directory(dir));
            if self.protocol == Protocol::V1 {
                builder = builder.query(&[("directory", dir)]);
            }
        }
        if let Some(body) = &request.body {
            builder = builder.json(body);
        }
        Ok(builder)
    }

    /// The bus request: a GET of the generation's event route.
    pub(super) fn bus_request(&self) -> Result<reqwest::RequestBuilder, HarnessError> {
        Ok(self
            .builder(&Request::get_path(self.protocol.event_path()), None)?
            .header(reqwest::header::ACCEPT, "text/event-stream"))
    }

    /// Send `request`: the status and the body. Only a transport failure or a refused request is
    /// an `Err`; the caller decides what a status means.
    pub(super) async fn send(
        &self,
        request: &Request,
        dir: Option<&str>,
    ) -> Result<(StatusCode, String), HarnessError> {
        let sent = self
            .builder(request, dir)?
            .timeout(CALL_TIMEOUT)
            .send()
            .await
            .map_err(|e| HarnessError::Protocol(format!("opencode {}: {e}", request.path)))?;
        let status = sent.status();
        let text = sent.text().await.unwrap_or_default();
        Ok((status, text))
    }

    /// `send`, with a non-2xx status as an error.
    pub(super) async fn call(
        &self,
        request: &Request,
        dir: Option<&str>,
    ) -> Result<String, HarnessError> {
        let (status, text) = self.send(request, dir).await?;
        if status.is_success() {
            Ok(text)
        } else {
            Err(HarnessError::Protocol(format!(
                "opencode {:?} {}: {status} {}",
                request.method,
                request.path,
                truncate(&text)
            )))
        }
    }

    /// `call`, the body read as JSON (an empty body is `null`).
    pub(super) async fn json(
        &self,
        request: &Request,
        dir: Option<&str>,
    ) -> Result<Value, HarnessError> {
        let text = self.call(request, dir).await?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| {
            HarnessError::Protocol(format!("opencode {}: not JSON: {e}", request.path))
        })
    }

    /// The raw bytes of a GET (the catalog is megabytes: decoded without a `Value`).
    pub(super) async fn bytes(
        &self,
        request: &Request,
        dir: Option<&str>,
    ) -> Result<Vec<u8>, HarnessError> {
        let response = self
            .builder(request, dir)?
            .timeout(CALL_TIMEOUT)
            .send()
            .await
            .map_err(|e| HarnessError::Protocol(format!("opencode {}: {e}", request.path)))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| HarnessError::Protocol(format!("opencode {}: {e}", request.path)))?;
        if status.is_success() {
            Ok(body.to_vec())
        } else {
            Err(HarnessError::Protocol(format!(
                "opencode GET {}: {status} {}",
                request.path,
                truncate(&String::from_utf8_lossy(&body))
            )))
        }
    }

    /// Whether the server answers its alive route at all, within `bound`.
    pub(super) async fn alive(&self, bound: Duration) -> bool {
        let Ok(builder) = self.builder(&Request::get_path(self.protocol.alive_path()), None) else {
            return false;
        };
        builder.timeout(bound).send().await.is_ok()
    }
}

/// Why a server did not start.
enum SpawnFailure {
    /// Launching again can succeed (a locked database, a lost migration race, a port taken).
    Retryable(HarnessError),
    Fatal(HarnessError),
}

/// opencode's report of its shared SQLite database held by another process.
fn is_database_locked(line: &str) -> bool {
    let line = line.to_ascii_lowercase();
    line.contains("database is locked") || line.contains("sqlite_busy")
}

/// A schema migration that lost a race with another process's: concurrent first starts on a fresh
/// database each run the migration DDL, and the losers exit with "Unexpected error" / "Failed
/// query:", the statement on the lines after it (zeron saw 3 to 5 of 6 at once on 1.18.21). A
/// relaunch finds the schema in place. Fed line by line.
#[derive(Default)]
struct MigrationRace {
    after_failed_query: bool,
}

impl MigrationRace {
    fn feed(&mut self, line: &str) -> bool {
        let line = line.trim().to_ascii_lowercase();
        let is_ddl = |sql: &str| {
            ["create ", "alter ", "drop "]
                .iter()
                .any(|verb| sql.starts_with(verb))
        };
        if let Some((_, sql)) = line.split_once("failed query:") {
            let sql = sql.trim();
            self.after_failed_query = sql.is_empty();
            return is_ddl(sql);
        }
        if line.is_empty() {
            return false;
        }
        std::mem::take(&mut self.after_failed_query) && is_ddl(&line)
    }
}

/// The port we picked was claimed before the server bound it.
fn is_address_in_use(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("eaddrinuse")
        || text.contains("address already in use")
        || text.contains("failed to start server. is port")
}

/// A kernel-assigned free loopback port, released for the child to claim.
fn free_localhost_port() -> Option<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .ok()
}

/// A running `opencode serve`.
pub(super) struct Server {
    pub(super) api: Api,
    child: OwnedChild,
    pub(super) stderr_tail: StderrTail,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

impl Server {
    /// Start a server and wait for a health answer that names its generation.
    pub(super) async fn spawn(spec: &SpawnSpec<'_>) -> Result<Self, HarnessError> {
        let mut attempt = 0;
        loop {
            match Self::spawn_once(spec).await {
                Err(SpawnFailure::Retryable(error)) if attempt < SPAWN_RETRIES => {
                    attempt += 1;
                    tracing::warn!(
                        target: "reno_harness::opencode",
                        attempt,
                        %error,
                        "opencode serve exited at startup on a transient fault; retrying"
                    );
                    tokio::time::sleep(SPAWN_RETRY_DELAY * attempt).await;
                }
                Err(SpawnFailure::Retryable(error) | SpawnFailure::Fatal(error)) => {
                    return Err(error);
                }
                Ok(server) => return Ok(server),
            }
        }
    }

    async fn spawn_once(spec: &SpawnSpec<'_>) -> Result<Self, SpawnFailure> {
        let fatal = |error: HarnessError| SpawnFailure::Fatal(error);
        let port = free_localhost_port().ok_or_else(|| {
            fatal(HarnessError::Protocol(
                "no free localhost port for opencode serve".into(),
            ))
        })?;
        let password = uuid::Uuid::new_v4().to_string();
        let mut cmd = Command::new(spec.exe);
        crate::child_env::restrict(&mut cmd, crate::child_env::OPENCODE);
        cmd.arg("serve")
            .arg("--port")
            .arg(port.to_string())
            .arg("--hostname")
            .arg("127.0.0.1");
        crate::compose_child_path(&mut cmd, spec.exe);
        crate::apply_run_env(&mut cmd, spec.run_env);
        // Environment only, never config: opencode would otherwise upgrade itself in place
        // (through Homebrew, for a brew install) under a running run.
        cmd.env("OPENCODE_DISABLE_AUTOUPDATE", "1");
        if spec.expected == Protocol::V1 {
            // 1.x registers the question tool only for a client app that asks for it.
            cmd.env("OPENCODE_ENABLE_QUESTION_TOOL", "true");
        }
        if spec.offline_catalog {
            cmd.env("OPENCODE_DISABLE_MODELS_FETCH", "1");
        }
        if let Some(inline) = &spec.inline_config {
            cmd.env("OPENCODE_CONFIG_CONTENT", inline);
        }
        // Ours win over anything the run's environment set: 2.x prefers OPENCODE_PASSWORD over the
        // legacy name, and a value of someone else's would lock us out of our own server.
        cmd.env("OPENCODE_PASSWORD", &password)
            .env("OPENCODE_SERVER_PASSWORD", &password)
            .env("OPENCODE_SERVER_USERNAME", USER)
            .env("OPENCODE_CLIENT", "reno");
        cmd.current_dir(spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = crate::process::owned::spawn_grouped(&mut cmd).map_err(|e| {
            fatal(crate::executable::spawn_error(e, Some(spec.cwd), || {
                crate::executable::binary_hint(spec.exe)
            }))
        })?;
        let stderr_tail = StderrTail::for_run(&redactable(spec.run_env, &password));
        let transient = Arc::new(AtomicBool::new(false));
        let listening = Arc::new(Notify::new());
        let mut tasks = Vec::new();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            let transient = transient.clone();
            tasks.push(tokio::spawn(async move {
                let mut lines = crate::lossy_lines::lossy_lines(stderr);
                let mut migration = MigrationRace::default();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "reno_harness::opencode", "stderr: {}", tail.scrub(&line));
                    if migration.feed(&line) || is_database_locked(&line) || is_address_in_use(&line) {
                        transient.store(true, Ordering::Release);
                    }
                    tail.push(&line);
                }
                tail.close();
            }));
        }
        // stdout announces the bound port; it is drained for the process's whole life, since a
        // full pipe would block the server's next write.
        if let Some(stdout) = child.stdout.take() {
            let listening = listening.clone();
            tasks.push(tokio::spawn(async move {
                let mut lines = crate::lossy_lines::lossy_lines(stdout);
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.to_ascii_lowercase().contains("server listening on") {
                        listening.notify_one();
                    }
                }
            }));
        }
        let base = format!("http://127.0.0.1:{port}");
        let probe = Api::new(
            base,
            password,
            spec.expected,
            ServerVersion::parse("unknown"),
        );
        let mut server = Self {
            api: probe,
            child,
            stderr_tail,
            tasks,
        };
        // Readiness: the server binds a few seconds into its life (minutes on a plugin-heavy cold
        // start). An exited child fails fast with its stderr tail.
        let deadline = Instant::now() + spec.startup;
        loop {
            if let Ok(Some(status)) = server.child.try_wait() {
                server.stderr_tail.wait_closed().await;
                let error = HarnessError::Protocol(crate::crash_message(
                    "opencode serve",
                    Some(status),
                    &server.stderr_tail,
                ));
                return Err(if transient.load(Ordering::Acquire) {
                    SpawnFailure::Retryable(error)
                } else {
                    fatal(error)
                });
            }
            match server.health(spec.expected).await {
                Ok(Some(found)) => {
                    server.api.version = found;
                    return Ok(server);
                }
                Ok(None) => {}
                Err(error) => {
                    server.shutdown(Duration::from_secs(1)).await;
                    return Err(fatal(error));
                }
            }
            if Instant::now() >= deadline {
                server.shutdown(Duration::from_secs(1)).await;
                return Err(fatal(HarnessError::Protocol(format!(
                    "opencode serve did not become healthy within {}s",
                    spec.startup.as_secs()
                ))));
            }
            // The listening line means the port is bound: probe at once.
            tokio::select! {
                () = tokio::time::sleep(HEALTH_POLL) => {}
                () = listening.notified() => {}
            }
        }
    }

    /// The version in the first health answer of the expected generation, if the server is up. A
    /// 401 or 403 is an error: the password was not taken (something else answers on the port).
    async fn health(&self, expected: Protocol) -> Result<Option<ServerVersion>, HarnessError> {
        for (path, protocol) in PROBES {
            if protocol != expected {
                continue;
            }
            let Ok(sent) = self
                .api
                .builder(&Request::get_path(path), None)?
                .timeout(PROBE_TIMEOUT)
                .send()
                .await
            else {
                continue;
            };
            if matches!(sent.status().as_u16(), 401 | 403) {
                return Err(HarnessError::Protocol(format!(
                    "opencode authentication rejected at {path}: {}",
                    sent.status()
                )));
            }
            if sent.status().is_success()
                && let Ok(body) = sent.json::<Value>().await
                && let Some(found) = version::health_version(&body)
            {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// The process's exit, once it has died.
    pub(super) fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// The pid, for the supervisor's process record.
    #[cfg(unix)]
    pub(super) fn record(&self) -> Option<crate::proc::record::ProcessRecord> {
        self.child.record()
    }

    /// End the server and everything it started (its MCP servers, the tools it ran): trace the
    /// descendants while it still parents them, then SIGTERM and SIGKILL the group and them.
    pub(super) async fn shutdown(&mut self, grace: Duration) {
        #[cfg(unix)]
        let tree = match self.child.id() {
            Some(pid) => crate::process::descendants(pid).await,
            None => Vec::new(),
        };
        #[cfg(not(unix))]
        let tree = Vec::new();
        crate::shutdown_agent(&mut self.child, tree, grace).await;
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

/// The values the stderr tail and the logs must not show: the run's own, and the server password.
fn redactable(
    run_env: &std::collections::BTreeMap<String, String>,
    password: &str,
) -> std::collections::BTreeMap<String, String> {
    let mut env = run_env.clone();
    env.insert("OPENCODE_SERVER_PASSWORD".into(), password.to_owned());
    env
}

/// Where the CLI is looked for besides PATH.
pub(super) fn install_locations() -> Vec<PathBuf> {
    let mut extra = Vec::new();
    if let Some(home) = crate::executable::home_dir() {
        extra.push(home.join(".opencode").join("bin").join("opencode"));
        extra.push(home.join(".local").join("bin").join("opencode"));
        extra.push(home.join(".bun").join("bin").join("opencode"));
        extra.push(home.join(".npm-global").join("bin").join("opencode"));
    }
    extra.push(PathBuf::from("/opt/homebrew/bin/opencode"));
    extra.push(PathBuf::from("/usr/local/bin/opencode"));
    extra
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_header_is_percent_encoded_with_its_slashes_kept() {
        assert_eq!(encode_directory("/tmp/a b/é"), "/tmp/a%20b/%C3%A9");
    }

    #[test]
    fn transient_startup_faults_are_told_from_fatal_ones() {
        assert!(is_database_locked("Error: SQLITE_BUSY: database is locked"));
        assert!(is_address_in_use(
            "listen EADDRINUSE: address already in use"
        ));
        assert!(!is_address_in_use("config invalid"));
        let mut race = MigrationRace::default();
        assert!(!race.feed("Unexpected error"));
        assert!(!race.feed("Failed query:"));
        assert!(race.feed("CREATE TABLE x (id)"));
        let mut other = MigrationRace::default();
        assert!(!other.feed("Failed query: select 1"));
    }

    #[test]
    fn the_debug_of_an_api_never_shows_the_password() {
        let api = Api::new(
            "http://127.0.0.1:1".into(),
            "hunter2-password".into(),
            Protocol::V1,
            ServerVersion::parse("1.19.5"),
        );
        assert!(!format!("{api:?}").contains("hunter2"));
    }
}
