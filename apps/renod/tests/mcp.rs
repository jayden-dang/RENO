//! `reno mcp` (T1.9) end to end: a daemon served in-process over the mock executors, and the real
//! `reno mcp` process started exactly as a harness would start it (the command, arguments and
//! environment the daemon put in the run's `ExecSpec`), spoken to over stdio by a small
//! newline-JSON MCP client written here, so the wire is tested and not a library against itself.
//! No network, no real harness (ARCH-16).
//!
//! `reno` is built beside `renod` when it is not there yet, as `reno`'s own tests build `renod`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Once;

use common::{Fixture, within};
use futures::StreamExt;
use reno_client::{Client, ClientError};
use reno_proto::{
    ApprovalDecision, ApprovalOutcome, ErrorCode, ExecEvent, McpServerSpec, ReportStatus, RunId,
    RunStatus, TaskRunParams, token_digest,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const RENO: &str = "reno";

/// Builds `reno` beside `renod` if it is not there yet.
fn reno_binary() -> PathBuf {
    static BUILD: Once = Once::new();
    let beside = Path::new(env!("CARGO_BIN_EXE_renod")).with_file_name(RENO);
    BUILD.call_once(|| {
        if beside.exists() {
            return;
        }
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "--package", "reno"])
            .status()
            .expect("run cargo");
        assert!(status.success(), "cargo build --package reno failed");
    });
    assert!(
        beside.exists(),
        "reno was not built at {}",
        beside.display()
    );
    beside
}

/// Bots, and optionally a policy, written before the daemon starts.
fn seed(config: &mut renod::DaemonConfig, policy: Option<&str>) {
    let defs = config.paths.defs_dir.clone();
    std::fs::create_dir_all(defs.join("bots")).unwrap();
    for bot in ["keeper", "scout"] {
        std::fs::write(
            defs.join(format!("bots/{bot}.toml")),
            format!("name = \"{bot}\"\ninstructions = \"x\"\nexecutors = [\"mock-hold\"]\n"),
        )
        .unwrap();
    }
    if let Some(policy) = policy {
        std::fs::write(defs.join("policy.toml"), policy).unwrap();
    }
    config.reno_bin = Some(reno_binary());
}

async fn fixture() -> Fixture {
    Fixture::with(|config| seed(config, None)).await
}

/// A run that is live on `mock-hold`, with the server the daemon injected into it.
struct Live {
    run: RunId,
    server: McpServerSpec,
}

impl Live {
    fn token_file(&self) -> PathBuf {
        PathBuf::from(&self.server.env["RENO_TOKEN_FILE"])
    }

    fn token(&self) -> String {
        std::fs::read_to_string(self.token_file()).unwrap()
    }
}

/// Starts a run (of `bot`, else with none) and waits until its executor has begun.
async fn start(fx: &Fixture, bot: Option<&str>) -> Live {
    let before = fx.executors.hold.log().specs.len();
    let run = within(fx.client.run_task(TaskRunParams {
        prompt: "go".into(),
        executor: bot.is_none().then(|| common::id("mock-hold")),
        bot: bot.map(str::to_owned),
        cwd: None,
        model: None,
        in_place: false,
        attachments: Vec::new(),
    }))
    .await
    .expect("task.run");
    // The executor's first event arrives after its spec was logged.
    let mut events = Box::pin(fx.client.run_events(run.clone(), None));
    let first = within(events.next()).await.expect("an event").expect("ok");
    assert!(
        matches!(first.event, ExecEvent::Started { .. }),
        "{first:?}"
    );
    let specs = fx.executors.hold.log().specs;
    assert_eq!(specs.len(), before + 1);
    let spec = specs
        .into_iter()
        .find(|s| s.run_id == run)
        .expect("its spec");
    Live {
        run,
        server: spec.mcp[0].clone(),
    }
}

/// Lets the held run finish.
async fn finish(fx: &Fixture, run: &RunId) -> Vec<ExecEvent> {
    within(fx.client.steer(run, "go on".into())).await.unwrap();
    fx.events(run, None)
        .await
        .into_iter()
        .map(|item| item.event)
        .collect()
}

/// An MCP client over a child's stdio: newline-delimited JSON-RPC.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next: u64,
}

