//! The facts the Run screen adds to a run: the bot to read, and the access mode its definition grants.
//! (The old Runs list, its filters and its paging went with the All runs lanes: `vm/lanes.rs`.)

use reno_client::State;
use reno_proto::RunId;
use reno_proto::defs::DefKind;

/// The bot definition to read for a run's access mode, when it is not known yet.
pub(crate) fn bot_to_fetch(state: &State, run: &RunId) -> Option<(String, u32)> {
    let pin = state.run(run)?.bot.as_ref()?;
    state
        .def_record(DefKind::Bot, &pin.name, pin.version)
        .is_none()
        .then(|| (pin.name.clone(), pin.version))
}

/// The access mode the run's bot definition grants, in words. `None` until the definition is
/// read, or when the run has no bot.
pub(crate) fn access_label(state: &State, run: &RunId) -> Option<&'static str> {
    let pin = state.run(run)?.bot.as_ref()?;
    let record = state.def_record(DefKind::Bot, &pin.name, pin.version)?;
    access_words(record.body.get("access")?.as_str()?)
}

pub(crate) fn access_words(access: &str) -> Option<&'static str> {
    Some(match access {
        "supervised" => "Supervised",
        "auto_accept_edits" => "Auto-accept edits",
        "full_access" => "Full access",
        _ => return None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_client::Update;
    use reno_proto::defs::{BotPin, DefRecord};
    use reno_proto::{RunStatus, RunSummary, TaskId};

    use super::*;

    const NOW: u64 = 1_000 * 3_600_000;

    fn run(name: &str, status: RunStatus, age_min: u64) -> RunSummary {
        RunSummary {
            model_options: Default::default(),
            effort: None,
            run_id: RunId::from_suffix(name).unwrap(),
            task_id: TaskId::from_suffix(name).unwrap(),
            executor: "claude-code".parse().unwrap(),
            status,
            attempt: 1,
            title: format!("keeper · atlas-api: job {name}"),
            prompt: None,
            model: None,
            stop_reason: None,
            bot: Some(BotPin {
                name: "keeper".into(),
                version: 1,
            }),
            cwd: None,
            created_at_ms: NOW - age_min * 60_000,
            updated_at_ms: NOW - age_min * 60_000 + 120_000,
            stop: None,
            report: None,
            not_resumed: None,
            session: None,
        }
    }

    #[test]
    fn access_comes_from_the_bots_definition() {
        let mut state = State::new();
        let r = run("a", RunStatus::Running, 5);
        let id = r.run_id.clone();
        state.apply(Update::Runs(vec![r]));
        assert_eq!(bot_to_fetch(&state, &id), Some(("keeper".into(), 1)));
        assert_eq!(access_label(&state, &id), None);
        state.apply(Update::DefRecord(DefRecord {
            kind: DefKind::Bot,
            name: "keeper".into(),
            version: 1,
            hash: "0".repeat(64),
            body: serde_json::json!({"access": "auto_accept_edits"}),
            created_at_ms: 1,
            latest: true,
        }));
        assert_eq!(bot_to_fetch(&state, &id), None);
        assert_eq!(access_label(&state, &id), Some("Auto-accept edits"));
        assert_eq!(access_words("full_access"), Some("Full access"));
        assert_eq!(access_words("???"), None);
    }
}
