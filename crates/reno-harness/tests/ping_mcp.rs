//! The MCP server the live conformance run injects speaks enough MCP for a CLI to list and call
//! its one tool, and the call leaves the marker the run asserts on. No model, no network.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

struct Peer {
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl Peer {
    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn call(&mut self, request: &Value) -> Value {
        self.send(request);
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}

#[test]
fn it_lists_one_tool_and_a_call_writes_the_marker() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker.txt");
    let mut child = Command::new(env!("CARGO_BIN_EXE_conformance-ping-mcp"))
        .arg(&marker)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut peer = Peer {
        stdin: child.stdin.take().unwrap(),
        stdout: BufReader::new(child.stdout.take().unwrap()),
    };
    let mut call = |request: Value| peer.call(&request);

    let init = call(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                   "clientInfo": {"name": "t", "version": "0"}}}));
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    // A notification gets no answer: the next reply is the next request's.
    drop(call);
    peer.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let mut call = |request: Value| peer.call(&request);

    let list = call(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "ping");
    assert!(!marker.exists(), "listing leaves no marker");

    let wrong = call(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "other", "arguments": {}}}));
    assert!(wrong["error"].is_object());
    assert!(!marker.exists());

    let done = call(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "ping", "arguments": {}}}));
    assert_eq!(done["result"]["isError"], false);
    assert_eq!(done["result"]["content"][0]["text"], "pong");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "pong\n");

    let unknown = call(json!({"jsonrpc": "2.0", "id": 5, "method": "nope"}));
    assert_eq!(unknown["error"]["code"], -32601);

    drop(call);
    drop(peer);
    assert!(child.wait().unwrap().success());
}