impl Mcp {
    /// Starts `server` the way a harness does: its command, its arguments, and nothing in the
    /// environment but what the spec carries.
    fn spawn(server: &McpServerSpec) -> Self {
        let mut child = Command::new(&server.command)
            .args(&server.args)
            .env_clear()
            .envs(&server.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("start reno mcp");
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            lines,
            next: 1,
        }
    }

    async fn write(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// Sends a request and answers its id.
    async fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next;
        self.next += 1;
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
    }

    /// Reads until the response to `id` and answers its `result`. Anything else the server says
    /// must be a notification or a request of its own; stdout carries MCP and nothing else.
    async fn receive(&mut self, id: u64) -> Value {
        loop {
            let line = within(self.lines.next_line())
                .await
                .expect("read")
                .expect("reno mcp closed its stdout");
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdout carried more than MCP: {line:?} ({e})"));
            assert_eq!(message["jsonrpc"], "2.0", "{message}");
            if message.get("method").is_some() {
                continue;
            }
            if message["id"] == json!(id) {
                assert!(message.get("error").is_none(), "{message}");
                return message["result"].clone();
            }
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        self.receive(id).await
    }

    async fn initialize(&mut self) -> Value {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "test-harness", "version": "0"},
                }),
            )
            .await;
        self.write(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        result
    }

    /// Starts a tool call without waiting for it.
    async fn start_tool(&mut self, name: &str, arguments: Value) -> u64 {
        self.send("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    /// A finished tool call: whether it failed, and what its text said, as JSON when it is.
    async fn finish_tool(&mut self, id: u64) -> (bool, Value) {
        let result = self.receive(id).await;
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        let value = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
        (result["isError"].as_bool().unwrap_or(false), value)
    }

    async fn tool(&mut self, name: &str, arguments: Value) -> (bool, Value) {
        let id = self.start_tool(name, arguments).await;
        self.finish_tool(id).await
    }

    /// Closes stdin, as a harness does when it is done, and waits for the process to end.
    async fn close(mut self) -> std::process::ExitStatus {
        drop(self.stdin);
        within(self.child.wait()).await.unwrap()
    }
}

async fn serving(live: &Live) -> Mcp {
    let mut mcp = Mcp::spawn(&live.server);
    mcp.initialize().await;
    mcp
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_injects_reno_first_with_a_private_token_file_and_no_token_in_sight() {
    let fx = fixture().await;
    let live = start(&fx, None).await;

    let server = &live.server;
    assert_eq!(server.name, "reno");
    assert_eq!(server.args, ["mcp"]);
    assert_eq!(server.command, reno_binary().to_str().unwrap());
    assert_eq!(server.env["RENO_RUN_ID"], live.run.to_string());
    assert_eq!(server.env["RENO_SOCKET"], fx.paths.socket.to_str().unwrap());
    let file = live.token_file();
    assert!(file.starts_with(fx.paths.data_dir.join("run")));

    // 0600 in a 0700 directory, and the token is in that file and nowhere else the daemon wrote.
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(file.parent().unwrap()), 0o700);
    let token = live.token();
    assert!(token.starts_with("rnt_") && token.len() == 68);
    let spec = format!("{:?}", fx.executors.hold.log().specs);
    assert!(
        !spec.contains(&token),
        "the spec names the file, not the token"
    );

    let events = finish(&fx, &live.run).await;
    assert!(
        !format!("{events:?}").contains(&token),
        "not in the journal"
    );
    // Revoked with the run: the file is gone, and so is the directory.
    assert!(!file.exists());
    let mut leaks = Vec::new();
    scan(&fx.paths.data_dir, token.as_bytes(), &mut leaks);
    assert!(leaks.is_empty(), "the token was written to {leaks:?}");
    fx.stop().await;
}

/// Every file under `dir` that holds `needle`.
fn scan(dir: &Path, needle: &[u8], found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan(&path, needle, found);
        } else if let Ok(bytes) = std::fs::read(&path)
            && bytes.windows(needle.len()).any(|window| window == needle)
        {
            found.push(path);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_token_is_in_its_file_and_nowhere_else_in_the_data_directory_while_the_run_lives() {
    let fx = fixture().await;
    let live = start(&fx, None).await;
    let token = live.token();
    // Use it, so that anything the daemon would record about a session has been recorded.
    let mut mcp = serving(&live).await;
    mcp.tool("board_write", json!({"key": "k", "value": "v"}))
        .await;
    mcp.tool("report", json!({"status": "progress", "summary": "s"}))
        .await;

    let mut leaks = Vec::new();
    scan(&fx.paths.data_dir, token.as_bytes(), &mut leaks);
    assert_eq!(leaks, [live.token_file()], "the token is in its file only");
    drop(mcp);
    finish(&fx, &live.run).await;
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reno_mcp_speaks_mcp_and_offers_exactly_its_four_tools() {
    let fx = fixture().await;
    let live = start(&fx, None).await;
    let mut mcp = Mcp::spawn(&live.server);
    let init = mcp.initialize().await;
    assert_eq!(init["serverInfo"]["name"], "reno");
    assert!(init["capabilities"]["tools"].is_object(), "{init}");

    let listed = mcp.request("tools/list", json!({})).await;
    let tools = listed["tools"].as_array().unwrap();
    let names: BTreeSet<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        BTreeSet::from(["board_read", "board_write", "report", "request_approval"])
    );
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert!(!tool["description"].as_str().unwrap().is_empty());
    }
    let required = |name: &str| -> BTreeSet<String> {
        let tool = tools.iter().find(|t| t["name"] == name).unwrap();
        tool["inputSchema"]["required"]
            .as_array()
            .map(|r| r.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
            .unwrap_or_default()
    };
    assert_eq!(
        required("report"),
        BTreeSet::from(["status".into(), "summary".into()])
    );
    assert_eq!(
        required("board_write"),
        BTreeSet::from(["key".into(), "value".into()])
    );
    assert!(required("board_read").is_empty());
    assert_eq!(
        required("request_approval"),
        BTreeSet::from(["action".into(), "reason".into()])
    );

    // Closing stdin, as a harness does at the end, ends the process cleanly.
    assert!(mcp.close().await.success());
    finish(&fx, &live.run).await;
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn report_is_journaled_as_a_structured_event_in_the_runs_transcript() {
    let fx = fixture().await;
    let live = start(&fx, None).await;
    let mut mcp = serving(&live).await;
    let (failed, _) = mcp
        .tool(
            "report",
            json!({"status": "blocked", "summary": "need the key", "details": "tried the vault"}),
        )
        .await;
    assert!(!failed);
    // A status that is not one of the four is the model's mistake, answered as an error.
    let id = mcp
        .start_tool("report", json!({"status": "great", "summary": "x"}))
        .await;
    let rejected = mcp.send_and_wait_for_error(id).await;
    assert!(rejected);

    let events = finish(&fx, &live.run).await;
    assert!(events.iter().any(|event| matches!(
        event,
        ExecEvent::AgentReport { status: ReportStatus::Blocked, summary, details }
            if summary == "need the key" && details.as_deref() == Some("tried the vault")
    )));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ExecEvent::AgentReport { .. }))
            .count(),
        1
    );
    drop(mcp);
    fx.stop().await;
}

