//! The ACP registry audit: every agent of the public registry, through Reno's own registry path
//! (`acp_registry::{parse_index, choose, readiness, install}` and the executor's registry launch
//! and handshake), with no model call: `initialize`, then `session/new`, then the agent is shut
//! down. Never a `session/prompt`.
//!
//! Opt-in and ignored, so `cargo test` never touches the network or the desktop:
//!
//! ```text
//! RENO_ACP_AUDIT_OUT=/tmp/acp-audit RENO_ACP_AUDIT_NODE_BIN=<dir with real node+npm> \
//!   cargo test -p reno-harness --lib registry_audit -- --ignored --nocapture
//! ```
//!
//! `RENO_ACP_AUDIT_INDEX` reads the index from a file instead of the CDN; `RENO_ACP_AUDIT_ONLY`
//! (comma separated ids) narrows the run; `RENO_ACP_AUDIT_UV_BIN` puts a uv on the agents' PATH;
//! `RENO_ACP_AUDIT_KEEP=1` keeps the scratch installs. Results land in `<out>/results.jsonl`.
//!
//! Isolation: the orchestrator test runs each agent in a WORKER (this same test binary, the
//! ignored `registry_audit_worker` test) started with a cleared environment: `HOME`,
//! `XDG_*_HOME` and `TMPDIR` in a per-agent scratch folder, a PATH that starts with recording
//! no-op shims (`open`, `xdg-open`, `osascript`, `security`, `launchctl`; they log and exit 1),
//! and `BROWSER` at one too. Reno's code inside the worker therefore sees only that world, and
//! so does every process it starts. The worker leads its own process group, which the
//! orchestrator kills on timeout and after the run.
//!
//! What this cannot see: a program that opens a browser by absolute path (`/usr/bin/open`) or
//! calls the Keychain through the Security framework bypasses the shims. "No side effects
//! observed" means none through the shims.

#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use reno_proto::AccessMode;
use serde_json::{Value, json};

use super::AcpExecutor;
use super::session::request_draining;
use super::signin::{auth_methods, still_signed_out};
use crate::acp_registry as reg;
use crate::agent_id::RegistryAgent;
use crate::process::Stdio;
use crate::shutdown_child;

const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);
const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// The orchestrator's bound on a whole worker: every step's bound and some slack.
const WORKER_TIMEOUT: Duration = Duration::from_secs(600 + 30 + 30 + 90);
const SHIMS: [&str; 5] = ["open", "xdg-open", "osascript", "security", "launchctl"];
/// Agents Reno is turning into native drivers on other branches.
const NATIVE_COMING: [&str; 3] = ["cursor", "opencode", "pi-acp"];

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn secs(since: Instant) -> f64 {
    (since.elapsed().as_secs_f64() * 10.0).round() / 10.0
}

// ---------------------------------------------------------------------------------------------
// Orchestrator.

