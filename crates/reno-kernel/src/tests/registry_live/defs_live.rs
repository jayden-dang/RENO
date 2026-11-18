//! Definitions see the agents added and removed while the daemon runs: a bot naming an agent that
//! is not registered is invalid, adding the agent makes it valid with no restart, and removing it
//! makes the bot invalid again (its file and the set in force are left alone).

use std::sync::Arc;

use reno_proto::defs::{DefFile, DefKind};

use super::{World, id, until};
use crate::tests::defs::{FakePort, file};

fn bot_naming(name: &str, executor: &str) -> DefFile {
    file(
        DefKind::Bot,
        name,
        &format!("name = \"{name}\"\ninstructions = \"go\"\nexecutors = [\"{executor}\"]\n"),
    )
}

fn def_errors(w: &World) -> Vec<String> {
    w.fx.kernel
        .current_defs()
        .errors()
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_bot_naming_an_agent_that_is_not_registered_stays_invalid() {
    let port = FakePort::with(vec![bot_naming("keeper", "acp.demo")]);
    let w = World::with_defs(&port).await;
    assert_eq!(
        def_errors(&w),
        ["bots/keeper.toml: executors[0]: unknown executor \"acp.demo\""]
    );
    assert!(w.fx.kernel.current_defs().set().bot("keeper").is_none());
}

#[tokio::test(start_paused = true)]
async fn adding_the_agent_makes_the_bot_valid_without_a_restart() {
    let port = FakePort::with(vec![bot_naming("keeper", "acp.demo")]);
    let w = World::with_defs(&port).await;
    w.index.list("demo", "1.0.0");
    assert!(!def_errors(&w).is_empty());
    w.fx.kernel
        .add_registry_executor(Arc::new(w.agent("acp.demo")))
        .await
        .unwrap();
    until(async || w.fx.kernel.current_defs().set().bot("keeper").map(|_| ())).await;
    assert!(def_errors(&w).is_empty(), "{:?}", def_errors(&w));
}

#[tokio::test(start_paused = true)]
async fn removing_an_agent_a_bot_names_makes_the_bot_invalid_and_keeps_the_file() {
    let port = FakePort::with(vec![bot_naming("keeper", "acp.demo")]);
    let w = World::with_defs(&port).await;
    w.index.list("demo", "1.0.0");
    w.fx.kernel
        .add_registry_executor(Arc::new(w.agent("acp.demo")))
        .await
        .unwrap();
    until(async || {
        let i = w.info("acp.demo")?;
        (i.job.is_none() && w.fx.kernel.current_defs().set().bot("keeper").is_some()).then_some(())
    })
    .await;
    w.fx.kernel
        .remove_registry_executor(&id("acp.demo"))
        .await
        .unwrap();
    let errors = until(async || {
        let errors = def_errors(&w);
        (!errors.is_empty()).then_some(errors)
    })
    .await;
    assert!(
        errors[0].starts_with("bots/keeper.toml: executors[0]: unknown executor \"acp.demo\""),
        "{errors:?}"
    );
    assert!(
        errors[0].contains("was removed") && errors[0].contains("add it again"),
        "it says why: {errors:?}"
    );
    // The bot's file is untouched; the bot is not deleted.
    assert!(port.snapshot_files().iter().any(|f| f.name == "keeper"));
    assert!(w.fx.kernel.current_defs().set().bot("keeper").is_some());
}
