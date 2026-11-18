//! A registry agent's own sign-in, the same for every one: what its `initialize` offers
//! (`authMethods`), signing in, signing out, and whether it is signed in. Not part of a run: a
//! run that finds its agent signed out ends with a diagnostic and never starts a sign-in.
//!
//! These spawn short-lived agent processes of their own (a sign-in client is not a run), which
//! no `ExecControls` exists to report: process records are reported through a run's
//! `ProcessSink`, and sign-in has no run, no supervisor to store a record and no fence to put on
//! one. Instead every child is owned by the call that spawned it: it is spawned as a group
//! leader (`owned::spawn_grouped`) that is signalled on drop, so a call that ends, errors, times
//! out or is dropped by its caller takes the agent and its descendants with it, and its pipe
//! readers are aborted with it. (A daemon that dies mid sign-in leaves such a child; sign-in is
//! user-driven and short, and the next sign-in starts its own.)

use std::path::{Path, PathBuf};
use std::time::Duration;

use reno_proto::AccessMode;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::AcpExecutor;
use super::launch::find_on_paths;
use super::session::{initialize_params, request_draining, request_draining_with};
use crate::HarnessError;
use crate::jsonrpc::{Incoming, RpcClient};
use crate::process::{Command, Stdio};
use crate::shutdown_child;

/// A registry agent started and initialized: its process, its client, what
/// it sends unasked, and its `initialize` answer.
pub(super) type Started = (
    crate::process::owned::Child,
    RpcClient,
    mpsc::Receiver<Incoming>,
    Value,
);

/// Builds the sign-in command from the program's arguments and its extra environment.
type CommandBuilder<'a> = dyn Fn(&[String], &[(String, String)]) -> Command + Sync + 'a;

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(330);
/// sign-out is local credential removal; this bound covers the server's cold
/// start.
const SIGN_OUT_TIMEOUT: Duration = Duration::from_secs(30);

/// How [`AcpExecutor::sign_in_registry`] runs the agent's own sign-in.
#[derive(Debug, Clone, Default)]
pub struct SignInOptions {
    /// `$BROWSER` for the agent: a no-op keeps it from opening a second tab
    /// when the caller opens the reported url itself; a recording script
    /// captures a url the agent never prints.
    pub browser: Option<PathBuf>,
    /// The `authenticate` method to use, instead of the first one the agent offers.
    pub method: Option<String>,
    /// Extra environment. A throwaway data home (`XDG_DATA_HOME`, …) lands a
    /// NEW login there, leaving the live one untouched.
    pub env: Vec<(String, std::ffi::OsString)>,
}

/// Milestones of [`AcpExecutor::sign_in_registry`] a caller can surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInProgress {
    /// The agent is waiting on the user at this sign-in url.
    OpenBrowser(String),
    /// a line the sign-in printed (where a device code to type in shows);
    /// raw, for the caller to scan and redact, never to show as it is.
    Output(String),
}

/// How a registry agent's sign-in fails when it needs typed input: this,
/// then the command to run in a terminal on the agent's device.
pub const NEEDS_TERMINAL: &str = "needs a terminal: ";

/// A sign-in method an agent offers in `initialize`'s `authMethods`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthMethod {
    /// `authenticate` with this id: the agent signs in itself.
    Agent(String),
    /// A command run in a terminal: the agent's own launch with these
    /// arguments after it (`type: "terminal"`), or, as the older
    /// `_meta["terminal-auth"]` gives it, a `command` of its own with these
    /// arguments alone (Kimi's `kimi login`).
    Terminal {
        command: Option<String>,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
}

/// A registry agent's sign-in as it says it is
/// ([`AcpExecutor::registry_sign_in_state`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrySignIn {
    pub signed_in: bool,
    /// Who it's signed in as, when it says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// It offers ACP's `logout`.
    pub can_sign_out: bool,
    pub methods: Vec<AuthMethodInfo>,
}

/// One way an agent offers to sign in (`initialize`'s `authMethods`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthMethodInfo {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// "agent" (it signs in itself), "terminal" (a command, typed into) or
    /// "env_var" (a key it reads from its environment).
    pub kind: String,
    /// The environment variable(s) an `env_var` method reads.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub vars: Vec<String>,
    /// Where to get the key, when the agent says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Why Reno cannot use this method (an agent method with no `id` cannot be authenticated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unusable: Option<String>,
}

