//! The Runs list: the `runs.watch` window plus older pages, narrowed by a status filter; and the
//! facts the Run screen adds (access mode from the bot's definition).

use reno_client::{Feed, RunPage, State};
use reno_proto::defs::DefKind;
use reno_proto::{RunId, RunStatus, RunSummary};

use super::{Banner, Section, banner};
use crate::summary;

/// The filter chips above the list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum RunFilter {
    #[default]
    All,
    Running,
    Held,
    Queued,
    Passed,
    Failed,
    Stopped,
}

impl RunFilter {
    pub(crate) const ALL: [Self; 7] = [
        Self::All,
        Self::Running,
        Self::Held,
        Self::Queued,
        Self::Passed,
        Self::Failed,
        Self::Stopped,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Running => "Running",
            Self::Held => "Held",
            Self::Queued => "Queued",
            Self::Passed => "Passed",
            Self::Failed => "Failed",
            Self::Stopped => "Stopped",
        }
    }

    /// The statuses it matches; empty is every status.
    pub(crate) const fn statuses(self) -> &'static [RunStatus] {
        match self {
            Self::All => &[],
            Self::Running => &[RunStatus::Leased, RunStatus::Running, RunStatus::Evaluating],
            Self::Held => &[RunStatus::WaitingApproval, RunStatus::WaitingInput],
            Self::Queued => &[RunStatus::Queued],
            Self::Passed => &[RunStatus::Succeeded],
            Self::Failed => &[RunStatus::Failed, RunStatus::Interrupted],
            Self::Stopped => &[RunStatus::Cancelled],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunRow {
    pub(crate) run: RunId,
    pub(crate) what: String,
    pub(crate) bot: Option<String>,
    pub(crate) repo: Option<String>,
    pub(crate) executor: String,
    pub(crate) status: RunStatus,
    /// The pill's words: "stopping…" for a person's stop still settling.
    pub(crate) status_label: &'static str,
    pub(crate) attempt: u32,
    /// "6h ago".
    pub(crate) started: String,
    /// How long it ran, or has been running: "14m 05s".
    pub(crate) took: String,
}

/// The "load older runs" control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum More {
    /// Every page was loaded.
    None,
    /// The daemon has more, or the filter was not loaded yet.
    Available,
    /// A page is on its way.
    Loading,
    /// The daemon is out of reach: no page can be read, and "every run" is not known.
    Offline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunsView {
    pub(crate) banner: Option<Banner>,
    pub(crate) filter: RunFilter,
    /// How many runs each filter matches among the runs known so far.
    pub(crate) counts: Vec<(RunFilter, usize)>,
    pub(crate) rows: Section<RunRow>,
    pub(crate) more: More,
    /// The lede's second line: how many runs are known, or that none can be while offline.
    pub(crate) detail: String,
}

fn row(run: &RunSummary, now_ms: u64) -> RunRow {
    let parts = summary::parse_title(&run.title);
    let end = if run.status.is_terminal() {
        run.updated_at_ms
    } else {
        now_ms
    };
    RunRow {
        run: run.run_id.clone(),
        what: parts.what.to_owned(),
        bot: run
            .bot
            .as_ref()
            .map(|b| b.name.clone())
            .or_else(|| parts.bot.map(str::to_owned)),
        repo: parts.repo.map(str::to_owned),
        executor: run.executor.as_str().to_owned(),
        status: run.status,
        status_label: summary::run_status_label(run),
        attempt: run.attempt,
        started: summary::ago_label(now_ms.saturating_sub(run.created_at_ms)),
        // A queued run has not run yet, so it has no duration to show.
        took: if run.status == RunStatus::Queued {
            "\u{2014}".to_owned()
        } else {
            summary::duration_label(end.saturating_sub(run.created_at_ms))
        },
    }
}

/// What the Runs list shows for `filter`. `paging` is whether a page request is in flight.
pub(crate) fn build(state: &State, filter: RunFilter, paging: bool, now_ms: u64) -> RunsView {
    let everything = state.all_runs(&[]);
    let counts = RunFilter::ALL
        .iter()
        .map(|f| {
            let n = if f.statuses().is_empty() {
                everything.len()
            } else {
                everything
                    .iter()
                    .filter(|r| f.statuses().contains(&r.status))
                    .count()
            };
            (*f, n)
        })
        .collect();
    let rows: Vec<RunRow> = state
        .all_runs(filter.statuses())
        .into_iter()
        .map(|r| row(r, now_ms))
        .collect();
    let offline = super::is_offline(state);
    let detail = match everything.len() {
        0 if offline => "Not connected: runs appear when the daemon answers.".to_owned(),
        0 => "No runs yet.".to_owned(),
        1 => "1 run known.".to_owned(),
        n => format!("{n} runs known."),
    };
    let more = match (state.run_page(filter.statuses()), paging) {
        _ if offline => More::Offline,
        (_, true) => More::Loading,
        (RunPage::Done, false) => More::None,
        (RunPage::NotLoaded | RunPage::More(_), false) => More::Available,
    };
    RunsView {
        banner: banner(state),
        filter,
        counts,
        rows: Section::of_state(state, state.feed(Feed::Runs), rows),
        more,
        detail,
    }
}

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
    use reno_client::{FeedState, Update};
    use reno_proto::TaskId;
    use reno_proto::defs::{BotPin, DefKind, DefRecord};

    use super::*;

    const NOW: u64 = 1_000 * 3_600_000;

    fn run(name: &str, status: RunStatus, age_min: u64) -> RunSummary {
        RunSummary {
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
    fn offline_claims_neither_no_runs_nor_older_runs() {
        use reno_client::{Offline, OfflineKind};
        let mut state = State::new();
        state.apply(Update::Offline(Offline {
            kind: OfflineKind::NotRunning,
            detail: "x".into(),
            attempt: 2,
        }));
        let view = build(&state, RunFilter::All, false, NOW);
        assert!(view.banner.is_some_and(|b| !b.connecting));
        assert_ne!(
            view.rows,
            Section::Empty,
            "nothing is known, not nothing there"
        );
        assert_ne!(view.rows, Section::Loading, "nothing is on its way either");
        assert_ne!(view.more, More::Available, "no daemon to page from");
        assert_eq!(view.rows, Section::NotConnected);
        assert_eq!(view.more, More::Offline);
        assert_eq!(
            view.detail,
            "Not connected: runs appear when the daemon answers."
        );
        // Even after an empty list was once read, the connection is gone: it is unknown now.
        let mut seen = State::new();
        seen.apply(Update::Runs(vec![]));
        seen.apply(Update::Offline(Offline {
            kind: OfflineKind::NotRunning,
            detail: "x".into(),
            attempt: 1,
        }));
        assert_eq!(
            build(&seen, RunFilter::All, false, NOW).rows,
            Section::NotConnected
        );
    }

    #[test]
    fn loading_then_empty() {
        let state = State::new();
        let view = build(&state, RunFilter::All, false, NOW);
        assert_eq!(view.rows, Section::Loading);
        assert!(view.banner.is_some());
        let mut state = State::new();
        state.apply(Update::Runs(vec![]));
        let view = build(&state, RunFilter::All, false, NOW);
        assert_eq!(view.rows, Section::Empty);
        assert!(view.banner.is_some(), "not connected yet");
        // Nothing was paged, so older runs may exist.
        assert_eq!(view.more, More::Available);
    }

    #[test]
    fn rows_are_newest_first_with_ages_and_durations() {
        let mut state = State::new();
        state.apply(Update::Runs(vec![
            run("old", RunStatus::Succeeded, 120),
            run("new", RunStatus::Running, 10),
        ]));
        let view = build(&state, RunFilter::All, false, NOW);
        let rows = view.rows.rows();
        assert_eq!(rows[0].run.to_string(), "run_new");
        assert_eq!(rows[0].started, "10m ago");
        assert_eq!(rows[0].took, "10m 00s", "a live run is timed to now");
        assert_eq!(rows[0].what, "job new");
        assert_eq!(rows[0].bot.as_deref(), Some("keeper"));
        assert_eq!(rows[0].repo.as_deref(), Some("atlas-api"));
        assert_eq!(rows[1].took, "2m 00s", "a finished run is timed to its end");

        state.apply(Update::Runs(vec![run("waiting", RunStatus::Queued, 5)]));
        let view = build(&state, RunFilter::All, false, NOW);
        assert_eq!(
            view.rows.rows()[0].took,
            "\u{2014}",
            "a queued run has not run yet"
        );
    }

    #[test]
    fn filters_narrow_the_rows_and_count_everything() {
        let mut state = State::new();
        state.apply(Update::Runs(vec![
            run("a", RunStatus::Running, 5),
            run("b", RunStatus::WaitingApproval, 6),
            run("c", RunStatus::Failed, 7),
            run("d", RunStatus::Interrupted, 8),
            run("e", RunStatus::Succeeded, 9),
        ]));
        let held = build(&state, RunFilter::Held, false, NOW);
        assert_eq!(held.rows.rows().len(), 1);
        let failed = build(&state, RunFilter::Failed, false, NOW);
        assert_eq!(failed.rows.rows().len(), 2, "failed and interrupted");
        let counts: Vec<_> = held.counts.iter().map(|(f, n)| (f.label(), *n)).collect();
        assert_eq!(
            counts,
            [
                ("All", 5),
                ("Running", 1),
                ("Held", 1),
                ("Queued", 0),
                ("Passed", 1),
                ("Failed", 2),
                ("Stopped", 0)
            ]
        );
        // A filter with no match is empty, not loading.
        let queued = build(&state, RunFilter::Queued, false, NOW);
        assert_eq!(queued.rows, Section::Empty);
    }

    #[test]
    fn paging_state_per_filter() {
        let mut state = State::new();
        state.apply(Update::Runs(vec![run("a", RunStatus::Failed, 5)]));
        assert_eq!(
            build(&state, RunFilter::Failed, false, NOW).more,
            More::Available
        );
        assert_eq!(
            build(&state, RunFilter::Failed, true, NOW).more,
            More::Loading
        );
        state.apply(Update::RunsPage {
            filter: RunFilter::Failed.statuses().to_vec(),
            runs: vec![run("z", RunStatus::Failed, 500)],
            next: None,
        });
        let view = build(&state, RunFilter::Failed, false, NOW);
        assert_eq!(view.more, More::None);
        assert_eq!(view.rows.rows().len(), 2, "the older page joins the window");
        // Another filter was not paged.
        assert_eq!(
            build(&state, RunFilter::All, false, NOW).more,
            More::Available
        );
    }

    #[test]
    fn a_failed_feed_with_no_rows_says_so() {
        let mut state = State::new();
        state.apply(Update::Feed(Feed::Runs, FeedState::Failed("nope".into())));
        let view = build(&state, RunFilter::All, false, NOW);
        assert_eq!(view.rows, Section::Failed("nope".into()));
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
