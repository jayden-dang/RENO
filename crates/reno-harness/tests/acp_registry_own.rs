//! An agent of your own: added from a command, run through the generic ACP harness, never
//! installed or updated by Reno, with its environment kept owner-only in Reno's agents root.

// Tests unwrap freely and set environment variables.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
#![cfg(unix)]

use reno_harness::RegistryAgent;
use reno_harness::acp_registry as reg;

#[test]
fn an_own_agent_is_added_from_a_command_with_its_env_and_removed() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    // SAFETY: this file's only test; nothing else reads the environment.
    unsafe { std::env::set_var("RENO_AGENTS_DIR", root.path()) };

    let record = reg::add_own("My Agent", "/bin/sh -c 'exec cat'", "TOKEN=abc\nMODE=fast\n").unwrap();
    assert_eq!(record.id, "own.my-agent");
    assert_eq!(record.kind, reg::Kind::Own);
    assert_eq!(record.args, ["-c", "exec cat"]);
    let agent = RegistryAgent::new("own.my-agent").unwrap();
    assert!(reg::installed(agent));

    // It launches as typed, with its own environment.
    let (program, args, env) = reg::launch(agent).unwrap();
    assert_eq!(program, std::path::PathBuf::from("/bin/sh"));
    assert_eq!(args, ["-c", "exec cat"]);
    assert!(env.contains(&("TOKEN".to_owned(), "abc".to_owned())));

    // The environment is owner-only and replaced whole.
    let file = root.path().join("own.my-agent/env");
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    reg::set_user_env(agent, "ONLY=1\n").unwrap();
    assert_eq!(reg::env_text(&reg::user_env(agent)), "ONLY=1\n");
    // A bad line is refused without repeating it.
    let error = reg::set_user_env(agent, "TOKEN=secret-value\nnot a pair").unwrap_err();
    assert!(!error.to_string().contains("secret-value"), "{error}");
    assert_eq!(reg::env_text(&reg::user_env(agent)), "ONLY=1\n", "left as it was");

    // It is listed as one of your own, and a second with the same name is refused.
    let view = reg::record_view(&reg::record(agent).unwrap()).unwrap();
    assert!(view.own && !view.delisted);
    assert!(reg::add_own("my agent", "/bin/sh", "").is_err());
    // A program that is not there is refused, naming it.
    let error = reg::add_own("Ghost", "/no/such/program --acp", "").unwrap_err();
    assert!(error.to_string().contains("/no/such/program"), "{error}");
    assert!(reg::add_own("x", "a | b", "").is_err());

    reg::remove(agent).unwrap();
    assert!(!reg::installed(agent));
    assert!(!root.path().join("own.my-agent").exists());
}
