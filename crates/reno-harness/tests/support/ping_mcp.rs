//! A tiny MCP server for the live conformance run: stdio, newline-delimited JSON-RPC, one tool
//! `ping`. Calling it writes `pong` to the marker file named by its first argument, which is how
//! the run proves a real model reached a server Reno injected. It never touches the network and
//! needs no daemon.
//!
//! Only what a CLI needs to list and call one tool is spoken: `initialize`, `ping`,
//! `tools/list` and `tools/call`. Anything else with an id is answered "method not found".

use std::io::{BufRead, Write};

use serde_json::{Value, json};

const TOOL: &str = "ping";

fn reply(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn failure(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The answer to one request, or `None` for a notification.
fn handle(request: &Value, marker: &str) -> Option<Value> {
    let id = request.get("id")?;
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    Some(match method {
        "initialize" => {
            // Speak the version the client asked for: the tool surface is the same in all.
            let version = request
                .pointer("/params/protocolVersion")
                .cloned()
                .unwrap_or_else(|| json!("2025-06-18"));
            reply(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "reno-conformance-ping", "version": "0.0.0"},
                }),
            )
        }
        "ping" => reply(id, json!({})),
        "tools/list" => reply(
            id,
            json!({"tools": [{
                "name": TOOL,
                "description": "Records that it was called. Takes no arguments and answers pong.",
                "inputSchema": {"type": "object", "properties": {}},
            }]}),
        ),
        "tools/call" => {
            let name = request.pointer("/params/name").and_then(Value::as_str);
            if name != Some(TOOL) {
                return Some(failure(id, -32602, "unknown tool"));
            }
            match std::fs::write(marker, "pong\n") {
                Ok(()) => reply(
                    id,
                    json!({"content": [{"type": "text", "text": "pong"}], "isError": false}),
                ),
                Err(error) => reply(
                    id,
                    json!({"content": [{"type": "text", "text": error.to_string()}], "isError": true}),
                ),
            }
        }
        _ => failure(id, -32601, "method not found"),
    })
}

fn main() {
    let Some(marker) = std::env::args().nth(1) else {
        let _ = writeln!(
            std::io::stderr(),
            "usage: conformance-ping-mcp <marker file>"
        );
        std::process::exit(2);
    };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(response) = handle(&request, &marker)
            && (writeln!(stdout, "{response}").is_err() || stdout.flush().is_err())
        {
            break;
        }
    }
}
