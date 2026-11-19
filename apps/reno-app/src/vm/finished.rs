//! All runs' "Finished" section: the runs that have ended, newest first, paged through the
//! daemon's `run.list` with a "Show older" control while more pages exist.

use reno_client::{ConnectionStatus, RunPage, State, filter_key};
use reno_proto::{RunId, RunListParams, RunStatus};

use crate::summary;

/// The statuses of a run that has ended: the filter the pages are asked with.
pub(crate) const STATUSES: [RunStatus; 4] = [
    RunStatus::Succeeded,
    RunStatus::Failed,
    RunStatus::Cancelled,
    RunStatus::Interrupted,
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) run: RunId,
    pub(crate) title: String,
    pub(crate) executor: String,
    pub(crate) status_label: &'static str,
    pub(crate) time: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum More {
    None,
    Available,
    Loading,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Finished {
    pub(crate) rows: Vec<Row>,
    pub(crate) more: More,
}

/// The section: ended runs the app knows (the live window and the pages read), newest first.
/// "Show older" is offered while the daemon may have more, and not while offline.
pub(crate) fn build(state: &State, paging: bool, now_ms: u64) -> Finished {
    let rows = state
        .all_runs(&STATUSES)
        .into_iter()
        .map(|run| Row {
            run: run.run_id.clone(),
            title: summary::parse_title(&run.title).what.to_owned(),
            executor: run.executor.as_str().to_owned(),
            status_label: summary::run_status_label(run),
            time: summary::age_label(now_ms.saturating_sub(run.updated_at_ms)),
        })
        .collect();
    let more = if state.connection() != ConnectionStatus::Connected
        || state.run_page(&STATUSES) == RunPage::Done
    {
        More::None
    } else if paging {
        More::Loading
    } else {
        More::Available
    };
    Finished { rows, more }
}

/// The next `run.list` request, or `None` when every page was read.
pub(crate) fn next_page(state: &State, limit: u32) -> Option<RunListParams> {
    let before = match state.run_page(&STATUSES) {
        RunPage::Done => return None,
        RunPage::More(cursor) => Some(cursor),
        RunPage::NotLoaded => None,
    };
    Some(RunListParams {
        status: Some(filter_key(&STATUSES)),
        limit: Some(limit),
        before,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_client::{ConnectionStatus, Update};
    use reno_proto::{RunSummary, TaskId};

    use super::*;

    fn run(id: &str, status: RunStatus, title: &str, created: u64, updated: u64) -> RunSummary {
        RunSummary {
            model_options: Default::default(),
            effort: None,
            run_id: RunId::from_suffix(id).unwrap(),
            task_id: TaskId::from_suffix(id).unwrap(),
            executor: "codex".parse().unwrap(),
            status,
            attempt: 1,
            title: title.to_owned(),
            prompt: None,
            model: None,
            stop_reason: None,
            stop: None,
            not_resumed: None,
            session: None,
            bot: None,
            cwd: None,
            report: None,
            created_at_ms: created,
            updated_at_ms: updated,
        }
    }

    fn state(runs: Vec<RunSummary>) -> State {
        let mut s = State::new();
        s.apply(Update::Connection(ConnectionStatus::Connected));
        s.apply(Update::Runs(runs));
        s
    }

    fn rid(id: &str) -> RunId {
        RunId::from_suffix(id).unwrap()
    }

    #[test]
    fn finished_rows_are_only_ended_runs_newest_first_with_their_words() {
        let s = state(vec![
            run(
                "old",
                RunStatus::Succeeded,
                "builder \u{b7} reno: old one",
                100,
                160_000,
            ),
            run("live", RunStatus::Running, "still going", 900, 900),
            run("bad", RunStatus::Failed, "failed one", 500, 560_000),
            run("stop", RunStatus::Cancelled, "stopped one", 300, 360_000),
        ]);
        let f = build(&s, false, 620_000);
        let ids: Vec<_> = f.rows.iter().map(|r| r.run.to_string()).collect();
        assert_eq!(ids, ["run_bad", "run_stop", "run_old"]);
        assert_eq!(f.rows[0].status_label, "failed");
        assert_eq!(f.rows[1].status_label, "stopped");
        assert_eq!(f.rows[2].status_label, "passed");
        assert_eq!(
            f.rows[2].title, "old one",
            "without the bot and repository prefix"
        );
        assert_eq!(f.rows[2].executor, "codex");
        assert_eq!(f.rows[0].time, "1m", "since it ended");
    }

    #[test]
    fn show_older_is_offered_only_while_more_pages_may_exist() {
        let mut s = state(vec![run("a", RunStatus::Succeeded, "a", 1, 2)]);
        // Nothing paged yet: there may be more than the window holds.
        assert_eq!(build(&s, false, 10).more, More::Available);
        assert_eq!(build(&s, true, 10).more, More::Loading);
        s.apply(Update::RunsPage {
            filter: STATUSES.to_vec(),
            runs: vec![run("b", RunStatus::Failed, "b", 0, 1)],
            next: Some(rid("b")),
        });
        assert_eq!(build(&s, false, 10).more, More::Available);
        s.apply(Update::RunsPage {
            filter: STATUSES.to_vec(),
            runs: vec![],
            next: None,
        });
        assert_eq!(build(&s, false, 10).more, More::None, "every page was read");
        // Offline: no control that cannot work.
        let mut off = state(vec![]);
        off.apply(Update::Connection(ConnectionStatus::Disconnected));
        assert_eq!(build(&off, false, 10).more, More::None);
    }

    #[test]
    fn the_next_page_starts_at_the_cursor_and_stops_when_done() {
        let mut s = state(vec![]);
        let first = next_page(&s, 20).unwrap();
        assert_eq!(first.before, None);
        assert_eq!(first.limit, Some(20));
        let mut want = STATUSES.to_vec();
        want.sort();
        assert_eq!(first.status, Some(want));
        s.apply(Update::RunsPage {
            filter: STATUSES.to_vec(),
            runs: vec![],
            next: Some(rid("cursor")),
        });
        assert_eq!(next_page(&s, 20).unwrap().before, Some(rid("cursor")));
        s.apply(Update::RunsPage {
            filter: STATUSES.to_vec(),
            runs: vec![],
            next: None,
        });
        assert_eq!(next_page(&s, 20), None);
    }
}
