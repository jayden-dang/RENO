//! `tests/fixtures/fake-acp-tool.py`: a fake ACP agent whose turn starts the MCP servers it was
//! handed in `session/new` and calls one tool. Checked here against a tiny stub MCP server, with
//! Reno's own server first in `ExecSpec::mcp` and a plugin's after it.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use reno_harness::AcpExecutor;
use reno_harness::exec::{ExecControls, Executor};
use reno_proto::{AccessMode, ExecEvent, ExecSpec, McpServerSpec, RunId, SandboxLevel, StopReason};

/// A stdio MCP server offering the one tool named by `$STUB_TOOL`. Its call
/// result echoes the arguments and the server's `$STUB_LABEL`; a call with
/// `{"fail": true}` is an `isError` result.
const STUB_MCP: &str = r#"#!/usr/bin/env python3
import json, os, sys
tool = os.environ["STUB_TOOL"]
for line in sys.stdin:
    msg = json.loads(line)
    ident, method = msg.get("id"), msg.get("method")
    if ident is None:
        continue
    if method == "initialize":
        result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "stub", "version": "1"}}
        # A server notification in the middle must be skipped.
        print(json.dumps({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}), flush=True)
    elif method == "tools/list":
        result = {"tools": [{"name": tool, "description": "stub", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        args = msg["params"].get("arguments", {})
        value = {"label": os.environ.get("STUB_LABEL"), "args": args}
        result = {"content": [{"type": "text", "text": json.dumps(value)}],
                  "structuredContent": value, "isError": bool(args.get("fail"))}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": ident, "error": {"code": -32601, "message": method}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": ident, "result": result}), flush=True)
"#;

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-acp-tool.py");
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    path
}

fn stub(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("stub-mcp.py");
    std::fs::write(&path, STUB_MCP).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn server(name: &str, command: &Path, tool: &str) -> McpServerSpec {
    McpServerSpec {
        name: name.into(),
        command: command.display().to_string(),
        args: vec![],
        env: [
            ("STUB_TOOL".to_owned(), tool.to_owned()),
            ("STUB_LABEL".to_owned(), name.to_owned()),
        ]
        .into_iter()
        .collect(),
    }
}

fn spec(prompt: &str, cwd: &Path, env: &[(&str, &str)], mcp: Vec<McpServerSpec>) -> ExecSpec {
    ExecSpec {
        run_id: RunId::from_suffix("01HZX9").unwrap(),
        prompt: prompt.into(),
        cwd: cwd.to_path_buf(),
        model: None,
        effort: None,
        access: AccessMode::FullAccess,
        sandbox: SandboxLevel::Unrestricted,
        resume: None,
        mcp,
        env: env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        output_schema: None,
        instructions: None,
    }
}

/// The turn's reply text, once it completes.
async fn reply(spec: ExecSpec) -> String {
    let (controls, _handle) = ExecControls::new(8);
    let executor = AcpExecutor::grok().with_executable(fixture_path());
    let mut stream = executor.start(spec, controls).await.expect("run starts");
    let mut text = String::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(event) = stream.next().await {
            match event {
                ExecEvent::Text { text: delta } => text.push_str(&delta),
                ExecEvent::Done {
                    stop, diagnostic, ..
                } => {
                    assert_eq!(stop, StopReason::Completed, "{diagnostic:?}");
                    return;
                }
                _ => {}
            }
        }
        panic!("stream ended before Done");
    })
    .await
    .expect("turn finished in time");
    text
}

#[tokio::test]
async fn calls_the_named_tool_on_the_server_that_lists_it() {
    let dir = tempfile::tempdir().unwrap();
    let stub = stub(dir.path());
    // Reno's own server does not list the tool; the plugin's does.
    let spec = spec(
        "hello",
        dir.path(),
        &[
            ("FAKE_ACP_CALL_TOOL", "focus__count_words"),
            ("FAKE_ACP_CALL_ARGS", r#"{"text":"a b c"}"#),
        ],
        vec![
            server("reno", &stub, "whoami"),
            server("plugin-focus", &stub, "focus__count_words"),
        ],
    );
    let text = reply(spec).await;
    let result: serde_json::Value = serde_json::from_str(&text).expect(&text);
    assert_eq!(result["isError"], false);
    assert_eq!(result["structuredContent"]["label"], "plugin-focus");
    assert_eq!(
        result["structuredContent"]["args"],
        serde_json::json!({ "text": "a b c" })
    );
}

#[tokio::test]
async fn a_prompt_can_name_the_tool_and_a_pinned_server() {
    let dir = tempfile::tempdir().unwrap();
    let stub = stub(dir.path());
    let spec_for = |prompt: &str, env: &[(&str, &str)]| {
        spec(
            prompt,
            dir.path(),
            env,
            vec![server("reno", &stub, "whoami")],
        )
    };
    let text = reply(spec_for(
        r#"call-tool whoami {"fail": true}"#,
        &[("FAKE_ACP_MCP_SERVER", "reno")],
    ))
    .await;
    let result: serde_json::Value = serde_json::from_str(&text).expect(&text);
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["label"], "reno");

    assert_eq!(
        reply(spec_for("call-tool nope", &[])).await,
        "fake-acp-tool: no MCP server lists nope"
    );
}
