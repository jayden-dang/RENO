//! The sidebar's projects: runs grouped by the git repository they were submitted in, one
//! folder per repository, with a status dot per run (ui-brief 2026-11-17, "Shell — sidebar").
//!
//! A project is the repository root of a run's `cwd`. Finding it reads the disk, so callers pass
//! a resolver (`local::Roots` in the app, a table in tests); a run with no `cwd`, or one outside any
//! repository (the daemon's scratch folder), goes to "No project".

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use reno_proto::{ExecutorId, RunId, RunStatus, RunSummary};

use crate::summary::{self, Problem};

/// The most runs an open folder lists; waiting runs are always listed on top of this.
pub(crate) const PER_FOLDER: usize = 8;

/// A run's dot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dot {
    /// Queued, starting or running.
    Running,
    /// Waiting for you: a held action or a question.
    Waiting,
    /// Ended since you last opened it: "has a message".
    New,
    /// Ended and seen: no dot.
    Seen,
    /// Failed, or the agent reported it failed or blocked. Wins over `New`.
    Failed,
    /// Cancelled or interrupted.
    Stopped,
}

/// When you last opened each run. A run never opened counts as seen at `baseline_ms`: the first
/// time this app ran, so runs that ended before Reno's app was ever opened are not all "new".
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Seen {
    pub(crate) baseline_ms: u64,
    pub(crate) opened: HashMap<RunId, u64>,
}

/// How many runs [`Seen`] remembers; the oldest openings are forgotten first.
const SEEN_KEEP: usize = 500;

impl Seen {
    pub(crate) fn new(baseline_ms: u64) -> Self {
        Self {
            baseline_ms,
            opened: HashMap::new(),
        }
    }

    /// When `run` was last looked at.
    pub(crate) fn last(&self, run: &RunId) -> u64 {
        self.opened.get(run).copied().unwrap_or(self.baseline_ms)
    }

    /// You looked at `run` at `now_ms`.
    pub(crate) fn mark(&mut self, run: &RunId, now_ms: u64) {
        self.opened.insert(run.clone(), now_ms);
        if self.opened.len() > SEEN_KEEP {
            let mut by_age: Vec<(RunId, u64)> =
                self.opened.iter().map(|(r, at)| (r.clone(), *at)).collect();
            by_age.sort_by_key(|(_, at)| *at);
            for (run, _) in by_age.into_iter().take(self.opened.len() - SEEN_KEEP) {
                self.opened.remove(&run);
            }
        }
    }
}

/// The dot for `run`. `looking` is true while the run is on screen: it is seen as it ends.
pub(crate) fn dot(run: &RunSummary, seen: &Seen, looking: bool) -> Dot {
    match run.status {
        RunStatus::WaitingApproval | RunStatus::WaitingInput => Dot::Waiting,
        RunStatus::Queued | RunStatus::Leased | RunStatus::Running | RunStatus::Evaluating => {
            Dot::Running
        }
        RunStatus::Failed => Dot::Failed,
        RunStatus::Cancelled | RunStatus::Interrupted => Dot::Stopped,
        RunStatus::Succeeded => match summary::reported_problem(run) {
            Some(Problem::Failed | Problem::Blocked) => Dot::Failed,
            None if !looking && run.updated_at_ms > seen.last(&run.run_id) => Dot::New,
            None => Dot::Seen,
        },
    }
}

/// Which folder a run is in.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum FolderKey {
    /// A repository, by its canonical root.
    Repo(PathBuf),
    /// Runs outside any repository.
    NoProject,
}

impl FolderKey {
    /// Folders start open, except "No project".
    pub(crate) fn open_by_default(&self) -> bool {
        matches!(self, Self::Repo(_))
    }

    /// The repository root, when there is one.
    pub(crate) fn root(&self) -> Option<&Path> {
        match self {
            Self::Repo(root) => Some(root),
            Self::NoProject => None,
        }
    }

    /// The folder's name: the repository's directory name.
    pub(crate) fn name(&self) -> String {
        match self {
            Self::Repo(root) => root.file_name().map_or_else(
                || root.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
            Self::NoProject => "No project".to_owned(),
        }
    }
}

/// One run under a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunRow {
    pub(crate) run: RunId,
    pub(crate) title: String,
    pub(crate) executor: ExecutorId,
    pub(crate) dot: Dot,
    /// "2m", "1h", "1d".
    pub(crate) age: String,
}

/// One project folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Folder {
    pub(crate) key: FolderKey,
    pub(crate) name: String,
    pub(crate) open: bool,
    /// Runs waiting for you inside, listed even while the folder is closed.
    pub(crate) waiting: usize,
    pub(crate) rows: Vec<RunRow>,
    /// Runs inside that are not listed: "N more".
    pub(crate) more: usize,
}

/// What the sidebar needs besides the runs.
pub(crate) struct Inputs<'a> {
    pub(crate) seen: &'a Seen,
    /// Folders whose open/closed state differs from [`FolderKey::open_by_default`].
    pub(crate) toggled: &'a HashSet<FolderKey>,
    /// The run on screen.
    pub(crate) looking_at: Option<&'a RunId>,
    pub(crate) now_ms: u64,
}