/// The agent's scratch world and the command that runs the worker in it.
fn worker_command(
    work: &Path,
    agent_id: &str,
    index: &Path,
    result: &Path,
) -> (std::process::Command, PathBuf) {
    let dir = work.join(agent_id);
    let shims = dir.join("shims");
    for sub in ["home", "config", "data", "cache", "tmp", "shims"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let log = dir.join("shim.log");
    std::fs::write(&log, "").unwrap();
    for name in SHIMS.iter().copied().chain(["browser"]) {
        let path = shims.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s %s\\n' '{name}' \"$*\" >> '{}'\nexit 1\n",
                log.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut path = vec![shims.clone()];
    path.extend(env_path("RENO_ACP_AUDIT_NODE_BIN"));
    path.extend(env_path("RENO_ACP_AUDIT_UV_BIN"));
    path.extend(["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from));
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "drivers::acp::audit::registry_audit_worker",
        "--exact",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ])
    .env_clear()
    .env("PATH", std::env::join_paths(path).unwrap())
    .env("HOME", dir.join("home"))
    .env("XDG_CONFIG_HOME", dir.join("config"))
    .env("XDG_DATA_HOME", dir.join("data"))
    .env("XDG_CACHE_HOME", dir.join("cache"))
    .env("TMPDIR", dir.join("tmp"))
    .env("BROWSER", shims.join("browser"))
    .env("USER", "audit")
    .env("LANG", "en_US.UTF-8")
    .env("SHELL", "/bin/sh")
    .env("TERM", "dumb")
    .env("UV_CACHE_DIR", dir.join("cache/uv"))
    .env("UV_TOOL_DIR", dir.join("data/uv-tools"))
    .env("UV_TOOL_BIN_DIR", dir.join("data/uv-bin"))
    .env("UV_PYTHON_INSTALL_DIR", dir.join("data/uv-python"))
    .env("UV_NO_BUILD", "1")
    .env("UV_PYTHON", "3.12")
    // A JVM agent (Junie) takes the home from the passwd entry, not $HOME, and wrote the real
    // ~/.junie on the first run of this audit.
    .env(
        "JAVA_TOOL_OPTIONS",
        format!("-Duser.home={}", dir.join("home").display()),
    )
    .env("RENO_AGENTS_DIR", work.join("agents"))
    .env("RENO_ACP_AUDIT_WORKER", agent_id)
    .env("RENO_ACP_AUDIT_INDEX", index)
    .env("RENO_ACP_AUDIT_RESULT", result)
    .env("RENO_ACP_AUDIT_DIR", &dir)
    .stdin(std::process::Stdio::null());
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    (cmd, dir)
}

/// Whether anything is still alive in the group `pgid` leads.
fn group_alive(pgid: u32) -> bool {
    // SAFETY: signal 0 only tests for existence.
    unsafe { libc::killpg(pgid as i32, 0) == 0 }
}

fn kill_group(pgid: u32) {
    // SAFETY: the group is the worker's own (it was started as its leader).
    unsafe { libc::killpg(pgid as i32, libc::SIGKILL) };
}

fn du_kib(path: &Path) -> u64 {
    std::process::Command::new("du")
        .arg("-sk")
        .arg(path)
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0)
}

/// One agent, in its worker; its result with what the orchestrator saw around it.
async fn run_worker(work: &Path, id: &str, index: &Path) -> Value {
    let result = work.join(format!("{id}.result.json"));
    let (mut cmd, dir) = worker_command(work, id, index, &result);
    let log = std::fs::File::create(dir.join("worker.log")).unwrap();
    cmd.stdout(log.try_clone().unwrap()).stderr(log);
    let started = Instant::now();
    let mut child = tokio::process::Command::from(cmd).spawn().unwrap();
    let pgid = child.id().unwrap();
    let waited = tokio::time::timeout(WORKER_TIMEOUT, child.wait()).await;
    let timed_out = waited.is_err();
    let survivors = group_alive(pgid) && !timed_out;
    kill_group(pgid);
    let _ = child.wait().await;
    let mut out: Value = std::fs::read(&result)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({ "id": id, "worker": "no result written" }));
    let shim_calls: Vec<String> = std::fs::read_to_string(dir.join("shim.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    out["orchestrator"] = json!({
        "worker_secs": secs(started),
        "worker_timed_out": timed_out,
        "worker_group_survivors_killed": survivors,
        "shim_calls": shim_calls,
        "install_dir_kib": du_kib(&work.join("agents").join(id)),
        "home_kib": du_kib(&dir.join("home")) + du_kib(&dir.join("cache")) + du_kib(&dir.join("data")) + du_kib(&dir.join("config")),
    });
    out
}