impl Mcp {
    /// A call that must fail: either a JSON-RPC error (bad arguments) or a tool error.
    async fn send_and_wait_for_error(&mut self, id: u64) -> bool {
        loop {
            let line = within(self.lines.next_line()).await.unwrap().unwrap();
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == json!(id) {
                return message.get("error").is_some()
                    || message["result"]["isError"].as_bool().unwrap_or(false);
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_board_is_versioned_and_a_stale_write_answers_with_the_current_entry() {
    let fx = fixture().await;
    let live = start(&fx, Some("keeper")).await;
    let mut mcp = serving(&live).await;

    let (failed, written) = mcp
        .tool(
            "board_write",
            json!({"key": "plan", "value": "v1", "expectedVersion": 0}),
        )
        .await;
    assert!(!failed, "{written}");
    assert_eq!(
        (written["scope"].as_str(), written["version"].as_u64()),
        (Some("bot:keeper"), Some(1))
    );

    let (failed, stale) = mcp
        .tool(
            "board_write",
            json!({"key": "plan", "value": "v2", "expectedVersion": 0}),
        )
        .await;
    assert!(failed);
    let text = stale.as_str().unwrap();
    assert!(
        text.contains("\"value\":\"v1\"") && text.contains("\"version\":1"),
        "{text}"
    );

    let (_, read) = mcp.tool("board_read", json!({"key": "plan"})).await;
    assert_eq!(read["entries"][0]["value"], "v1");
    let (_, listing) = mcp.tool("board_read", json!({})).await;
    assert_eq!(listing["entries"][0]["key"], "plan");
    assert!(
        listing["entries"][0].get("value").is_none(),
        "a listing has no values"
    );

    let (failed, _) = mcp
        .tool(
            "board_write",
            json!({"key": "big", "value": "x".repeat(70_000)}),
        )
        .await;
    assert!(failed, "a value over 64 KiB is refused");
    drop(mcp);
    finish(&fx, &live.run).await;
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bot_a_cannot_read_bot_b_and_a_bots_notes_outlive_its_run() {
    let fx = fixture().await;
    let keeper = start(&fx, Some("keeper")).await;
    let scout = start(&fx, Some("scout")).await;
    let mut a = serving(&keeper).await;
    let mut b = serving(&scout).await;

    a.tool("board_write", json!({"key": "secret", "value": "keeper's"}))
        .await;
    let (_, seen) = b.tool("board_read", json!({"key": "secret"})).await;
    assert_eq!(seen["scope"], "bot:scout");
    assert!(seen["entries"].as_array().unwrap().is_empty());
    let (_, listing) = b.tool("board_read", json!({})).await;
    assert!(listing["entries"].as_array().unwrap().is_empty());
    // The same key on its own board is its own.
    b.tool("board_write", json!({"key": "secret", "value": "scout's"}))
        .await;
    let (_, mine) = a.tool("board_read", json!({"key": "secret"})).await;
    assert_eq!(mine["entries"][0]["value"], "keeper's");

    // A later run of the same bot reads what the first wrote.
    drop(a);
    finish(&fx, &keeper.run).await;
    let again = start(&fx, Some("keeper")).await;
    let mut a2 = serving(&again).await;
    let (_, read) = a2.tool("board_read", json!({"key": "secret"})).await;
    assert_eq!(read["entries"][0]["value"], "keeper's");

    drop((a2, b));
    finish(&fx, &again.run).await;
    finish(&fx, &scout.run).await;
    fx.stop().await;
}

/// Waits for the first pending approval.
async fn pending(fx: &Fixture) -> reno_proto::Approval {
    let mut watch = within(fx.client.watch_approvals()).await.unwrap();
    loop {
        let snapshot = within(watch.next()).await.unwrap().unwrap();
        if let Some(approval) = snapshot.approvals.into_iter().next() {
            return approval;
        }
    }
}

fn publish() -> Value {
    json!({
        "action": "command",
        "reason": "the release is ready",
        "command": "npm publish",
        "paths": ["dist/"],
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_approval_blocks_until_a_human_approves_and_returns_the_decision() {
    let fx = fixture().await;
    let live = start(&fx, None).await;
    let mut mcp = serving(&live).await;

    let call = mcp.start_tool("request_approval", publish()).await;
    let held = pending(&fx).await;
    assert_eq!(held.run_id, live.run);
    assert_eq!(held.subject.command.as_deref(), Some("npm publish"));
    assert_eq!(held.subject.raw["reason"], "the release is ready");
    assert_eq!(fx.status(&live.run).await, RunStatus::WaitingApproval);

    within(fx.client.decide(&held.id, ApprovalDecision::ApproveOnce))
        .await
        .unwrap();
    let (failed, answer) = mcp.finish_tool(call).await;
    assert!(!failed);
    assert_eq!(answer["decision"], "approved");
    assert_eq!(answer["by"]["type"], "human");

    let events = finish(&fx, &live.run).await;
    assert!(events.iter().any(|e| matches!(
        e,
        ExecEvent::ApprovalResolved {
            decision: ApprovalOutcome::Approved,
            ..
        }
    )));
    drop(mcp);
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denial_carries_the_reason_and_asking_again_is_refused_unasked() {
    let fx = fixture().await;
    let live = start(&fx, None).await;
    let mut mcp = serving(&live).await;

    let call = mcp.start_tool("request_approval", publish()).await;
    let held = pending(&fx).await;
    within(fx.client.decide_with_message(
        &held.id,
        ApprovalDecision::Deny,
        Some("not before the review".into()),
    ))
    .await
    .unwrap();
    let (failed, answer) = mcp.finish_tool(call).await;
    assert!(!failed, "a denial is an answer, not a failure");
    assert_eq!(answer["decision"], "denied");
    assert_eq!(answer["message"], "not before the review");

    let (_, again) = mcp.tool("request_approval", publish()).await;
    assert_eq!(again["decision"], "denied");
    assert_eq!(again["by"]["type"], "repeat_denial");
    assert!(within(fx.client.list_approvals()).await.unwrap().is_empty());
    drop(mcp);
    finish(&fx, &live.run).await;
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unanswered_request_expires_as_a_denial() {
    // A one second deadline: the daemon's own timer ends the wait, nothing here sleeps.
    let fx =
        Fixture::with(|config| seed(config, Some("default = \"ask\"\napproval_ttl = \"1s\"\n")))
            .await;
    let live = start(&fx, None).await;
    let mut mcp = serving(&live).await;

    let (failed, answer) = mcp.tool("request_approval", publish()).await;
    assert!(!failed);
    assert_eq!(answer["decision"], "denied");
    assert_eq!(answer["by"]["type"], "expired");
    assert!(within(fx.client.list_approvals()).await.unwrap().is_empty());
    drop(mcp);
    finish(&fx, &live.run).await;
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_waiting_when_the_run_is_cancelled_is_answered_and_the_process_can_go() {
    let fx = fixture().await;
    let live = start(&fx, None).await;
    let mut mcp = serving(&live).await;
    let call = mcp.start_tool("request_approval", publish()).await;
    pending(&fx).await;

    within(fx.client.cancel_run(&live.run, None)).await.unwrap();
    // Whatever the process says (a denial, or an error because the run is over), it says.
    let _ = mcp.finish_tool(call).await;
    assert!(within(fx.client.list_approvals()).await.unwrap().is_empty());
    fx.stop().await;
}

// ----- Tokens, and the two kinds of connection.

fn code(error: &ClientError) -> ErrorCode {
    error.code().expect("a daemon error")
}

async fn connect(fx: &Fixture) -> Client {
    within(Client::connect_unix(&fx.paths.socket))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_works_until_the_run_ends_and_not_after() {
    let fx = fixture().await;
    let live = start(&fx, None).await;
    let token = live.token();

    let client = connect(&fx).await;
    let auth = within(client.agent_auth(&live.run, token.clone()))
        .await
        .unwrap();
    assert_eq!(auth.run_id, live.run);
    assert_eq!(auth.scope, format!("run:{}", live.run));
    within(client.agent_board_write(reno_proto::AgentBoardWriteParams {
        key: "k".into(),
        value: "v".into(),
        expected_version: None,
    }))
    .await
    .unwrap();

    finish(&fx, &live.run).await;
    // The open connection is refused on its next call, and a new one cannot authenticate.
    let after = within(client.agent_board_read(None)).await.unwrap_err();
    assert_eq!(code(&after), ErrorCode::Unauthorized);
    let fresh = connect(&fx).await;
    let refused = within(fresh.agent_auth(&live.run, token))
        .await
        .unwrap_err();
    assert_eq!(code(&refused), ErrorCode::Unauthorized);
    // And `reno mcp` itself cannot start for the run any more.
    let mut gone = Mcp::spawn(&live.server);
    let status = within(gone.child.wait()).await.unwrap();
    assert!(!status.success());
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_is_refused_for_another_run_a_wrong_token_and_a_connection_that_never_authenticated()
 {
    let fx = fixture().await;
    let first = start(&fx, Some("keeper")).await;
    let second = start(&fx, Some("scout")).await;

    let client = connect(&fx).await;
    // The first run's token under the second run's id.
    let wrong_run = within(client.agent_auth(&second.run, first.token()))
        .await
        .unwrap_err();
    assert_eq!(code(&wrong_run), ErrorCode::Unauthorized);
    let guessed = within(client.agent_auth(&first.run, token_digest(&first.token())))
        .await
        .unwrap_err();
    assert_eq!(code(&guessed), ErrorCode::Unauthorized);
    // Every refusal reads the same, so a caller cannot tell which part was wrong.
    let unknown = RunId::from_suffix("nope").unwrap();
    let missing = within(client.agent_auth(&unknown, first.token()))
        .await
        .unwrap_err();
    assert_eq!(wrong_run.to_string(), missing.to_string());

    // None of that bound the connection: its agent calls are still refused.
    let unbound = within(client.agent_board_read(None)).await.unwrap_err();
    assert_eq!(code(&unbound), ErrorCode::Unauthorized);

    finish(&fx, &first.run).await;
    finish(&fx, &second.run).await;
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_connection_cannot_call_a_single_method_of_the_owner() {
    let fx = fixture().await;
    let live = start(&fx, Some("keeper")).await;
    // Something to approve, to prove `approval.decide` is out of reach and not merely empty.
    let mut mcp = serving(&live).await;
    let call = mcp.start_tool("request_approval", publish()).await;
    let held = pending(&fx).await;

    let run = connect(&fx).await;
    within(run.agent_auth(&live.run, live.token()))
        .await
        .unwrap();

    let unauthorized =
        |error: ClientError| assert_eq!(code(&error), ErrorCode::Unauthorized, "{error}");
    unauthorized(
        within(run.decide(&held.id, ApprovalDecision::ApproveOnce))
            .await
            .unwrap_err(),
    );
    unauthorized(within(run.list_approvals()).await.unwrap_err());
    unauthorized(within(run.pause(None, true)).await.unwrap_err());
    unauthorized(within(run.resume(None)).await.unwrap_err());
    unauthorized(within(run.budget_status()).await.unwrap_err());
    unauthorized(within(run.status()).await.unwrap_err());
    unauthorized(within(run.list_runs(Default::default())).await.unwrap_err());
    unauthorized(within(run.cancel_run(&live.run, None)).await.unwrap_err());
    unauthorized(
        within(run.steer(&live.run, "stop".into()))
            .await
            .unwrap_err(),
    );
    unauthorized(within(run.list_defs(None)).await.unwrap_err());
    unauthorized(within(run.list_executors()).await.unwrap_err());
    unauthorized(
        within(run.run_task(TaskRunParams {
            prompt: "do more".into(),
            executor: Some(common::id("mock")),
            bot: None,
            cwd: None,
            model: None,
            in_place: false,
            attachments: Vec::new(),
        }))
        .await
        .unwrap_err(),
    );
    unauthorized(within(run.shutdown_daemon()).await.unwrap_err());
    // A subscription is a method too.
    let mut watch = within(run.watch_approvals()).await;
    let refused = match &mut watch {
        Ok(stream) => within(stream.next()).await.and_then(Result::err),
        Err(error) => Some(ClientError::Protocol(error.to_string())),
    };
    assert!(refused.is_some(), "a run connection watches nothing");

    // Nothing it tried changed anything: the approval is still pending, the run still live, the
    // daemon still up, and its own tools still work.
    assert_eq!(within(fx.client.list_approvals()).await.unwrap().len(), 1);
    assert_eq!(fx.status(&live.run).await, RunStatus::WaitingApproval);
    within(run.agent_board_read(None)).await.unwrap();

    within(fx.client.decide(&held.id, ApprovalDecision::Deny))
        .await
        .unwrap();
    mcp.finish_tool(call).await;
    drop(mcp);
    finish(&fx, &live.run).await;
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_owner_connection_is_not_affected_by_runs_and_keeps_every_method() {
    let fx = fixture().await;
    let live = start(&fx, None).await;
    // A run connection exists beside it.
    let run = connect(&fx).await;
    within(run.agent_auth(&live.run, live.token()))
        .await
        .unwrap();

    within(fx.client.status()).await.unwrap();
    within(fx.client.list_approvals()).await.unwrap();
    let denied = within(fx.client.agent_board_read(None)).await.unwrap_err();
    assert_eq!(code(&denied), ErrorCode::Unauthorized);
    finish(&fx, &live.run).await;
    fx.stop().await;
}