/// The methods `initialize` offers, each with its kind. (A method without a
/// `type` is the agent's own; one with `_meta["terminal-auth"]` a terminal
/// one.)
pub(super) fn auth_methods(initialized: &Value) -> Vec<AuthMethodInfo> {
    initialized
        .get("authMethods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(at, m)| {
            // A method with no `id` is kept (the protocol requires one, but an agent that
            // forgets it still offers something): named by its place, and said unusable when
            // it is one that has to be authenticated by id.
            let given = m
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= 128);
            let id = given.map_or_else(|| format!("unnamed-{at}"), str::to_owned);
            let id = id.as_str();
            let kind = m
                .get("type")
                .and_then(Value::as_str)
                .or_else(|| m.pointer("/_meta/terminal-auth").map(|_| "terminal"))
                .unwrap_or("agent");
            if !matches!(kind, "agent" | "terminal" | "env_var") {
                return None;
            }
            let description = m.get("description").and_then(Value::as_str);
            // An agent method that says it's a key read from the environment
            // (Factory Droid's "…set in the FACTORY_API_KEY environment
            // variable"): a key to paste, as an `env_var` method is.
            let named_var = (kind == "agent")
                .then(|| description.and_then(env_var_named))
                .flatten();
            let kind = if named_var.is_some() { "env_var" } else { kind };
            let vars = match m.get("vars").and_then(Value::as_array) {
                Some(vars) => vars
                    .iter()
                    .filter_map(|v| v.get("name").and_then(Value::as_str).or_else(|| v.as_str()))
                    .map(str::to_owned)
                    .collect(),
                None => m
                    .get("varName")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(named_var)
                    .map(|v| vec![v])
                    .unwrap_or_default(),
            };
            Some(AuthMethodInfo {
                id: id.to_owned(),
                name: m
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_owned(),
                description: m
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                kind: kind.to_owned(),
                vars,
                link: m
                    .get("link")
                    .and_then(Value::as_str)
                    .filter(|l| l.starts_with("https://"))
                    .map(str::to_owned),
                unusable: (given.is_none() && kind == "agent").then(|| {
                    tracing::warn!(
                        target: "reno_harness::acp",
                        "an agent offers a sign-in method with no id; it cannot be used"
                    );
                    "the agent gave this sign-in method no id, so it cannot be used".to_owned()
                }),
            })
        })
        .collect()
}

/// The environment variable a method's description says it reads a key
/// from (`FOO_API_KEY`, `FOO_TOKEN`), when it says "environment variable".
fn env_var_named(description: &str) -> Option<String> {
    let lower = description.to_ascii_lowercase();
    if !(lower.contains("environment variable") || lower.contains("env var")) {
        return None;
    }
    description
        .split(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
        .find(|word| {
            word.len() >= 4
                && word.len() <= 64
                && word.contains('_')
                && word.starts_with(|c: char| c.is_ascii_uppercase())
                && (word.ends_with("KEY") || word.ends_with("TOKEN"))
        })
        .map(str::to_owned)
}

/// Who an agent says it's signed in as, in a `_meta` it sends: an email,
/// else an account or user name, at most a level down.
fn account_in(meta: Option<&Value>) -> Option<String> {
    fn find(value: &Value, depth: u8) -> Option<String> {
        let object = value.as_object()?;
        for want in ["email", "account", "user", "username", "login"] {
            for (key, v) in object {
                let key = key.to_ascii_lowercase().replace(['_', '-'], "");
                if (key == want || key == format!("{want}email") || key == format!("{want}name"))
                    && let Some(text) = v
                        .as_str()
                        .map(str::trim)
                        .filter(|t| !t.is_empty() && t.len() <= 200)
                {
                    return Some(text.to_owned());
                }
            }
        }
        if depth == 0 {
            return None;
        }
        object.values().find_map(|v| find(v, depth - 1))
    }
    find(meta?, 1)
}

/// The method `wanted` names, else the first the agent runs itself, else the
/// first run as a command. (A method without a `type` is the agent's own;
/// `env_var` methods, gone from the protocol, are passed over.)
fn choose_auth_method(initialized: &Value, wanted: Option<&str>) -> Option<AuthMethod> {
    let methods: Vec<&Value> = initialized
        .get("authMethods")
        .and_then(Value::as_array)
        .map(|m| m.iter().collect())
        .unwrap_or_default();
    let read = |m: &Value| -> Option<AuthMethod> {
        // (Only an agent method is authenticated by id: a terminal one runs without it.)
        let id = m.get("id").and_then(Value::as_str);
        let kind = m
            .get("type")
            .and_then(Value::as_str)
            .or_else(|| m.pointer("/_meta/terminal-auth").map(|_| "terminal"))
            .unwrap_or("agent");
        // (A key it reads from its environment isn't signed in this way:
        // it's pasted, and kept in the agent's environment.)
        let reads_a_key = m
            .get("description")
            .and_then(Value::as_str)
            .and_then(env_var_named)
            .is_some();
        match kind {
            "agent" if reads_a_key && wanted.is_none() => None,
            "agent" => Some(AuthMethod::Agent(id?.to_owned())),
            "terminal" => {
                // The method's own `type: "terminal"` with its `args` is the standard form; the
                // older `_meta["terminal-auth"]` object (its own `command`) is read only when the
                // method does not say so itself: agents that offer both get the legacy one wrong
                // (qoder's runs `node login`, kilo's names `opencode`).
                let standard = m.get("type").and_then(Value::as_str) == Some("terminal");
                let legacy = m
                    .pointer("/_meta/terminal-auth")
                    .filter(|v| v.is_object() && !standard);
                let spec = legacy.unwrap_or(m);
                let command = legacy
                    .and_then(|l| l.get("command"))
                    .and_then(Value::as_str)
                    .filter(|c| !c.trim().is_empty())
                    .map(str::to_owned);
                let args = spec
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let env = spec
                    .get("env")
                    .and_then(Value::as_object)
                    .map(|e| {
                        e.iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                            .collect()
                    })
                    .unwrap_or_default();
                Some(AuthMethod::Terminal { command, args, env })
            }
            _ => None,
        }
    };
    if let Some(wanted) = wanted {
        return methods
            .iter()
            .find(|m| m.get("id").and_then(Value::as_str) == Some(wanted))
            .and_then(|m| read(m));
    }
    let all: Vec<AuthMethod> = methods.iter().filter_map(|m| read(m)).collect();
    all.iter()
        .find(|m| matches!(m, AuthMethod::Agent(_)))
        .or_else(|| all.first())
        .cloned()
}

/// The system's openers (`open`, `xdg-open`) as a sign-in sees them: a web
/// address goes to `url_file` (beside it, the sign-in's own owner-only
/// folder) instead of a browser here; anything else goes to the real one.
#[cfg(unix)]
fn login_openers(url_file: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = url_file.parent()?.join("openers");
    std::fs::create_dir_all(&dir).ok()?;
    for (name, real) in [("open", "/usr/bin/open"), ("xdg-open", "/usr/bin/xdg-open")] {
        let script = format!(
            "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in http://*|https://*) umask 077; \
             printf '%s\\n' \"$a\" >> '{}'; exit 0;; esac; done\n\
             [ -x {real} ] && exec {real} \"$@\"\nexit 1\n",
            url_file.display().to_string().replace('\'', "'\\''")
        );
        let path = dir.join(name);
        std::fs::write(&path, script).ok()?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).ok()?;
    }
    Some(dir)
}