/// The folders, most recently active first, "No project" last. Inside a folder, waiting runs
/// first, then newest first. `root_of` maps a `cwd` to its repository root.
pub(crate) fn folders(
    runs: &[&RunSummary],
    root_of: impl Fn(&Path) -> Option<PathBuf>,
    inputs: &Inputs<'_>,
) -> Vec<Folder> {
    let mut groups: Vec<(FolderKey, Vec<&RunSummary>)> = Vec::new();
    for run in runs {
        let key = run
            .cwd
            .as_deref()
            .and_then(&root_of)
            .map_or(FolderKey::NoProject, FolderKey::Repo);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push(run),
            None => groups.push((key, vec![run])),
        }
    }
    let latest = |list: &[&RunSummary]| list.iter().map(|r| r.updated_at_ms).max().unwrap_or(0);
    groups.sort_by(|(ka, la), (kb, lb)| {
        (*ka == FolderKey::NoProject)
            .cmp(&(*kb == FolderKey::NoProject))
            .then_with(|| latest(lb).cmp(&latest(la)))
            .then_with(|| ka.cmp(kb))
    });
    groups
        .into_iter()
        .map(|(key, mut list)| {
            list.sort_by(|a, b| {
                is_waiting(b)
                    .cmp(&is_waiting(a))
                    .then_with(|| b.created_at_ms.cmp(&a.created_at_ms))
                    .then_with(|| a.run_id.cmp(&b.run_id))
            });
            let open = key.open_by_default() != inputs.toggled.contains(&key);
            let waiting = list.iter().filter(|r| is_waiting(r)).count();
            let shown = if open {
                waiting.max(PER_FOLDER.min(list.len()))
            } else {
                waiting
            };
            let rows = list
                .iter()
                .take(shown)
                .map(|run| RunRow {
                    run: run.run_id.clone(),
                    title: run.title.clone(),
                    executor: run.executor.clone(),
                    dot: dot(run, inputs.seen, inputs.looking_at == Some(&run.run_id)),
                    age: summary::age_label(inputs.now_ms.saturating_sub(run.created_at_ms)),
                })
                .collect();
            Folder {
                name: key.name(),
                open,
                waiting,
                rows,
                more: list.len() - shown,
                key,
            }
        })
        .collect()
}

/// The project a run belongs to (`None`: a run not known yet, which shows as "No project").
pub(crate) fn key_of(
    run: Option<&RunSummary>,
    root_of: impl Fn(&Path) -> Option<PathBuf>,
) -> FolderKey {
    run.and_then(|r| r.cwd.as_deref())
        .and_then(root_of)
        .map_or(FolderKey::NoProject, FolderKey::Repo)
}

