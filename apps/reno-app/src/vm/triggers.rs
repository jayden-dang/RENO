//! The Triggers list: each trigger with its schedule, what it fires next, what it did last, and
//! whether it is backing off. From `triggers.watch`; the schedule text comes from the trigger's
//! definition (`def.get`).

use reno_client::{Feed, State};
use reno_proto::defs::DefKind;
use reno_proto::{RunId, RunStatus, TriggerInfo, TriggerStatus};
use serde_json::Value;

use super::{Banner, Section, banner};
use crate::summary;

/// The run a trigger last started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LastRun {
    pub(crate) run: RunId,
    /// `None` until the run's summary is known.
    pub(crate) status: Option<RunStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TriggerRow {
    pub(crate) name: String,
    /// `cron`, `every`, `fs`, `manual`, `at` or `after`.
    pub(crate) kind: String,
    /// "0 6 * * * (UTC)", "every 6h", "when ~/code/web-console/src changes"; empty until the
    /// definition is read.
    pub(crate) schedule: String,
    pub(crate) bot: String,
    /// "~/code/app": where its runs work, when the definition names a directory.
    pub(crate) cwd: Option<String>,
    pub(crate) status: TriggerStatus,
    pub(crate) status_label: &'static str,
    /// "in 18h"; `None` when nothing is scheduled (manual, fs, finished, disabled).
    pub(crate) next: Option<String>,
    /// "6h ago"; `None` if it never fired.
    pub(crate) last_fired: Option<String>,
    pub(crate) last_run: Option<LastRun>,
    /// "backing off until 06:40 UTC after 3 failed starts".
    pub(crate) backoff: Option<String>,
    pub(crate) error: Option<String>,
    /// Whether "Fire now" does anything.
    pub(crate) can_fire: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TriggersView {
    pub(crate) banner: Option<Banner>,
    pub(crate) rows: Section<TriggerRow>,
    /// How many are backing off, for the lede.
    pub(crate) backing_off: usize,
}

pub(crate) const fn status_label(status: TriggerStatus) -> &'static str {
    match status {
        TriggerStatus::Scheduled => "scheduled",
        TriggerStatus::Watching => "watching",
        TriggerStatus::Manual => "manual",
        TriggerStatus::Done => "done",
        TriggerStatus::BackedOff => "backing off",
        TriggerStatus::Disabled => "disabled",
        TriggerStatus::Failed => "failed",
    }
}