/// `dir` first on the child's PATH (as composed for it, else this process's).
fn prepend_path(cmd: &mut Command, dir: &Path) {
    let current = cmd
        .as_std()
        .get_envs()
        .find(|(k, _)| *k == "PATH")
        .and_then(|(_, v)| v.map(std::ffi::OsStr::to_os_string))
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(std::env::split_paths(&current));
    if let Ok(joined) = std::env::join_paths(paths) {
        cmd.env("PATH", joined);
    }
}

/// Aborts the tasks it holds when dropped, so a reader of a sign-in child's pipes ends with the
/// call that started it, even when a descendant holds the pipe open.
struct AbortOnDrop(Vec<tokio::task::AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Whether an ACP error says the agent isn't signed in: the protocol's
/// `auth_required` (-32000), or its words.
pub(super) fn still_signed_out(error: &HarnessError) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("-32000")
        || text.contains("auth_required")
        || text.contains("authentication required")
        || text.contains("not logged in")
        || text.contains("please log in")
}

/// Each line `stream` prints, reported (and any page in it opened).
async fn report_lines(
    stream: impl tokio::io::AsyncRead + Unpin,
    on_progress: std::sync::Arc<dyn Fn(SignInProgress) + Send + Sync>,
) {
    let mut lines = crate::lossy_lines::lossy_lines(stream);
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(
            target: "reno_harness::acp",
            "sign-in output: {}",
            crate::redact::redact_output(&line)
        );
        if let Some(url) = sign_in_url(&line) {
            on_progress(SignInProgress::OpenBrowser(url));
        }
        on_progress(SignInProgress::Output(line));
    }
}

/// A word as a POSIX shell reads it back.
fn shell_quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

fn sign_in_url(line: &str) -> Option<String> {
    let start = [line.find("https://"), line.find("http://")]
        .into_iter()
        .flatten()
        .min()?;
    let url = line[start..]
        .split_whitespace()
        .next()?
        .trim_end_matches(['.', ',', ';', ')', ']', '}']);
    reqwest::Url::parse(url).ok().map(|url| url.to_string())
}

