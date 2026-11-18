//! `ExecSpec::env` (host-resolved credentials on a Cloud device) reaches the agent process each
//! native driver spawns.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reno_harness::exec::{ExecControls, Executor};
use reno_harness::{ClaudeExecutor, CodexExecutor};
use reno_proto::{AccessMode, ExecSpec, RunId, SandboxLevel};

/// A stand-in agent CLI: records the env it was started with into its cwd
/// (atomically, so the reader never sees a partial file) and exits.
fn env_dumper(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("agent.sh");
    std::fs::write(
        &path,
        "#!/bin/sh\nprintf 'CODEX_HOME=%s\\nGH_TOKEN=%s\\nANTHROPIC_API_KEY=%s\\n' \
         \"$CODEX_HOME\" \"$GH_TOKEN\" \"$ANTHROPIC_API_KEY\" > env.tmp\n\
         mv env.tmp env.txt\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn run_env() -> BTreeMap<String, String> {
    [
        ("CODEX_HOME".to_string(), "/managed/codex-home".to_string()),
        ("GH_TOKEN".to_string(), "ghu_test".to_string()),
        ("ANTHROPIC_API_KEY".to_string(), "sk-ant-test".to_string()),
    ]
    .into_iter()
    .collect()
}

fn spec(cwd: &Path) -> ExecSpec {
    ExecSpec {
        model_options: Default::default(),
        run_id: RunId::from_suffix("01HZX9").unwrap(),
        inherit_user_config: false,
        prompt: "hello".into(),
        cwd: cwd.to_path_buf(),
        model: None,
        effort: None,
        access: AccessMode::FullAccess,
        sandbox: SandboxLevel::Unrestricted,
        resume: None,
        mcp: Vec::new(),
        env: run_env(),
        output_schema: None,
        instructions: None,
    }
}

/// Start a run on `executor` and read back what the child it spawned was started with.
async fn recorded_env(executor: &dyn Executor, dir: &Path) -> String {
    let (controls, _handle) = ExecControls::new(1);
    let run = executor.start(spec(dir), controls).await;
    let file = dir.join("env.txt");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !file.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the agent process was spawned");
    drop(run);
    tokio::time::sleep(Duration::from_millis(50)).await;
    std::fs::read_to_string(file).unwrap()
}

#[tokio::test]
async fn codex_child_receives_the_run_env() {
    let dir = tempfile::tempdir().unwrap();
    let executor = CodexExecutor::new()
        .with_executable(env_dumper(dir.path()))
        .with_graces(Duration::from_millis(20), Duration::from_millis(100));
    let env = recorded_env(&executor, dir.path()).await;
    assert!(env.contains("CODEX_HOME=/managed/codex-home"), "{env}");
    assert!(env.contains("GH_TOKEN=ghu_test"), "{env}");
}

#[tokio::test]
async fn claude_child_receives_the_run_env() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ClaudeExecutor::new()
        .with_executable(env_dumper(dir.path()))
        .with_graces(Duration::from_millis(20), Duration::from_millis(100));
    let env = recorded_env(&executor, dir.path()).await;
    assert!(env.contains("ANTHROPIC_API_KEY=sk-ant-test"), "{env}");
    assert!(env.contains("GH_TOKEN=ghu_test"), "{env}");
}