/// What `ps` shows under `needle` (a scratch path): processes left behind.
fn leftover_processes(needle: &str) -> Vec<String> {
    let ps = std::process::Command::new("ps")
        .args(["-axo", "pid,pgid,command"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|l| l.contains(needle))
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
#[ignore = "downloads and runs every agent of the live ACP registry (opt in: RENO_ACP_AUDIT_OUT)"]
async fn registry_audit() {
    let Some(out) = env_path("RENO_ACP_AUDIT_OUT") else {
        eprintln!("registry_audit: RENO_ACP_AUDIT_OUT isn't set; nothing to do");
        return;
    };
    let out = std::fs::canonicalize({
        std::fs::create_dir_all(&out).unwrap();
        &out
    })
    .unwrap();
    let work = out.join("work");
    std::fs::create_dir_all(work.join("agents")).unwrap();
    let index = out.join("registry.json");
    match env_path("RENO_ACP_AUDIT_INDEX") {
        Some(file) => {
            std::fs::copy(file, &index).unwrap();
        }
        None => {
            let (bytes, _) = reg::fetch_index(reg::INDEX_URL, None).await.unwrap();
            std::fs::write(&index, bytes.unwrap()).unwrap();
        }
    }
    let raw: Value = serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
    let ids: Vec<String> = raw["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["id"].as_str().map(str::to_owned))
        .collect();
    let only: Option<Vec<String>> = std::env::var("RENO_ACP_AUDIT_ONLY")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.split(',').map(|s| s.trim().to_owned()).collect());
    let results = out.join("results.jsonl");
    let mut lines = String::new();
    for id in ids
        .iter()
        .filter(|id| only.as_ref().is_none_or(|o| o.contains(id)))
    {
        eprintln!("audit: {id}");
        let row = run_worker(&work, id, &index).await;
        lines.push_str(&row.to_string());
        lines.push('\n');
        // One line per agent, appended as it ends: a crash keeps what was done.
        std::fs::write(&results, &lines).unwrap();
        eprintln!(
            "audit: {id} -> {}",
            row.pointer("/session/status")
                .and_then(Value::as_str)
                .unwrap_or("-")
        );
    }
    let left = leftover_processes(&work.display().to_string());
    std::fs::write(out.join("leftover-processes.txt"), left.join("\n")).unwrap();
    let total_kib: u64 = du_kib(&work);
    std::fs::write(out.join("total-kib.txt"), total_kib.to_string()).unwrap();
    eprintln!(
        "audit: {} agents; {} MiB on disk; {} process(es) left under the scratch folder",
        ids.len(),
        total_kib / 1024,
        left.len()
    );
    if std::env::var_os("RENO_ACP_AUDIT_KEEP").is_none() {
        std::fs::remove_dir_all(&work).unwrap();
    }
}

/// The canary: a fake agent in the same isolated world, dumping what it can see (its
/// environment, which opener programs resolve where) before answering the handshake.
#[tokio::test]
#[ignore = "opt in: RENO_ACP_AUDIT_OUT"]
async fn registry_audit_canary() {
    let Some(out) = env_path("RENO_ACP_AUDIT_OUT") else {
        return;
    };
    std::fs::create_dir_all(&out).unwrap();
    let out = std::fs::canonicalize(&out).unwrap();
    let work = out.join("canary-work");
    std::fs::create_dir_all(work.join("agents")).unwrap();
    let index = out.join("canary-index.json");
    std::fs::write(&index, "{}").unwrap();
    let row = run_worker(&work, "canary", &index).await;
    std::fs::write(
        out.join("canary.json"),
        serde_json::to_string_pretty(&row).unwrap(),
    )
    .unwrap();
    eprintln!("{}", serde_json::to_string_pretty(&row).unwrap());
    // The dump is kept for the reader; the scratch folder goes.
    let dump = work.join("canary").join("canary-env.txt");
    if dump.exists() {
        std::fs::copy(&dump, out.join("canary-env.txt")).unwrap();
    }
    std::fs::remove_dir_all(&work).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Worker: one agent, in the isolated world its orchestrator made.

const CANARY: &str = r#"#!/bin/sh
{
  env | sort
  echo "--- command -v"
  for c in open xdg-open osascript security launchctl node npm uv; do
    printf '%s=%s\n' "$c" "$(command -v $c)"
  done
  echo "--- cwd=$(pwd)"
} > "$CANARY_OUT"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"authMethods":[{"id":"key","name":"Key","type":"env_var","vars":[{"name":"X_API_KEY"}]}]}}\n' "$id";;
    *'"session/new"'*) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"Authentication required"}}\n' "$id";;
  esac
done
"#;

fn install_canary(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let agent = reg::agents_root().unwrap().join("canary");
    std::fs::create_dir_all(&agent).unwrap();
    let program = agent.join("canary.sh");
    std::fs::write(&program, CANARY).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let record = json!({
        "id": "canary", "name": "Canary", "version": "0.0.0", "kind": "binary",
        "program": program, "args": [], "env": { "CANARY_OUT": dir.join("canary-env.txt") },
        "verified": true,
    });
    std::fs::write(agent.join("agent.json"), record.to_string()).unwrap();
}

fn entry_for(raw: &Value) -> (Option<reg::Entry>, Vec<String>) {
    let one = json!({ "agents": [raw] });
    let entries = reg::parse_index(one.to_string().as_bytes()).unwrap_or_default();
    (entries.into_iter().next(), Vec::new())
}