impl AcpExecutor {
    /// A registry agent's own sign-in, the same for every one: what its
    /// `initialize` offers (`authMethods`), the one `method` names, else
    /// the first `agent` method (the agent signs in itself, through
    /// `authenticate`), else the first `terminal` one (its command run here
    /// with the method's arguments, no terminal attached). Either way the
    /// browser is the caller's (`$BROWSER`, captured), its page reported as
    /// [`SignInProgress::OpenBrowser`], and every line it prints as
    /// [`SignInProgress::Output`] (a device code). A terminal method that
    /// stops for typed input fails with [`NEEDS_TERMINAL`] and the command
    /// to run in a terminal instead.
    pub async fn sign_in_registry(
        &self,
        options: SignInOptions,
        on_progress: impl Fn(SignInProgress) + Send + Sync + 'static,
    ) -> Result<(), HarnessError> {
        let SignInOptions {
            browser,
            method,
            env,
        } = options;
        let display_name = self.spec.display_name;
        let on_progress: std::sync::Arc<dyn Fn(SignInProgress) + Send + Sync> =
            std::sync::Arc::new(on_progress);
        let (exe, args) = self.launcher().resolve_program()?;
        // A page the agent opens however it opens one: `$BROWSER`, or the
        // system's opener (macOS `open`, `xdg-open`, as npm's `open` and
        // most CLIs call it), recorded where the caller reads it, never
        // opened here (the app opens it on the device the person is at).
        let openers = env
            .iter()
            .find(|(k, _)| k == "RENO_LOGIN_URL_FILE")
            .and_then(|(_, file)| login_openers(Path::new(file)));
        let command = |extra: &[String], extra_env: &[(String, String)]| {
            let mut cmd = Command::new(&exe);
            cmd.args(&args).args(extra);
            crate::compose_child_path(&mut cmd, &exe);
            cmd.current_dir(crate::acp_registry::probe_dir());
            self.launcher().registry_environment(&mut cmd);
            if let Some(browser) = &browser {
                cmd.env("BROWSER", browser);
            }
            for (key, value) in &env {
                cmd.env(key, value);
            }
            for (key, value) in extra_env {
                cmd.env(key, value);
            }
            if let Some(dir) = &openers {
                prepend_path(&mut cmd, dir);
            }
            cmd
        };
        // What it offers.
        let mut cmd = command(&[], &[]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = crate::process::owned::spawn_grouped(&mut cmd).map_err(|e| {
            crate::executable::spawn_error(e, None, || crate::executable::binary_hint(&exe))
        })?;
        // The stderr reader is owned by the tracker and ends with this call.
        let _stderr_reader = child.stderr.take().map(|stderr| {
            AbortOnDrop(vec![
                self.tracker
                    .spawn(report_lines(stderr, on_progress.clone()))
                    .abort_handle(),
            ])
        });
        let (client, mut incoming) = match (child.stdin.take(), child.stdout.take()) {
            (Some(stdin), Some(stdout)) => {
                let on_progress = on_progress.clone();
                RpcClient::spawn_observing(
                    &self.tracker,
                    stdin,
                    stdout,
                    Some(Box::new(move |line| {
                        if let Some(url) = sign_in_url(line) {
                            on_progress(SignInProgress::OpenBrowser(url));
                        }
                    })),
                )
            }
            _ => {
                shutdown_child(&mut child, self.kill_grace).await;
                return Err(HarnessError::Protocol("agent child has no stdio".into()));
            }
        };
        let mut params = initialize_params();
        // Terminal methods are offered only to a client that says it runs
        // them: as the protocol now says it, and as older agents read it.
        params["clientCapabilities"]["auth"] = json!({ "terminal": true });
        params["clientCapabilities"]["_meta"]["terminal-auth"] = json!(true);
        // A page it asks to have opened (`elicitation/create`, `url` mode).
        params["clientCapabilities"]["elicitation"] = json!({ "url": {} });
        let initialized = match tokio::time::timeout(
            self.handshake_timeout,
            client.request("initialize", params),
        )
        .await
        {
            Ok(Ok(initialized)) => initialized,
            Ok(Err(error)) => {
                shutdown_child(&mut child, self.kill_grace).await;
                return Err(error);
            }
            Err(_) => {
                shutdown_child(&mut child, self.kill_grace).await;
                return Err(HarnessError::Protocol(format!(
                    "{display_name} didn't start its sign-in"
                )));
            }
        };
        let chosen = choose_auth_method(&initialized, method.as_deref());
        let Some(chosen) = chosen else {
            shutdown_child(&mut child, self.kill_grace).await;
            let unusable = auth_methods(&initialized)
                .iter()
                .any(|m| m.unusable.is_some());
            return Err(HarnessError::Protocol(if unusable {
                format!(
                    "{display_name} offers a sign-in method with no id, which Reno cannot use"
                )
            } else {
                format!("{display_name} offers no sign-in")
            }));
        };
        // How it signs in through its own command, if it does: a terminal
        // method, else, for an agent that says it signs in in a terminal
        // (`agentCapabilities._meta["terminal-auth"]`), its own CLI with no
        // ACP arguments (Factory Droid's `droid`, whose `/login` is there).
        let terminal_method = choose_auth_method(&initialized, None)
            .into_iter()
            .chain(
                initialized
                    .get("authMethods")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|m| choose_auth_method(&json!({ "authMethods": [m] }), None)),
            )
            .find(|m| matches!(m, AuthMethod::Terminal { .. }));
        let signs_in_in_terminal = initialized
            .pointer("/agentCapabilities/_meta/terminal-auth")
            .is_some_and(|v| v.as_bool() == Some(true) || v.is_object());
        let own_cli = || -> Vec<String> {
            // The launch less the registry's ACP arguments at its end.
            let acp_args = self
                .spec
                .id
                .registry()
                .and_then(crate::acp_registry::record)
                .map(|r| r.args)
                .unwrap_or_default();
            let keep = if args.ends_with(&acp_args) {
                args.len() - acp_args.len()
            } else {
                args.len()
            };
            std::iter::once(exe.display().to_string())
                .chain(args[..keep].iter().cloned())
                .collect()
        };
        match chosen {
            AuthMethod::Agent(id) => {
                // ACP's own order: a session first, and `authenticate` only
                // when it answers auth_required. Signed in already: done.
                // Some agents start their sign-in there (Factory Droid mints
                // its device code in that answer, and its `authenticate`
                // then opens the page and waits for the pairing).
                let cwd = crate::acp_registry::probe_dir();
                let can_log_out = initialized
                    .pointer("/agentCapabilities/auth/logout")
                    .is_some();
                let pages = on_progress.clone();
                let on_page = move |url: String| pages(SignInProgress::OpenBrowser(url));
                let on_page: &(dyn Fn(String) + Send + Sync) = &on_page;
                let mut verified: Result<(), HarnessError> = Ok(());
                for round in 0..2 {
                    let first = tokio::time::timeout(
                        self.probe_timeout * 3,
                        request_draining_with(
                            &client,
                            &mut incoming,
                            AccessMode::FullAccess,
                            "session/new",
                            json!({ "cwd": cwd, "mcpServers": [] }),
                            Some(on_page),
                        ),
                    )
                    .await;
                    match first {
                        Ok(Ok(_)) => {
                            shutdown_child(&mut child, self.kill_grace).await;
                            return Ok(());
                        }
                        // What it said (a device code to type in) is the
                        // sign-in's to show.
                        Ok(Err(error)) if still_signed_out(&error) => {
                            for line in error.to_string().lines().filter(|l| !l.trim().is_empty()) {
                                on_progress(SignInProgress::Output(line.trim().to_owned()));
                            }
                        }
                        _ => {}
                    }
                    let flow = request_draining_with(
                        &client,
                        &mut incoming,
                        AccessMode::FullAccess,
                        "authenticate",
                        json!({ "methodId": id }),
                        Some(on_page),
                    );
                    let result = match tokio::time::timeout(SIGN_IN_TIMEOUT, flow).await {
                        Ok(outcome) => outcome.map(|_| ()),
                        Err(_) => Err(HarnessError::Protocol(format!(
                            "{display_name} sign-in did not finish within {} minutes",
                            SIGN_IN_TIMEOUT.as_secs() / 60
                        ))),
                    };
                    // Signed in only if a session starts: some agents answer
                    // `authenticate` at once and sign nothing in.
                    verified = match result {
                        Ok(()) => {
                            let probe = request_draining_with(
                                &client,
                                &mut incoming,
                                AccessMode::FullAccess,
                                "session/new",
                                json!({ "cwd": cwd, "mcpServers": [] }),
                                Some(on_page),
                            );
                            match tokio::time::timeout(self.probe_timeout * 3, probe).await {
                                Ok(Err(error)) if still_signed_out(&error) => Err(error),
                                // Started, or failed for another reason than
                                // sign-in (its models, its network): signed in.
                                _ => Ok(()),
                            }
                        }
                        Err(error) => Err(error),
                    };
                    // Still signed out: credentials it holds and rejects
                    // ("Authentication failed") keep some agents from starting
                    // a new sign-in at all (Factory Droid mints no code then,
                    // and its `authenticate` does nothing). Once, those go
                    // (ACP's `logout`, offered as `auth.logout`), and the
                    // sign-in starts over.
                    let rejected = matches!(&verified, Err(error) if still_signed_out(error));
                    if !(rejected && can_log_out && round == 0) {
                        break;
                    }
                    let logout = request_draining_with(
                        &client,
                        &mut incoming,
                        AccessMode::FullAccess,
                        "logout",
                        json!({}),
                        Some(on_page),
                    );
                    if !matches!(
                        tokio::time::timeout(SIGN_OUT_TIMEOUT, logout).await,
                        Ok(Ok(_))
                    ) {
                        break;
                    }
                }
                shutdown_child(&mut child, self.kill_grace).await;
                match verified {
                    Ok(()) => Ok(()),
                    Err(error) if still_signed_out(&error) => match terminal_method {
                        Some(method) => {
                            self.terminal_sign_in(method, &exe, &args, &command, &on_progress)
                                .await
                        }
                        None if signs_in_in_terminal => {
                            let line = own_cli()
                                .iter()
                                .map(|a| shell_quote(a))
                                .collect::<Vec<_>>()
                                .join(" ");
                            Err(HarnessError::Protocol(format!("{NEEDS_TERMINAL}{line}")))
                        }
                        None => Err(HarnessError::Protocol(format!(
                            "{display_name} still isn't signed in: {error}"
                        ))),
                    },
                    Err(error) => Err(error),
                }
            }
            terminal @ AuthMethod::Terminal { .. } => {
                shutdown_child(&mut child, self.kill_grace).await;
                self.terminal_sign_in(terminal, &exe, &args, &command, &on_progress)
                    .await
            }
        }
    }

