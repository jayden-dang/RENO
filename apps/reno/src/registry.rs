//! `reno executor registry | add | remove | env`: browse the ACP registry, add and remove its
//! agents, and keep an agent's own environment. The functions take a connected client and return
//! what to print, so they run against the fake daemon in tests; `commands.rs` prints it.
//!
//! Human output is lines; `--json` is the daemon's own answer, whole. An agent's environment is
//! shown with its values hidden unless `--reveal` is given: a terminal's scrollback is no place for
//! a key.

use std::collections::BTreeMap;

use futures::StreamExt;
use reno_client::Client;
use reno_proto::{
    ExecutorId, ExecutorInfo, ExecutorRegistryListResult, InstallStatus, JobPhase, RegistryEntry,
    RegistryReadiness,
};

/// What a command prints.
pub(crate) enum Output {
    Json(serde_json::Value),
    Lines(Vec<String>),
}

fn json(value: &impl serde::Serialize) -> anyhow::Result<Output> {
    Ok(Output::Json(serde_json::to_value(value)?))
}

fn fail(error: reno_client::ClientError) -> anyhow::Error {
    match error {
        reno_client::ClientError::Remote(object) => anyhow::anyhow!("{}", object.message),
        other => anyhow::anyhow!("{other}"),
    }
}

/// `reno executor registry [--all] [--refresh]`.
pub(crate) async fn list(
    client: &Client,
    all: bool,
    refresh: bool,
    as_json: bool,
) -> anyhow::Result<Output> {
    let mut listed = client.registry_list(refresh).await.map_err(fail)?;
    if !all {
        listed.entries.retain(|entry| entry.built_in.is_none());
    }
    if as_json {
        return json(&listed);
    }
    Ok(Output::Lines(list_lines(&listed, all)))
}

/// The list as lines: one agent a line, the state last.
pub(crate) fn list_lines(listed: &ExecutorRegistryListResult, all: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if listed.offline {
        lines.push(format!(
            "offline: this is the copy fetched at {} ({})",
            reno_proto::defs::format_instant(
                i64::try_from(listed.fetched_at_ms).unwrap_or(0),
                None
            ),
            listed
                .error
                .as_deref()
                .unwrap_or("the registry did not answer")
        ));
    }
    if listed.entries.is_empty() {
        lines.push(if all {
            "the registry lists no agents".to_owned()
        } else {
            "no agents to add".to_owned()
        });
        return lines;
    }
    let width = listed.entries.iter().map(|e| e.id.len()).max().unwrap_or(0);
    for entry in &listed.entries {
        lines.push(entry_line(entry, width));
    }
    lines
}

fn entry_line(entry: &RegistryEntry, width: usize) -> String {
    let kind = entry.kind.map_or("-", |kind| match kind {
        reno_proto::RegistryDistribution::Binary => "binary",
        reno_proto::RegistryDistribution::Npx => "npx",
        reno_proto::RegistryDistribution::Uvx => "uvx",
    });
    let mut what = format!("{:<width$}  {:<8}  {kind:<6}", entry.id, entry.version);
    let source = match (&entry.host, entry.checksum) {
        (Some(host), Some(true)) => format!("{host}, checksum ok"),
        (Some(host), Some(false)) => format!("{host}, NO CHECKSUM"),
        (Some(host), None) => host.clone(),
        (None, _) => String::new(),
    };
    what.push_str(&format!("  {source:<30}"));
    let state = if let Some(built_in) = &entry.built_in {
        format!("built in: use {built_in}")
    } else if entry.adding {
        "adding".to_owned()
    } else if entry.own {
        "your own".to_owned()
    } else if entry.delisted {
        let version = entry.added.as_ref().map_or("", |a| a.version.as_str());
        format!("added {version}; no longer in the registry")
    } else if let Some(added) = &entry.added {
        if added.update_available {
            format!(
                "added {}; update to {} available",
                added.version, entry.version
            )
        } else {
            format!("added {}", added.version)
        }
    } else {
        match entry.readiness {
            RegistryReadiness::Ready => "ready to add".to_owned(),
            RegistryReadiness::NeedsNode => format!(
                "needs Node.js: {}",
                entry.fix.as_deref().unwrap_or("install Node.js")
            ),
            RegistryReadiness::NeedsUv => {
                format!("needs uv: {}", entry.fix.as_deref().unwrap_or("install uv"))
            }
            RegistryReadiness::NoBuild => "no build for this Mac".to_owned(),
        }
    };
    format!("{what}  {state}").trim_end().to_owned()
}

/// `reno executor add <id>`: starts the job and follows it. `say` gets each progress line as it
/// happens; the result is the executor once the job ended, or the failure.
pub(crate) async fn add(
    client: &Client,
    id: &str,
    mut say: impl FnMut(String),
) -> anyhow::Result<ExecutorInfo> {
    let started = client.registry_add(id).await.map_err(fail)?;
    follow(client, started, &mut say).await
}