fn static_facts(raw: &Value, entry: Option<&reg::Entry>) -> Value {
    let id = raw["id"].as_str().unwrap_or("?");
    let dist = &raw["distribution"];
    let platforms: Vec<&str> = dist["binary"]
        .as_object()
        .map(|b| b.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let sha256 = dist["binary"][reg::platform().unwrap_or("none")]["sha256"]
        .as_str()
        .map(|h| json!({ "present": true, "valid": h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) }))
        .unwrap_or(json!({ "present": false }));
    let distributions: Vec<&str> = ["binary", "npx", "uvx"]
        .into_iter()
        .filter(|k| !dist[*k].is_null())
        .collect();
    let mut facts = json!({
        "id": id,
        "version": raw["version"],
        "built_in": reg::built_in(id),
        "native_driver_coming": NATIVE_COMING.contains(&id),
        "platform": reg::platform(),
        "distributions": distributions,
        "binary_platforms": platforms,
        "parse_ok": entry.is_some(),
        "sha256": sha256,
    });
    if let Some(entry) = entry {
        facts["choice"] = json!(reg::choose(entry).map(|c| c.kind()));
        facts["readiness"] = json!(reg::readiness(entry));
        facts["checksum"] = json!(reg::checksum(entry));
        facts["package"] = json!(match reg::choose(entry) {
            Some(reg::Choice::Npx(p) | reg::Choice::Uvx(p)) => Some(p.package.clone()),
            Some(reg::Choice::Binary(t)) => Some(t.archive.clone()),
            None => None,
        });
    }
    facts
}

/// The agent's handshake, as Reno's registry path makes it: the executor's own start
/// (`initialize` with the sign-in client's capabilities), then `session/new`.
async fn handshake(agent: RegistryAgent, name: &str, dir: &Path) -> Value {
    let exec = match AcpExecutor::registry(agent, name) {
        Ok(exec) => exec.with_handshake_timeout(STEP_TIMEOUT),
        Err(e) => return json!({ "initialize": { "ok": false, "error": e.to_string() } }),
    };
    let stderr_path = dir.join("agent.stderr");
    let stderr = Stdio::from(std::fs::File::create(&stderr_path).unwrap());
    let t0 = Instant::now();
    let started = exec.start_registry_with(stderr).await;
    let (mut child, client, mut incoming, initialized) = match started {
        Ok(started) => started,
        Err(error) => {
            return json!({
                "initialize": { "ok": false, "secs": secs(t0), "error": error.to_string() },
                "stderr_tail": tail(&stderr_path),
            });
        }
    };
    let init = json!({
        "ok": true,
        "secs": secs(t0),
        "protocolVersion": initialized["protocolVersion"],
        "agentInfo": initialized["agentInfo"],
        "agentCapabilities": initialized["agentCapabilities"],
        "authMethods_raw": initialized["authMethods"],
        "authMethods_reno": auth_methods(&initialized),
        "can_log_out": initialized.pointer("/agentCapabilities/auth/logout").is_some(),
        "terminal_auth_meta": initialized.pointer("/agentCapabilities/_meta/terminal-auth"),
    });
    let home = std::env::var("HOME").unwrap();
    let cwd = Path::new(&home).join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let t1 = Instant::now();
    let session = tokio::time::timeout(
        STEP_TIMEOUT,
        request_draining(
            &client,
            &mut incoming,
            AccessMode::FullAccess,
            "session/new",
            json!({ "cwd": cwd, "mcpServers": [] }),
        ),
    )
    .await;
    let session = match session {
        Ok(Ok(started)) => json!({
            "status": "ok",
            "secs": secs(t1),
            "has_session_id": started["sessionId"].is_string(),
            "models": started.pointer("/models/availableModels").and_then(Value::as_array).map(Vec::len),
            "modes": started.pointer("/modes/availableModes").and_then(Value::as_array).map(Vec::len),
            "config_options": started["configOptions"].as_array().map(Vec::len),
        }),
        Ok(Err(error)) => {
            let text = error.to_string();
            let signed_out = still_signed_out(&error);
            json!({
                "status": if signed_out { "auth_required" } else { "error" },
                "secs": secs(t1),
                "reno_says_signed_out": signed_out,
                "error": text,
            })
        }
        Err(_) => json!({ "status": "timeout", "secs": secs(t1) }),
    };
    shutdown_child(&mut child, exec.kill_grace).await;
    json!({ "initialize": init, "session": session, "stderr_tail": tail(&stderr_path) })
}

