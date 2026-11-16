//! What reaches an agent child's environment (T1.13): the allowlist, the run's own variables and
//! nothing else from the daemon. The fake Claude CLI writes its environment to a file.
//!
//! One test in its own binary: it sets variables on this process, which nothing else here reads.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use futures::StreamExt;
use reno_harness::ClaudeExecutor;
use reno_harness::exec::{ExecControls, Executor};
use reno_proto::{AccessMode, ExecEvent, ExecSpec, McpServerSpec, RunId, SandboxLevel};

fn fixture() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-claude.sh");
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    path
}

#[tokio::test]
async fn the_child_gets_the_allowlist_the_run_env_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("env.txt");
    // SAFETY: the only test of this binary; no other thread reads or writes the environment.
    unsafe {
        std::env::set_var("LEAKY_DAEMON_VAR", "leaked-value");
        std::env::set_var("GITHUB_TOKEN", "daemon-github-token");
        std::env::set_var("RENO_DATA_DIR", "/daemon/data");
        std::env::set_var("CODEX_HOME", "/not/for/claude");
        std::env::set_var("OPENAI_API_KEY", "not-for-claude");
        std::env::set_var("CLAUDE_CONFIG_DIR", "/claude/config");
        std::env::set_var("ANTHROPIC_API_KEY", "anthropic-key");
        std::env::set_var("LC_MESSAGES", "C");
    }
    let env: BTreeMap<String, String> = [
        ("RENO_FAKE_ENV_OUT".to_owned(), out.display().to_string()),
        ("RENO_RUN_ID".to_owned(), "run_01HZX9".to_owned()),
        ("BOT_SECRET".to_owned(), "bot-secret-value".to_owned()),
    ]
    .into();
    let spec = ExecSpec {
        run_id: RunId::from_suffix("01HZX9").unwrap(),
        prompt: "scenario:env".into(),
        cwd: std::env::temp_dir(),
        model: None,
        effort: None,
        access: AccessMode::FullAccess,
        sandbox: SandboxLevel::Unrestricted,
        resume: None,
        // Reno's own MCP server: its variables travel in the server's own env, in the config the
        // CLI starts it from, not through the CLI's environment.
        mcp: vec![McpServerSpec {
            name: "reno".into(),
            command: "reno".into(),
            args: vec!["mcp".into()],
            env: [
                ("RENO_SOCKET", "/data/reno.sock"),
                ("RENO_RUN_ID", "run_01HZX9"),
                ("RENO_TOKEN_FILE", "/data/runs/run_01HZX9/token"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        }],
        env,
        output_schema: None,
        instructions: None,
    };
    let executor = ClaudeExecutor::new().with_executable(fixture());
    let (controls, _handle) = ExecControls::new(8);
    let mut stream = executor.start(spec, controls).await.unwrap();
    let mut done = false;
    while let Some(event) = stream.next().await {
        done |= matches!(event, ExecEvent::Done { .. });
    }
    assert!(done);

    let printed = std::fs::read_to_string(&out).unwrap();
    let seen: BTreeMap<&str, &str> = printed
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    for dropped in [
        "LEAKY_DAEMON_VAR",
        "GITHUB_TOKEN",
        "RENO_DATA_DIR",
        "CODEX_HOME",
        "OPENAI_API_KEY",
    ] {
        assert!(
            !seen.contains_key(dropped),
            "{dropped} reached the child: {:?}",
            seen.keys().collect::<Vec<_>>()
        );
    }
    // The driver's own sign-in variables, the base set and the run's variables arrive.
    assert_eq!(seen["CLAUDE_CONFIG_DIR"], "/claude/config");
    assert_eq!(seen["ANTHROPIC_API_KEY"], "anthropic-key");
    assert_eq!(seen["LC_MESSAGES"], "C");
    assert!(seen.contains_key("HOME"), "HOME is in the base set");
    assert!(seen.contains_key("PATH"), "PATH is composed for the child");
    assert_eq!(seen["RENO_RUN_ID"], "run_01HZX9");
    assert_eq!(seen["BOT_SECRET"], "bot-secret-value");
    assert_eq!(seen["CLAUDE_CODE_ENTRYPOINT"], "reno");

    // The injected `reno mcp` server gets its three variables from its own config entry, so the
    // allowlist on the CLI's environment does not touch them; and the CLI itself is not given
    // the socket or token file.
    let server = &mcp_config(&out)["mcpServers"]["reno"];
    assert_eq!(server["env"]["RENO_SOCKET"], "/data/reno.sock");
    assert_eq!(server["env"]["RENO_RUN_ID"], "run_01HZX9");
    assert_eq!(
        server["env"]["RENO_TOKEN_FILE"],
        "/data/runs/run_01HZX9/token"
    );
    assert!(!seen.contains_key("RENO_SOCKET") && !seen.contains_key("RENO_TOKEN_FILE"));
}

/// Reads the `--mcp-config` the fake CLI was started with (the one the real CLI starts its MCP
/// servers from) out of its recorded arguments.
fn mcp_config(out: &std::path::Path) -> serde_json::Value {
    let args = std::fs::read_to_string(format!("{}.args", out.display())).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    let at = lines.iter().position(|l| *l == "--mcp-config").unwrap();
    serde_json::from_str(lines[at + 1]).unwrap()
}