/// The schedule of a trigger definition's `when`, in words.
pub(crate) fn schedule_text(when: &Value) -> String {
    let text = |key: &str| when.get(key).and_then(Value::as_str);
    match text("kind") {
        Some("cron") => match (text("cron"), text("timezone")) {
            (Some(cron), Some(zone)) => format!("{cron} ({zone})"),
            (Some(cron), None) => cron.to_owned(),
            _ => "cron".to_owned(),
        },
        Some("every") => text("every").map_or_else(|| "every".to_owned(), |e| format!("every {e}")),
        Some("at") => text("at").map_or_else(|| "once".to_owned(), |at| format!("once at {at}")),
        Some("after") => text("after").map_or_else(
            || "once, after a delay".to_owned(),
            |d| format!("once, {d} after it was defined"),
        ),
        Some("fs") => {
            let paths: Vec<&str> = when
                .get("paths")
                .and_then(Value::as_array)
                .map(|p| p.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            format!("when {} changes", paths.join(", "))
        }
        Some("manual") => "only when fired by hand".to_owned(),
        Some(other) => other.to_owned(),
        None => String::new(),
    }
}

fn row(state: &State, info: &TriggerInfo, now_ms: u64) -> TriggerRow {
    let schedule = state
        .def_record(DefKind::Trigger, &info.name, info.version)
        .and_then(|record| record.body.get("when"))
        .map(schedule_text)
        .unwrap_or_default();
    let backoff = (info.status == TriggerStatus::BackedOff).then(|| {
        let until = info.backoff_until_ms.map_or_else(String::new, |at| {
            format!(" until {}", summary::clock_utc(at))
        });
        let tries = match info.failures {
            0 => String::new(),
            1 => " after 1 failed start".to_owned(),
            n => format!(" after {n} failed starts"),
        };
        format!("backing off{until}{tries}")
    });
    TriggerRow {
        name: info.name.clone(),
        kind: info.kind.clone(),
        schedule,
        bot: info.bot.clone(),
        cwd: info.cwd.clone(),
        status: info.status,
        status_label: status_label(info.status),
        next: info
            .next_due_at_ms
            .map(|due| summary::until_label(due, now_ms)),
        last_fired: info
            .last_fired_at_ms
            .map(|at| summary::ago_label(now_ms.saturating_sub(at))),
        last_run: info.last_run_id.as_ref().map(|run| LastRun {
            run: run.clone(),
            status: state.run(run).map(|r| r.status),
        }),
        backoff,
        error: info.error.clone(),
        can_fire: !matches!(
            info.status,
            TriggerStatus::Disabled | TriggerStatus::Done | TriggerStatus::Failed
        ),
    }
}

/// What the Triggers screen shows.
pub(crate) fn build(state: &State, now_ms: u64) -> TriggersView {
    let rows: Vec<TriggerRow> = state
        .triggers()
        .iter()
        .map(|t| row(state, t, now_ms))
        .collect();
    let backing_off = rows
        .iter()
        .filter(|r| r.status == TriggerStatus::BackedOff)
        .count();
    TriggersView {
        banner: banner(state),
        rows: Section::of_state(state, state.feed(Feed::Triggers), rows),
        backing_off,
    }
}

/// The last runs whose summary is not known yet (they left the window).
pub(crate) fn runs_to_fetch(state: &State) -> Vec<RunId> {
    state
        .triggers()
        .iter()
        .filter_map(|t| t.last_run_id.clone())
        .filter(|run| state.run(run).is_none())
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_client::{FeedState, Update};
    use reno_proto::defs::DefRecord;
    use serde_json::json;

    use super::*;

    const NOW: u64 = 1_000 * 3_600_000;
    const HOUR: u64 = 3_600_000;

    fn info(name: &str, kind: &str, status: TriggerStatus) -> TriggerInfo {
        TriggerInfo {
            name: name.into(),
            kind: kind.into(),
            bot: "keeper".into(),
            version: 1,
            status,
            next_due_at_ms: None,
            last_fired_at_ms: None,
            last_run_id: None,
            failures: 0,
            backoff_until_ms: None,
            error: None,
            cwd: None,
        }
    }

    #[test]
    fn schedules_read_as_words() {
        assert_eq!(
            schedule_text(&json!({"kind": "cron", "cron": "0 6 * * *", "timezone": "UTC"})),
            "0 6 * * * (UTC)"
        );
        assert_eq!(
            schedule_text(&json!({"kind": "every", "every": "6h"})),
            "every 6h"
        );
        assert_eq!(
            schedule_text(&json!({"kind": "fs", "paths": ["~/a", "~/b"]})),
            "when ~/a, ~/b changes"
        );
        assert_eq!(
            schedule_text(&json!({"kind": "manual"})),
            "only when fired by hand"
        );
        assert_eq!(
            schedule_text(&json!({"kind": "at", "at": "2026-12-01T09:00:00+07:00"})),
            "once at 2026-12-01T09:00:00+07:00"
        );
        assert_eq!(schedule_text(&json!({})), "");
    }

    #[test]
    fn loading_empty_and_unavailable() {
        let mut state = State::new();
        assert_eq!(build(&state, NOW).rows, Section::Loading);
        state.apply(Update::Triggers(vec![]));
        assert_eq!(build(&state, NOW).rows, Section::Empty);
        state.apply(Update::Feed(Feed::Triggers, FeedState::Unavailable));
        assert_eq!(build(&state, NOW).rows, Section::Unavailable);
    }

    #[test]
    fn a_row_shows_next_last_and_the_last_runs_status() {
        let mut state = State::new();
        let mut t = info("nightly", "cron", TriggerStatus::Scheduled);
        t.next_due_at_ms = Some(NOW + 18 * HOUR);
        t.last_fired_at_ms = Some(NOW - 6 * HOUR);
        t.last_run_id = Some(RunId::from_suffix("r1").unwrap());
        state.apply(Update::Triggers(vec![t]));
        state.apply(Update::DefRecord(DefRecord {
            kind: DefKind::Trigger,
            name: "nightly".into(),
            version: 1,
            hash: "0".repeat(64),
            body: json!({"when": {"kind": "cron", "cron": "0 6 * * *", "timezone": "UTC"}}),
            created_at_ms: 1,
            latest: true,
        }));
        let view = build(&state, NOW);
        let row = &view.rows.rows()[0];
        assert_eq!(row.schedule, "0 6 * * * (UTC)");
        assert_eq!(row.next.as_deref(), Some("in 18h"));
        assert_eq!(row.last_fired.as_deref(), Some("6h ago"));
        assert_eq!(
            row.last_run.as_ref().unwrap().status,
            None,
            "run not known yet"
        );
        assert_eq!(runs_to_fetch(&state).len(), 1);
        assert!(row.can_fire);
    }

    #[test]
    fn a_row_shows_the_directory_its_runs_work_in_when_one_is_named() {
        let mut state = State::new();
        let mut t = info("nightly", "cron", TriggerStatus::Scheduled);
        t.cwd = Some("~/code/app".into());
        state.apply(Update::Triggers(vec![
            t,
            info("poke", "manual", TriggerStatus::Manual),
        ]));
        let view = build(&state, NOW);
        let rows = view.rows.rows();
        let by_name = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
        assert_eq!(by_name("nightly").cwd.as_deref(), Some("~/code/app"));
        assert_eq!(by_name("poke").cwd, None);
    }

    #[test]
    fn backoff_and_failure_states() {
        let mut state = State::new();
        let mut backed = info("digest", "every", TriggerStatus::BackedOff);
        backed.failures = 3;
        backed.backoff_until_ms = Some(NOW + HOUR);
        let mut failed = info("broken", "cron", TriggerStatus::Failed);
        failed.error = Some("bad cron".into());
        let disabled = info("off", "manual", TriggerStatus::Disabled);
        state.apply(Update::Triggers(vec![backed, failed, disabled]));
        let view = build(&state, NOW);
        assert_eq!(view.backing_off, 1);
        let rows = view.rows.rows();
        let by_name = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
        let backoff = by_name("digest").backoff.clone().unwrap();
        assert!(backoff.starts_with("backing off until "), "{backoff}");
        assert!(backoff.ends_with("after 3 failed starts"), "{backoff}");
        assert!(
            by_name("digest").can_fire,
            "a backed-off trigger can still be fired by hand"
        );
        assert!(!by_name("broken").can_fire);
        assert_eq!(by_name("broken").error.as_deref(), Some("bad cron"));
        assert!(!by_name("off").can_fire);
    }

    #[test]
    fn a_runs_summary_fills_in_when_it_arrives() {
        use reno_proto::{RunSummary, TaskId};
        let mut state = State::new();
        let mut t = info("t", "manual", TriggerStatus::Manual);
        t.last_run_id = Some(RunId::from_suffix("r1").unwrap());
        state.apply(Update::Triggers(vec![t]));
        state.apply(Update::RunUpserted(RunSummary {
            run_id: RunId::from_suffix("r1").unwrap(),
            task_id: TaskId::from_suffix("r1").unwrap(),
            executor: "claude-code".parse().unwrap(),
            status: RunStatus::Failed,
            attempt: 1,
            title: "x".into(),
            prompt: None,
            model: None,
            stop_reason: None,
            bot: None,
            cwd: None,
            created_at_ms: 1,
            updated_at_ms: 2,
            stop: None,
            not_resumed: None,
            session: None,
        }));
        let view = build(&state, NOW);
        assert_eq!(
            view.rows.rows()[0].last_run.as_ref().unwrap().status,
            Some(RunStatus::Failed)
        );
        assert!(runs_to_fetch(&state).is_empty());
    }
}