fn tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let text = crate::redact::redact_output(&text);
    let cut = text.len().saturating_sub(6000);
    let start = (cut..=text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    text[start..].to_owned()
}

#[tokio::test]
#[ignore = "run by registry_audit, in an isolated environment"]
async fn registry_audit_worker() {
    let (Some(id), Some(index), Some(result), Some(dir)) = (
        std::env::var("RENO_ACP_AUDIT_WORKER").ok(),
        env_path("RENO_ACP_AUDIT_INDEX"),
        env_path("RENO_ACP_AUDIT_RESULT"),
        env_path("RENO_ACP_AUDIT_DIR"),
    ) else {
        return;
    };
    // Belt and braces: this is only ever the isolated world's.
    let home = std::env::var("HOME").unwrap();
    assert!(
        home.starts_with(dir.to_str().unwrap()),
        "{home} isn't the scratch home"
    );
    let row = if id == "canary" {
        install_canary(&dir);
        let mut row = handshake(RegistryAgent::new("canary").unwrap(), "Canary", &dir).await;
        row["id"] = json!("canary");
        row
    } else {
        audit_one(&id, &index, &dir).await
    };
    std::fs::write(result, serde_json::to_string_pretty(&row).unwrap()).unwrap();
}

async fn audit_one(id: &str, index: &Path, dir: &Path) -> Value {
    let raw: Value = serde_json::from_slice(&std::fs::read(index).unwrap()).unwrap();
    let raw = raw["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id)
        .unwrap()
        .clone();
    let (entry, _) = entry_for(&raw);
    let mut row = static_facts(&raw, entry.as_ref());
    row["id"] = json!(id);
    let Some(mut entry) = entry else {
        row["install"] = json!({ "ok": false, "error": "parse_index drops the entry" });
        return row;
    };
    if reg::choose(&entry).is_none() {
        row["install"] = json!({ "ok": false, "error": "no build for this device" });
        return row;
    }
    if reg::readiness(&entry) != reg::Readiness::Ready {
        row["install"] = json!({ "ok": false, "error": format!("not ready here: {:?}", reg::readiness(&entry)) });
        return row;
    }
    // Reno's own five are refused by install (they have native drivers): record that, then
    // install under an alias to see what the registry's build does. Off Reno's built-in path.
    if reg::built_in(id).is_some() {
        let refused = reg::install(&entry, None)
            .await
            .err()
            .map(|e| e.to_string());
        row["install_refusal"] = json!(refused);
        entry.id = format!("audit-{id}");
        row["installed_as"] = json!(entry.id);
    }
    let t0 = Instant::now();
    let installed = tokio::time::timeout(INSTALL_TIMEOUT, reg::install(&entry, None)).await;
    match installed {
        Ok(Ok(record)) => {
            row["install"] = json!({
                "ok": true,
                "secs": secs(t0),
                "kind": record.kind,
                "version": record.version,
                "verified": record.verified,
                "args": record.args,
                "env_names": record.env.keys().collect::<Vec<_>>(),
            });
        }
        Ok(Err(error)) => {
            row["install"] = json!({ "ok": false, "secs": secs(t0), "error": error.to_string() });
            return row;
        }
        Err(_) => {
            row["install"] = json!({ "ok": false, "secs": secs(t0), "error": "timeout (10 min)" });
            return row;
        }
    }
    let agent = RegistryAgent::new(&entry.id).unwrap();
    let launch = reg::launch(agent);
    row["launch"] = match &launch {
        Ok((program, args, env)) => json!({
            "ok": true, "program": program.file_name().map(|n| n.to_string_lossy().into_owned()),
            "args": args.iter().map(|a| a.replace(&dir.display().to_string(), "<scratch>")).collect::<Vec<_>>(),
            "env_names": env.iter().map(|(k, _)| k).collect::<Vec<_>>(),
        }),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    };
    if launch.is_err() {
        return row;
    }
    let shaken = handshake(agent, &entry.name, dir).await;
    for key in ["initialize", "session", "stderr_tail"] {
        row[key] = shaken[key].clone();
    }
    row
}