/// `reno executor add --own <name> -- <command...>`: registered and probed by the daemon.
pub(crate) async fn add_own(
    client: &Client,
    name: &str,
    command: &[String],
    env: &[String],
) -> anyhow::Result<ExecutorInfo> {
    let command = command
        .iter()
        .map(|word| shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ");
    let mut lines = String::new();
    for pair in env {
        if !pair.contains('=') {
            anyhow::bail!("--env takes KEY=VALUE");
        }
        lines.push_str(pair);
        lines.push('\n');
    }
    client
        .registry_add_own(name, &command, &lines)
        .await
        .map_err(fail)
}

/// A word the daemon's command splitter reads back whole.
fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// Follows the install job of `info` through `executors.watch` until it ends.
async fn follow(
    client: &Client,
    info: ExecutorInfo,
    say: &mut impl FnMut(String),
) -> anyhow::Result<ExecutorInfo> {
    let id = info.descriptor.id.clone();
    let mut stream = client.watch_executors().await.map_err(fail)?;
    let mut shown: Option<String> = None;
    let mut say_once = |line: String| {
        if shown.as_deref() != Some(&line) {
            say(line.clone());
            shown = Some(line);
        }
    };
    while let Some(snapshot) = stream.next().await {
        let snapshot = snapshot.map_err(fail)?;
        let Some(now) = snapshot.executors.iter().find(|e| e.descriptor.id == id) else {
            anyhow::bail!("{id} is no longer registered");
        };
        match now.job.as_ref().map(|job| &job.phase) {
            Some(JobPhase::Queued { waiting_runs }) => {
                say_once(format!("waiting for {waiting_runs} run(s) on {id} to end"));
            }
            Some(JobPhase::Running {
                step,
                steps,
                last_line,
                ..
            }) => say_once(match last_line {
                Some(line) => format!("[{step}/{steps}] {line}"),
                None => format!("[{step}/{steps}] working"),
            }),
            Some(JobPhase::Failed { detail, .. }) => {
                anyhow::bail!("{id}: {detail}");
            }
            None => {
                return match now.health.as_ref().map(|h| &h.installed) {
                    Some(InstallStatus::Installed { .. }) => Ok(now.clone()),
                    _ => anyhow::bail!("{id} did not install"),
                };
            }
        }
    }
    anyhow::bail!("the daemon stopped telling about {id}")
}

/// `reno executor remove <id>`.
pub(crate) async fn remove(client: &Client, id: &str) -> anyhow::Result<ExecutorId> {
    client.registry_remove(id).await.map_err(fail)
}

/// `reno executor env <id> [--set K=V ...] [--unset K ...] [--reveal]`.
pub(crate) async fn env(
    client: &Client,
    id: &str,
    set: &[String],
    unset: &[String],
    reveal: bool,
    as_json: bool,
) -> anyhow::Result<Output> {
    let mut text = client.registry_env_get(id).await.map_err(fail)?;
    if !set.is_empty() || !unset.is_empty() {
        let mut pairs = parse_pairs(&text);
        for pair in set {
            let (key, value) = pair
                .split_once('=')
                .filter(|(key, _)| !key.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("--set takes KEY=VALUE"))?;
            pairs.insert(key.trim().to_owned(), value.to_owned());
        }
        for key in unset {
            pairs.remove(key.as_str());
        }
        text = pairs.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        client.registry_env_set(id, &text).await.map_err(fail)?;
    }
    let shown: BTreeMap<String, String> = parse_pairs(&text)
        .into_iter()
        .map(|(key, value)| (key, if reveal { value } else { "***".to_owned() }))
        .collect();
    if as_json {
        return json(&serde_json::json!({ "id": id, "env": shown }));
    }
    if shown.is_empty() {
        return Ok(Output::Lines(vec![format!("{id} has no environment set")]));
    }
    Ok(Output::Lines(
        shown.iter().map(|(k, v)| format!("{k}={v}")).collect(),
    ))
}

fn parse_pairs(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.to_owned()))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_client::fake::{FakeConfig, FakeDaemon};

    use super::*;

    async fn daemon() -> (FakeDaemon, Client) {
        let daemon = FakeDaemon::start(FakeConfig {
            registry_agents: true,
            ..FakeConfig::default()
        })
        .unwrap();
        let client = daemon.connect().await.unwrap();
        (daemon, client)
    }

    fn lines(output: Output) -> Vec<String> {
        match output {
            Output::Lines(lines) => lines,
            Output::Json(value) => panic!("lines expected, got {value}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_list_hides_what_is_built_in_unless_asked() {
        let (_daemon, client) = daemon().await;
        let shown = lines(list(&client, false, false, false).await.unwrap());
        let text = shown.join("\n");
        assert!(
            text.contains("goose") && text.contains("fast-agent"),
            "{text}"
        );
        assert!(
            !text.contains("claude-acp") && !text.contains("codex-acp"),
            "{text}"
        );
        let all = lines(list(&client, true, false, false).await.unwrap()).join("\n");
        assert!(
            all.contains("claude-acp") && all.contains("built in: use claude-code"),
            "{all}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn each_state_reads_on_its_line() {
        let (_daemon, client) = daemon().await;
        let shown = lines(list(&client, true, false, false).await.unwrap());
        let line = |id: &str| {
            shown
                .iter()
                .find(|l| l.starts_with(id))
                .unwrap_or_else(|| panic!("{id} in {shown:?}"))
                .clone()
        };
        assert!(
            line("goose").contains("github.com, checksum ok"),
            "{}",
            line("goose")
        );
        assert!(line("goose").ends_with("ready to add"));
        assert!(line("vtcode").contains("NO CHECKSUM"), "{}", line("vtcode"));
        assert!(line("fast-agent").ends_with("needs uv: brew install uv"));
        assert!(line("gemini").ends_with("needs Node.js: brew install node"));
        assert!(line("windows-only").ends_with("no build for this Mac"));
        assert!(line("cline").ends_with("added 1.0.0; update to 1.1.0 available"));
        assert!(line("amp-acp").ends_with("added 0.7.0"));
        assert!(line("old-agent").ends_with("no longer in the registry"));
        assert!(line("own.mine").ends_with("your own"));
    }

    #[tokio::test(start_paused = true)]
    async fn json_is_the_daemons_answer_and_offline_is_said() {
        let (daemon, client) = daemon().await;
        match list(&client, false, false, true).await.unwrap() {
            Output::Json(value) => {
                let entries = value["entries"].as_array().unwrap();
                assert!(entries.iter().all(|e| e.get("builtIn").is_none()));
                assert!(entries.iter().any(|e| e["id"] == "goose"));
            }
            Output::Lines(_) => panic!("json expected"),
        }
        daemon.set_registry_offline(true);
        let shown = lines(list(&client, false, true, false).await.unwrap());
        assert!(
            shown[0].starts_with("offline: this is the copy fetched at"),
            "{shown:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn add_follows_the_job_to_the_installed_agent() {
        let (_daemon, client) = daemon().await;
        let mut said = Vec::new();
        let info = add(&client, "goose", |line| said.push(line)).await.unwrap();
        assert_eq!(info.descriptor.id.as_str(), "acp.goose");
        assert!(matches!(
            info.health.unwrap().installed,
            InstallStatus::Installed { .. }
        ));
        assert!(said.iter().any(|l| l.starts_with("[1/1]")), "{said:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_add_says_why() {
        let (daemon, client) = daemon().await;
        daemon.fail_next_job(reno_proto::JobFailure::ExitCode, "the download broke");
        let error = add(&client, "goose", |_| {}).await.unwrap_err();
        assert!(error.to_string().contains("the download broke"), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn errors_name_the_fix() {
        let (_daemon, client) = daemon().await;
        let uv = add(&client, "fast-agent", |_| {}).await.unwrap_err();
        assert_eq!(uv.to_string(), "fast-agent needs uv: brew install uv");
        let node = add(&client, "gemini", |_| {}).await.unwrap_err();
        assert!(
            node.to_string()
                .contains("needs Node.js: brew install node"),
            "{node}"
        );
        let built_in = add(&client, "claude-acp", |_| {}).await.unwrap_err();
        assert!(
            built_in.to_string().contains("built in: use claude-code"),
            "{built_in}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn remove_unregisters_and_own_agents_take_a_quoted_command() {
        let (_daemon, client) = daemon().await;
        let removed = remove(&client, "amp-acp").await.unwrap();
        assert_eq!(removed.as_str(), "acp.amp-acp");
        assert!(remove(&client, "amp-acp").await.is_err());
        let own = add_own(
            &client,
            "Mine Too",
            &["my agent".to_owned(), "--acp".to_owned()],
            &["K=v".to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(own.descriptor.id.as_str(), "acp.own.mine-too");
        assert_eq!(
            client.registry_env_get("own.mine-too").await.unwrap(),
            "K=v\n"
        );
        assert_eq!(shell_quote("my agent"), "'my agent'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("--acp"), "--acp");
    }

    #[tokio::test(start_paused = true)]
    async fn the_environment_is_merged_and_values_are_hidden_unless_revealed() {
        let (_daemon, client) = daemon().await;
        client
            .registry_env_set("own.mine", "A=1\nB=2\n")
            .await
            .unwrap();
        let shown = lines(
            env(
                &client,
                "own.mine",
                &["B=3".to_owned(), "C=4".to_owned()],
                &["A".to_owned()],
                false,
                false,
            )
            .await
            .unwrap(),
        );
        assert_eq!(shown, ["B=***", "C=***"]);
        let shown = lines(
            env(&client, "own.mine", &[], &[], true, false)
                .await
                .unwrap(),
        );
        assert_eq!(shown, ["B=3", "C=4"]);
        assert!(
            env(&client, "own.mine", &["bad".to_owned()], &[], false, false)
                .await
                .is_err()
        );
        match env(&client, "own.mine", &[], &[], false, true)
            .await
            .unwrap()
        {
            Output::Json(value) => assert_eq!(value["env"]["B"], "***"),
            Output::Lines(_) => panic!("json expected"),
        }
    }
}