fn is_waiting(run: &RunSummary) -> bool {
    matches!(
        run.status,
        RunStatus::WaitingApproval | RunStatus::WaitingInput
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_proto::{ReportStatus, RunReport, TaskId};

    use super::*;

    fn run(id: &str, status: RunStatus, cwd: Option<&str>, created: u64) -> RunSummary {
        RunSummary {
            run_id: RunId::from_suffix(id).unwrap(),
            task_id: TaskId::from_suffix(id).unwrap(),
            executor: "claude-code".parse().unwrap(),
            status,
            attempt: 1,
            title: format!("title {id}"),
            prompt: None,
            model: None,
            stop_reason: None,
            stop: None,
            not_resumed: None,
            session: None,
            bot: None,
            cwd: cwd.map(PathBuf::from),
            report: None,
            created_at_ms: created,
            updated_at_ms: created + 10,
        }
    }

    /// `/w/porch` and `/w/reno` are repositories; a subdirectory belongs to its repository.
    fn roots(dir: &Path) -> Option<PathBuf> {
        ["/w/porch", "/w/reno"]
            .into_iter()
            .map(PathBuf::from)
            .find(|root| dir.starts_with(root))
    }

    fn inputs<'a>(seen: &'a Seen, toggled: &'a HashSet<FolderKey>) -> Inputs<'a> {
        Inputs {
            seen,
            toggled,
            looking_at: None,
            now_ms: 1_000_000,
        }
    }

    #[test]
    fn runs_are_grouped_by_repository_with_no_project_last() {
        let a = run("a", RunStatus::Succeeded, Some("/w/porch"), 100);
        let b = run("b", RunStatus::Running, Some("/w/reno/crates"), 300);
        let c = run("c", RunStatus::Succeeded, Some("/scratch"), 900);
        let d = run("d", RunStatus::Succeeded, None, 800);
        let runs = vec![&c, &d, &b, &a];
        let seen = Seen::new(0);
        let toggled = HashSet::new();
        let folders = folders(&runs, roots, &inputs(&seen, &toggled));
        let names: Vec<_> = folders.iter().map(|f| f.name.as_str()).collect();
        // reno was active last; "No project" is last whatever its activity.
        assert_eq!(names, ["reno", "porch", "No project"]);
        assert_eq!(folders[2].rows.len(), 0, "No project starts closed");
        assert_eq!(folders[2].more, 2);
        assert!(folders[0].open && folders[1].open);
    }

    #[test]
    fn waiting_runs_come_first_and_stay_listed_under_a_closed_folder() {
        let old_wait = run("w", RunStatus::WaitingApproval, Some("/w/porch"), 100);
        let new_done = run("n", RunStatus::Succeeded, Some("/w/porch"), 500);
        let question = run("q", RunStatus::WaitingInput, Some("/w/porch"), 50);
        let runs = vec![&new_done, &old_wait, &question];
        let seen = Seen::new(0);
        let mut toggled = HashSet::new();
        let open = folders(&runs, roots, &inputs(&seen, &toggled));
        let order: Vec<_> = open[0].rows.iter().map(|r| r.run.to_string()).collect();
        assert_eq!(order, ["run_w", "run_q", "run_n"]);
        assert_eq!(open[0].waiting, 2);

        toggled.insert(FolderKey::Repo(PathBuf::from("/w/porch")));
        let closed = folders(&runs, roots, &inputs(&seen, &toggled));
        assert!(!closed[0].open);
        let order: Vec<_> = closed[0].rows.iter().map(|r| r.run.to_string()).collect();
        assert_eq!(
            order,
            ["run_w", "run_q"],
            "a closed folder still lists what waits"
        );
        assert_eq!(closed[0].more, 1);
    }

    #[test]
    fn an_open_folder_lists_at_most_a_page_plus_every_waiting_run() {
        let many: Vec<RunSummary> = (0..12)
            .map(|i| {
                let status = if i < 9 {
                    RunStatus::WaitingApproval
                } else {
                    RunStatus::Succeeded
                };
                run(&format!("r{i}"), status, Some("/w/reno"), i)
            })
            .collect();
        let runs: Vec<&RunSummary> = many.iter().collect();
        let seen = Seen::new(0);
        let toggled = HashSet::new();
        let f = &folders(&runs, roots, &inputs(&seen, &toggled))[0];
        assert_eq!(f.rows.len(), 9, "all 9 waiting runs, more than a page");
        assert_eq!(f.more, 3);
    }

    #[test]
    fn dots_follow_the_brief() {
        let seen = Seen::new(50);
        let d = |status, looking| dot(&run("x", status, None, 100), &seen, looking);
        assert_eq!(d(RunStatus::Running, false), Dot::Running);
        assert_eq!(d(RunStatus::Queued, false), Dot::Running);
        assert_eq!(d(RunStatus::WaitingApproval, false), Dot::Waiting);
        assert_eq!(d(RunStatus::WaitingInput, false), Dot::Waiting);
        assert_eq!(d(RunStatus::Failed, false), Dot::Failed);
        assert_eq!(d(RunStatus::Cancelled, false), Dot::Stopped);
        assert_eq!(d(RunStatus::Interrupted, false), Dot::Stopped);
        // Ended at 110, after the baseline (50): new, unless it is on screen.
        assert_eq!(d(RunStatus::Succeeded, false), Dot::New);
        assert_eq!(d(RunStatus::Succeeded, true), Dot::Seen);
    }

    #[test]
    fn opening_a_run_marks_it_seen_and_a_reported_failure_wins_over_new() {
        let mut seen = Seen::new(0);
        let done = run("x", RunStatus::Succeeded, None, 100);
        assert_eq!(dot(&done, &seen, false), Dot::New);
        seen.mark(&done.run_id, 200);
        assert_eq!(dot(&done, &seen, false), Dot::Seen);

        let mut failed = run("f", RunStatus::Succeeded, None, 100);
        failed.report = Some(RunReport {
            status: ReportStatus::Failed,
            summary: "tests failed".to_owned(),
        });
        assert_eq!(dot(&failed, &Seen::new(0), false), Dot::Failed);
    }

    #[test]
    fn runs_that_ended_before_the_baseline_are_not_new() {
        let seen = Seen::new(10_000);
        assert_eq!(
            dot(&run("x", RunStatus::Succeeded, None, 100), &seen, false),
            Dot::Seen
        );
    }

    #[test]
    fn seen_forgets_the_oldest_openings_past_its_cap() {
        let mut seen = Seen::new(0);
        for i in 0..(SEEN_KEEP as u64 + 5) {
            seen.mark(&RunId::from_suffix(&format!("r{i}")).unwrap(), i);
        }
        assert_eq!(seen.opened.len(), SEEN_KEEP);
        assert!(!seen.opened.contains_key(&RunId::from_suffix("r0").unwrap()));
    }

    #[test]
    fn folder_names_are_the_repository_directory() {
        assert_eq!(FolderKey::Repo(PathBuf::from("/w/porch")).name(), "porch");
        assert_eq!(FolderKey::NoProject.name(), "No project");
        assert!(!FolderKey::NoProject.open_by_default());
    }
}