    /// Whether a registry agent is signed in, asked without signing it in:
    /// `initialize`, then a session (ACP's own test; one refused with
    /// `auth_required` is signed out). Who it's signed in as, when it says
    /// (an email or account in its `_meta`), and the ways it offers to
    /// sign in. Errs when it can't tell (it didn't start, it timed out).
    pub async fn registry_sign_in_state(&self) -> Result<RegistrySignIn, HarnessError> {
        let (mut child, client, mut incoming, initialized) = self.start_registry().await?;
        let cwd = crate::acp_registry::probe_dir();
        let session = tokio::time::timeout(
            self.probe_timeout * 3,
            request_draining(
                &client,
                &mut incoming,
                AccessMode::FullAccess,
                "session/new",
                json!({ "cwd": cwd, "mcpServers": [] }),
            ),
        )
        .await;
        shutdown_child(&mut child, self.kill_grace).await;
        let display_name = self.spec.display_name;
        let signed_in = match session {
            Ok(Ok(started)) => Some(started),
            Ok(Err(error)) if still_signed_out(&error) => None,
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(HarnessError::Protocol(format!(
                    "{display_name} didn't say whether it's signed in"
                )));
            }
        };
        let account = signed_in
            .as_ref()
            .and_then(|started| account_in(started.get("_meta")))
            .or_else(|| account_in(initialized.get("_meta")))
            .or_else(|| account_in(initialized.pointer("/agentInfo/_meta")));
        Ok(RegistrySignIn {
            signed_in: signed_in.is_some(),
            account,
            can_sign_out: initialized
                .pointer("/agentCapabilities/auth/logout")
                .is_some(),
            methods: auth_methods(&initialized),
        })
    }

    /// Signs a registry agent out (ACP's `logout`, where it offers one).
    pub async fn sign_out_registry(&self) -> Result<(), HarnessError> {
        let (mut child, client, mut incoming, initialized) = self.start_registry().await?;
        let display_name = self.spec.display_name;
        let outcome = if initialized
            .pointer("/agentCapabilities/auth/logout")
            .is_none()
        {
            Err(HarnessError::Protocol(format!(
                "{display_name} doesn't offer signing out"
            )))
        } else {
            let logout = request_draining(
                &client,
                &mut incoming,
                AccessMode::FullAccess,
                "logout",
                json!({}),
            );
            match tokio::time::timeout(SIGN_OUT_TIMEOUT, logout).await {
                Ok(outcome) => outcome.map(|_| ()),
                Err(_) => Err(HarnessError::Protocol(format!(
                    "{display_name} didn't finish signing out"
                ))),
            }
        };
        shutdown_child(&mut child, self.kill_grace).await;
        outcome
    }

    /// A registry agent started and initialized as a sign-in client
    /// (terminal sign-ins offered), its output discarded.
    async fn start_registry(&self) -> Result<Started, HarnessError> {
        self.start_registry_with(Stdio::null()).await
    }

    /// [`Self::start_registry`] with the agent's stderr going to `stderr`
    /// (the registry audit keeps it to explain a failed start).
    pub(super) async fn start_registry_with(&self, stderr: Stdio) -> Result<Started, HarnessError> {
        let display_name = self.spec.display_name;
        let (exe, args) = self.launcher().resolve_program()?;
        let mut cmd = Command::new(&exe);
        cmd.args(&args);
        crate::compose_child_path(&mut cmd, &exe);
        cmd.current_dir(crate::acp_registry::probe_dir());
        self.launcher().registry_environment(&mut cmd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .kill_on_drop(true);
        let mut child = crate::process::owned::spawn_grouped(&mut cmd).map_err(|e| {
            crate::executable::spawn_error(e, None, || crate::executable::binary_hint(&exe))
        })?;
        let (client, incoming) = match (child.stdin.take(), child.stdout.take()) {
            (Some(stdin), Some(stdout)) => RpcClient::spawn(&self.tracker, stdin, stdout),
            _ => {
                shutdown_child(&mut child, self.kill_grace).await;
                return Err(HarnessError::Protocol("agent child has no stdio".into()));
            }
        };
        let mut params = initialize_params();
        params["clientCapabilities"]["auth"] = json!({ "terminal": true });
        params["clientCapabilities"]["_meta"]["terminal-auth"] = json!(true);
        match tokio::time::timeout(self.handshake_timeout, client.request("initialize", params))
            .await
        {
            Ok(Ok(initialized)) => Ok((child, client, incoming, initialized)),
            Ok(Err(error)) => {
                shutdown_child(&mut child, self.kill_grace).await;
                Err(error)
            }
            Err(_) => {
                shutdown_child(&mut child, self.kill_grace).await;
                Err(HarnessError::Protocol(format!(
                    "{display_name} didn't start"
                )))
            }
        }
    }

    /// A terminal method run headless: a login that only needs the browser
    /// finishes; one that stops for typed input meets the end of its input
    /// and fails, handed to a terminal with its command ([`NEEDS_TERMINAL`]).
    async fn terminal_sign_in(
        &self,
        method: AuthMethod,
        exe: &Path,
        args: &[String],
        command: &CommandBuilder<'_>,
        on_progress: &std::sync::Arc<dyn Fn(SignInProgress) + Send + Sync>,
    ) -> Result<(), HarnessError> {
        let display_name = self.spec.display_name;
        let AuthMethod::Terminal {
            command: own,
            args: extra,
            env: extra_env,
        } = method
        else {
            return Err(HarnessError::Protocol("not a terminal sign-in".into()));
        };
        // Its own command (the older `_meta` form): that program with these
        // arguments alone; a bare name is the agent's (its install) or on
        // PATH. Else the agent's launch with these arguments after it.
        let (mut cmd, line): (Command, Vec<String>) = match own {
            Some(program) => {
                let path = PathBuf::from(&program);
                let resolved = if path.is_absolute() {
                    path
                } else if exe.file_name().is_some_and(|n| n == path.as_os_str()) {
                    exe.to_path_buf()
                } else {
                    find_on_paths(&program, Vec::new()).unwrap_or(path)
                };
                let mut cmd = command(&[], &extra_env);
                let mut replaced = Command::new(&resolved);
                replaced.args(&extra);
                crate::compose_child_path(&mut replaced, &resolved);
                replaced.current_dir(crate::acp_registry::probe_dir());
                // (The environment the agent's own launch gets, kept.)
                self.launcher().registry_environment(&mut replaced);
                for (key, value) in cmd.as_std().get_envs() {
                    if let Some(value) = value {
                        replaced.env(key, value);
                    }
                }
                cmd = replaced;
                let line = std::iter::once(resolved.display().to_string())
                    .chain(extra.iter().cloned())
                    .collect();
                (cmd, line)
            }
            None => {
                let cmd = command(&extra, &extra_env);
                let line = std::iter::once(exe.display().to_string())
                    .chain(args.iter().cloned())
                    .chain(extra.iter().cloned())
                    .collect();
                (cmd, line)
            }
        };
        cmd.env("TERM", "dumb")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut login = crate::process::owned::spawn_grouped(&mut cmd).map_err(|e| {
            crate::executable::spawn_error(e, None, || crate::executable::binary_hint(exe))
        })?;
        let mut readers = Vec::new();
        if let Some(out) = login.stdout.take() {
            readers.push(self.tracker.spawn(report_lines(out, on_progress.clone())));
        }
        if let Some(err) = login.stderr.take() {
            readers.push(self.tracker.spawn(report_lines(err, on_progress.clone())));
        }
        // Whatever way this call ends (a timeout, an error, the caller dropping it), the readers
        // end with it.
        let _readers = AbortOnDrop(readers.iter().map(JoinHandle::abort_handle).collect());
        let status = match tokio::time::timeout(SIGN_IN_TIMEOUT, login.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                shutdown_child(&mut login, self.kill_grace).await;
                return Err(HarnessError::Protocol(format!(
                    "{display_name} sign-in did not finish within {} minutes",
                    SIGN_IN_TIMEOUT.as_secs() / 60
                )));
            }
        };
        for reader in readers {
            let _ = reader.await;
        }
        if status.success() {
            return Ok(());
        }
        let line = line
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        Err(HarnessError::Protocol(format!("{NEEDS_TERMINAL}{line}")))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod opener_path_tests {
    use super::*;

    #[test]
    fn the_opener_shim_dir_stays_first_on_the_composed_path() {
        let shim = PathBuf::from("/tmp/reno-openers-test");
        let mut cmd = Command::new("true");
        // The allowlist (`child_env`) clears the environment before the PATH is composed; the
        // shim is prepended after, so it must still lead.
        crate::child_env::restrict(
            &mut cmd,
            crate::child_env::for_harness(crate::HarnessId::Grok),
        );
        crate::compose_child_path(&mut cmd, Path::new("/usr/bin/true"));
        prepend_path(&mut cmd, &shim);
        let path = cmd
            .as_std()
            .get_envs()
            .find(|(k, _)| *k == "PATH")
            .and_then(|(_, v)| v)
            .unwrap();
        assert_eq!(std::env::split_paths(path).next().unwrap(), shim);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod method_tests {
    use super::*;

    fn offered(methods: Value) -> Value {
        json!({ "authMethods": methods })
    }

    /// qoder offers both forms; its `_meta` one says `node login`, which cannot work. The
    /// method's own `type: "terminal"` and `args` are the standard form and win.
    #[test]
    fn the_standard_terminal_form_wins_over_a_wrong_legacy_one() {
        let qoder = offered(json!([{
            "id": "qodercli-login-terminal", "name": "Login", "type": "terminal", "args": ["login"],
            "_meta": { "terminal-auth": { "command": "node", "args": ["login"], "type": "terminal" } }
        }]));
        assert_eq!(
            choose_auth_method(&qoder, None),
            Some(AuthMethod::Terminal {
                command: None,
                args: vec!["login".into()],
                env: Vec::new()
            })
        );
        // kilo: the legacy form names another program (`opencode`).
        let kilo = offered(json!([{
            "id": "kilo-login", "name": "Login", "type": "terminal", "args": ["auth", "login"],
            "env": { "KILO_X": "1" },
            "_meta": { "terminal-auth": { "command": "opencode", "args": ["auth", "login"] } }
        }]));
        assert_eq!(
            choose_auth_method(&kilo, None),
            Some(AuthMethod::Terminal {
                command: None,
                args: vec!["auth".into(), "login".into()],
                env: vec![("KILO_X".into(), "1".into())]
            })
        );
    }

    /// An agent that only has the older form (Kimi: no `type` on the method) still runs it.
    #[test]
    fn the_legacy_form_still_runs_when_it_is_the_only_one() {
        let kimi = offered(json!([{
            "id": "login", "name": "Login",
            "_meta": { "terminal-auth": { "command": "/bin/kimi", "args": ["login"] } }
        }]));
        assert_eq!(
            choose_auth_method(&kimi, None),
            Some(AuthMethod::Terminal {
                command: Some("/bin/kimi".into()),
                args: vec!["login".into()],
                env: Vec::new()
            })
        );
    }

    /// agoragentic's only method has no `id`. A terminal one runs without it; an agent one cannot
    /// be authenticated, and is listed as unusable instead of vanishing.
    #[test]
    fn a_method_without_an_id_is_kept_and_said_unusable_when_it_cannot_be_used() {
        let init = offered(json!([
            { "name": "Sign in", "type": "agent" },
            { "name": "Terminal", "type": "terminal", "args": ["login"] }
        ]));
        let listed = auth_methods(&init);
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert!(
            listed[0]
                .unusable
                .as_deref()
                .is_some_and(|why| why.contains("no id"))
        );
        assert!(listed[0].id.starts_with("unnamed-"));
        assert_eq!(listed[1].unusable, None);
        assert_eq!(
            choose_auth_method(&init, None),
            Some(AuthMethod::Terminal {
                command: None,
                args: vec!["login".into()],
                env: Vec::new()
            })
        );
        let agent_only = offered(json!([{ "name": "Sign in" }]));
        assert_eq!(choose_auth_method(&agent_only, None), None);
        assert_eq!(auth_methods(&agent_only).len(), 1);
    }
}
