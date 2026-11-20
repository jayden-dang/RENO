//! The sidebar's projects: runs grouped by the git repository they were submitted in, one
//! folder per repository, with a status dot per run (ui-brief 2026-11-17, "Shell — sidebar").
//!
//! A project is the repository root of a run's `cwd`. Finding it reads the disk, so callers pass
//! a resolver (`local::Roots` in the app, a table in tests); a run with no `cwd`, or one outside any
//! repository (the daemon's scratch folder), goes to "No project".

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use reno_client::thread_desk::ThreadMarks;
use reno_proto::{ExecutorId, RunId, RunStatus, RunSummary, ThreadId};

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

/// What a thread row says in words it is waiting for (the sidebar's text badge).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Badge {
    /// The agent asked a question.
    NeedsInput,
    /// A harness action is held for the person.
    NeedsApproval,
}

impl Badge {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::NeedsInput => "Needs input",
            Self::NeedsApproval => "Needs approval",
        }
    }
}

/// The badge of a run, from its real status.
pub(crate) fn badge(run: &RunSummary) -> Option<Badge> {
    match run.status {
        RunStatus::WaitingInput => Some(Badge::NeedsInput),
        RunStatus::WaitingApproval => Some(Badge::NeedsApproval),
        _ => None,
    }
}

/// One thread under a folder (a run of its own on a daemon without threads).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ThreadRow {
    /// The thread, when the daemon has them and has named it.
    pub(crate) thread: Option<ThreadId>,
    /// The thread's latest run: the one a click opens.
    pub(crate) run: RunId,
    /// Every run of the thread the app knows, oldest first (the latest included).
    pub(crate) members: Vec<RunId>,
    /// The first run's title.
    pub(crate) title: String,
    pub(crate) executor: ExecutorId,
    /// The latest run's dot.
    pub(crate) dot: Dot,
    /// What the latest run waits for, in words.
    pub(crate) badge: Option<Badge>,
    /// "2m", "1h", "1d": since the latest run started.
    pub(crate) age: String,
    /// The thread is pinned (the mocked thread desk; always false without it).
    pub(crate) pinned: bool,
    /// The folder's name: the project the thread is in.
    pub(crate) project: String,
    /// When the thread's first run started, and when its latest run last changed.
    pub(crate) created_ms: u64,
    pub(crate) updated_ms: u64,
}

/// One project folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Folder {
    pub(crate) key: FolderKey,
    pub(crate) name: String,
    pub(crate) open: bool,
    /// Threads waiting for you inside, listed even while the folder is closed.
    pub(crate) waiting: usize,
    pub(crate) rows: Vec<ThreadRow>,
    /// Threads the folder has but does not list: "Show 6 more".
    pub(crate) hidden: usize,
    /// Every thread is listed although there are more than a page: "Show less".
    pub(crate) expanded: bool,
}

/// What the sidebar needs besides the runs.
pub(crate) struct Inputs<'a> {
    pub(crate) seen: &'a Seen,
    /// Folders whose open/closed state differs from [`FolderKey::open_by_default`].
    pub(crate) toggled: &'a HashSet<FolderKey>,
    /// The run on screen.
    pub(crate) looking_at: Option<&'a RunId>,
    pub(crate) now_ms: u64,
    /// The thread a run belongs to (always `None` on a daemon without threads).
    pub(crate) thread_of: &'a dyn Fn(&RunId) -> Option<ThreadId>,
    /// What the sidebar's thread controls change about the list; `None` lists as it always did.
    pub(crate) edit: Option<&'a Edit<'a>>,
}

/// What the thread controls (pin, archive, rename, "Show more") change about the list.
pub(crate) struct Edit<'a> {
    pub(crate) marks: &'a ThreadMarks,
    /// Folders listed whole, not a page.
    pub(crate) expanded: &'a HashSet<FolderKey>,
    /// Pinned threads leave their folders (a "Pinned" section, a strip) instead of leading them.
    pub(crate) pinned_apart: bool,
}

/// The sidebar's list: the folders, and the threads that are listed apart from them.
pub(crate) struct Listing {
    pub(crate) folders: Vec<Folder>,
    /// Pinned threads, in pin order; only when [`Edit::pinned_apart`].
    pub(crate) pinned: Vec<ThreadRow>,
    /// Archived threads, the most recently archived first, from every project.
    pub(crate) archived: Vec<ThreadRow>,
}

/// A thread's runs the app knows, oldest first. A run with no known thread is one of its own.
struct Entry<'a> {
    thread: Option<ThreadId>,
    runs: Vec<&'a RunSummary>,
}

impl<'a> Entry<'a> {
    fn first(&self) -> &'a RunSummary {
        self.runs[0]
    }

    fn latest(&self) -> &'a RunSummary {
        self.runs[self.runs.len() - 1]
    }
}

fn entries<'a>(runs: &[&'a RunSummary], inputs: &Inputs<'_>) -> Vec<Entry<'a>> {
    let mut out: Vec<Entry<'a>> = Vec::new();
    for run in runs {
        match (inputs.thread_of)(&run.run_id) {
            Some(thread) => match out.iter_mut().find(|e| e.thread.as_ref() == Some(&thread)) {
                Some(entry) => entry.runs.push(run),
                None => out.push(Entry {
                    thread: Some(thread),
                    runs: vec![run],
                }),
            },
            None => out.push(Entry {
                thread: None,
                runs: vec![run],
            }),
        }
    }
    for entry in &mut out {
        entry
            .runs
            .sort_by(|a, b| (a.created_at_ms, &a.run_id).cmp(&(b.created_at_ms, &b.run_id)));
    }
    out
}

/// The folders, most recently active first, "No project" last. Inside a folder, waiting threads
/// first, then the most recently started. `root_of` maps a `cwd` to its repository root. A
/// thread is listed once: its row says what its first run asked and how its latest run stands.
pub(crate) fn folders(
    runs: &[&RunSummary],
    root_of: impl Fn(&Path) -> Option<PathBuf>,
    inputs: &Inputs<'_>,
) -> Vec<Folder> {
    listing(runs, root_of, inputs).folders
}

/// [`folders`], and with [`Inputs::edit`] the pinned and archived threads taken out of them.
pub(crate) fn listing(
    runs: &[&RunSummary],
    root_of: impl Fn(&Path) -> Option<PathBuf>,
    inputs: &Inputs<'_>,
) -> Listing {
    let mut groups: Vec<(FolderKey, Vec<Entry<'_>>)> = Vec::new();
    for entry in entries(runs, inputs) {
        let key = entry
            .first()
            .cwd
            .as_deref()
            .and_then(&root_of)
            .map_or(FolderKey::NoProject, FolderKey::Repo);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push(entry),
            None => groups.push((key, vec![entry])),
        }
    }
    let latest = |list: &[Entry<'_>]| {
        list.iter()
            .map(|e| e.latest().updated_at_ms)
            .max()
            .unwrap_or(0)
    };
    groups.sort_by(|(ka, la), (kb, lb)| {
        (*ka == FolderKey::NoProject)
            .cmp(&(*kb == FolderKey::NoProject))
            .then_with(|| latest(lb).cmp(&latest(la)))
            .then_with(|| ka.cmp(kb))
    });
    let index_in = |list: &[ThreadId], entry: &Entry<'_>| {
        entry
            .thread
            .as_ref()
            .and_then(|t| list.iter().position(|p| p == t))
    };
    let mut out = Listing {
        folders: Vec::new(),
        pinned: Vec::new(),
        archived: Vec::new(),
    };
    // (rank in the pin or archive order, the row)
    let mut pinned_apart: Vec<(usize, ThreadRow)> = Vec::new();
    let mut archived: Vec<(usize, ThreadRow)> = Vec::new();
    for (key, mut list) in groups {
        let name = key.name();
        let row_of = |entry: &Entry<'_>| {
            let latest = entry.latest();
            let looking = inputs
                .looking_at
                .is_some_and(|l| entry.runs.iter().any(|r| &r.run_id == l));
            let marks = inputs.edit.map(|e| e.marks);
            let title = entry
                .thread
                .as_ref()
                .and_then(|t| marks.and_then(|m| m.titles.get(t)))
                .unwrap_or(&entry.first().title);
            ThreadRow {
                thread: entry.thread.clone(),
                run: latest.run_id.clone(),
                members: entry.runs.iter().map(|r| r.run_id.clone()).collect(),
                title: title.clone(),
                executor: latest.executor.clone(),
                dot: dot(latest, inputs.seen, looking),
                badge: badge(latest),
                age: summary::age_label(inputs.now_ms.saturating_sub(latest.created_at_ms)),
                pinned: marks.is_some_and(|m| index_in(&m.pinned, entry).is_some()),
                project: name.clone(),
                created_ms: entry.first().created_at_ms,
                updated_ms: latest.updated_at_ms,
            }
        };
        if let Some(edit) = inputs.edit {
            list.retain(|entry| {
                if let Some(at) = index_in(&edit.marks.archived, entry) {
                    archived.push((at, row_of(entry)));
                    return false;
                }
                if edit.pinned_apart
                    && let Some(at) = index_in(&edit.marks.pinned, entry)
                {
                    pinned_apart.push((at, row_of(entry)));
                    return false;
                }
                true
            });
        }
        // Pinned threads lead their folder (when they stay in it), in pin order.
        let pin_rank = |entry: &Entry<'_>| {
            inputs
                .edit
                .and_then(|e| index_in(&e.marks.pinned, entry))
                .unwrap_or(usize::MAX)
        };
        list.sort_by(|a, b| {
            pin_rank(a)
                .cmp(&pin_rank(b))
                .then_with(|| is_waiting(b.latest()).cmp(&is_waiting(a.latest())))
                .then_with(|| b.latest().created_at_ms.cmp(&a.latest().created_at_ms))
                .then_with(|| a.latest().run_id.cmp(&b.latest().run_id))
        });
        let open = key.open_by_default() != inputs.toggled.contains(&key);
        let waiting = list.iter().filter(|e| is_waiting(e.latest())).count();
        // Waiting threads, and pinned ones that stay, are always listed.
        let fixed = list
            .iter()
            .filter(|e| is_waiting(e.latest()) || pin_rank(e) != usize::MAX)
            .count();
        let expanded = open
            && list.len() > PER_FOLDER
            && inputs.edit.is_some_and(|e| e.expanded.contains(&key));
        let shown = if !open {
            fixed
        } else if expanded {
            list.len()
        } else {
            fixed.max(PER_FOLDER.min(list.len()))
        };
        let rows = list.iter().take(shown).map(row_of).collect();
        out.folders.push(Folder {
            name: name.clone(),
            open,
            waiting,
            rows,
            hidden: if open { list.len() - shown } else { 0 },
            expanded,
            key,
        });
    }
    pinned_apart.sort_by_key(|(at, _)| *at);
    archived.sort_by_key(|(at, _)| *at);
    out.pinned = pinned_apart.into_iter().map(|(_, row)| row).collect();
    out.archived = archived.into_iter().map(|(_, row)| row).collect();
    out
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
            model_options: Default::default(),
            effort: None,
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
            thread_of: &|_| None,
            edit: None,
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
    }

    fn thread_named<'a>(
        table: &'a [(&'a str, &'a str)],
    ) -> impl Fn(&RunId) -> Option<ThreadId> + 'a {
        move |run: &RunId| {
            table
                .iter()
                .find(|(r, _)| run.as_str().ends_with(r))
                .map(|(_, t)| ThreadId::from_suffix(t).unwrap())
        }
    }

    #[test]
    fn a_thread_is_one_row_titled_by_its_first_run_and_dotted_by_its_latest() {
        let first = run("a1", RunStatus::Succeeded, Some("/w/porch"), 100);
        let second = run("a2", RunStatus::WaitingApproval, Some("/w/porch"), 400);
        let other = run("b1", RunStatus::Succeeded, Some("/w/porch"), 200);
        let runs = vec![&second, &other, &first];
        let seen = Seen::new(0);
        let toggled = HashSet::new();
        let table = [("a1", "t1"), ("a2", "t1"), ("b1", "t2")];
        let thread_of = thread_named(&table);
        let inputs = Inputs {
            thread_of: &thread_of,
            ..inputs(&seen, &toggled)
        };
        let f = &folders(&runs, roots, &inputs)[0];
        assert_eq!(f.rows.len(), 2, "two threads, three runs");
        // Waiting first; the row opens the latest run, says what the first asked, shows the
        // latest run's state.
        let row = &f.rows[0];
        assert_eq!(row.run.as_str(), "run_a2");
        assert_eq!(row.title, "title a1");
        assert_eq!(row.dot, Dot::Waiting);
        assert_eq!(
            row.members.iter().map(RunId::as_str).collect::<Vec<_>>(),
            ["run_a1", "run_a2"]
        );
        assert_eq!(f.waiting, 1);
        assert_eq!(f.rows[1].run.as_str(), "run_b1");
        // Sitting on any run of the thread is "looking at it".
        let looking = RunId::from_suffix("a1").unwrap();
        let inputs = Inputs {
            looking_at: Some(&looking),
            ..inputs
        };
        let ended = run("a2", RunStatus::Succeeded, Some("/w/porch"), 400);
        let runs = vec![&ended, &first];
        let f = &folders(&runs, roots, &inputs)[0];
        assert_eq!(f.rows[0].dot, Dot::Seen);
    }

    #[test]
    fn a_closed_thread_stays_listed_as_history_and_a_run_without_a_thread_is_its_own_row() {
        let a = run("a1", RunStatus::Succeeded, Some("/w/porch"), 100);
        let b = run("b1", RunStatus::Succeeded, Some("/w/porch"), 200);
        let c = run("c1", RunStatus::Succeeded, Some("/w/porch"), 300);
        let runs = vec![&c, &b, &a];
        let seen = Seen::new(0);
        let toggled = HashSet::new();
        let table = [("a1", "t1"), ("b1", "t2")];
        let thread_of = thread_named(&table);
        // Closing a thread releases its worktree; the migration closes every old finished
        // run's thread, so a closed thread is history and must keep its row.
        let inputs = Inputs {
            thread_of: &thread_of,
            ..inputs(&seen, &toggled)
        };
        let f = &folders(&runs, roots, &inputs)[0];
        let listed: Vec<_> = f.rows.iter().map(|r| r.run.as_str()).collect();
        assert_eq!(listed, ["run_c1", "run_b1", "run_a1"]);
        assert_eq!(f.rows[0].thread, None);
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

    #[test]
    fn a_thread_row_says_in_words_what_it_waits_for() {
        let question = run("q", RunStatus::WaitingInput, Some("/w/porch"), 50);
        let held = run("h", RunStatus::WaitingApproval, Some("/w/porch"), 40);
        let busy = run("b", RunStatus::Running, Some("/w/porch"), 30);
        let done = run("d", RunStatus::Succeeded, Some("/w/porch"), 20);
        let runs = vec![&question, &held, &busy, &done];
        let seen = Seen::new(0);
        let toggled = HashSet::new();
        let rows = &folders(&runs, roots, &inputs(&seen, &toggled))[0].rows;
        let badge = |id: &str| {
            rows.iter()
                .find(|r| r.run.to_string() == format!("run_{id}"))
                .map(|r| r.badge)
                .unwrap()
        };
        assert_eq!(badge("q"), Some(Badge::NeedsInput));
        assert_eq!(badge("h"), Some(Badge::NeedsApproval));
        assert_eq!(badge("b"), None, "running shows a spinner, not a badge");
        assert_eq!(badge("d"), None);
        assert_eq!(Badge::NeedsInput.label(), "Needs input");
        assert_eq!(Badge::NeedsApproval.label(), "Needs approval");
    }

    #[test]
    fn a_thread_waits_on_its_latest_run_only() {
        let first = run("a1", RunStatus::WaitingInput, Some("/w/porch"), 10);
        let second = run("a2", RunStatus::Succeeded, Some("/w/porch"), 20);
        let runs = vec![&first, &second];
        let seen = Seen::new(0);
        let toggled = HashSet::new();
        let table = [("a1", "t1"), ("a2", "t1")];
        let of = thread_named(&table);
        let f = &folders(
            &runs,
            roots,
            &Inputs {
                thread_of: &of,
                ..inputs(&seen, &toggled)
            },
        )[0];
        assert_eq!(f.rows.len(), 1);
        assert_eq!(f.rows[0].badge, None);
    }

    /// A thread named `t<n>` for each run `r<n>`.
    fn named<'a>(table: &'a [(String, String)]) -> impl Fn(&RunId) -> Option<ThreadId> + 'a {
        move |run: &RunId| {
            table
                .iter()
                .find(|(r, _)| run.as_str().ends_with(r.as_str()))
                .map(|(_, t)| ThreadId::from_suffix(t).unwrap())
        }
    }

    fn many(n: u64) -> (Vec<RunSummary>, Vec<(String, String)>) {
        let runs = (0..n)
            .map(|i| run(&format!("r{i}"), RunStatus::Succeeded, Some("/w/reno"), i))
            .collect();
        let table = (0..n).map(|i| (format!("r{i}"), format!("t{i}"))).collect();
        (runs, table)
    }

    fn marks(pinned: &[&str], archived: &[&str]) -> ThreadMarks {
        let ids = |v: &[&str]| {
            v.iter()
                .map(|t| ThreadId::from_suffix(t).unwrap())
                .collect()
        };
        ThreadMarks {
            pinned: ids(pinned),
            archived: ids(archived),
            ..ThreadMarks::default()
        }
    }

    fn list(
        runs: &[RunSummary],
        table: &[(String, String)],
        marks: &ThreadMarks,
        expanded: &HashSet<FolderKey>,
        apart: bool,
    ) -> Listing {
        let refs: Vec<&RunSummary> = runs.iter().collect();
        let seen = Seen::new(0);
        let toggled = HashSet::new();
        let of = named(table);
        let edit = Edit {
            marks,
            expanded,
            pinned_apart: apart,
        };
        listing(
            &refs,
            roots,
            &Inputs {
                thread_of: &of,
                edit: Some(&edit),
                ..inputs(&seen, &toggled)
            },
        )
    }

    #[test]
    fn a_long_project_says_how_many_it_hides_and_lists_them_all_when_asked() {
        let (runs, table) = many(14);
        let none = HashSet::new();
        let l = list(&runs, &table, &ThreadMarks::default(), &none, true);
        let f = &l.folders[0];
        assert_eq!((f.rows.len(), f.hidden, f.expanded), (8, 6, false));
        let open: HashSet<_> = [f.key.clone()].into();
        let l = list(&runs, &table, &ThreadMarks::default(), &open, true);
        let f = &l.folders[0];
        assert_eq!((f.rows.len(), f.hidden, f.expanded), (14, 0, true));
        // A page or less has nothing to show or hide.
        let (few, table) = many(8);
        let l = list(&few, &table, &ThreadMarks::default(), &open, true);
        assert_eq!((l.folders[0].hidden, l.folders[0].expanded), (0, false));
        // Without the controls a folder lists a page, as ever.
        let refs: Vec<&RunSummary> = runs.iter().collect();
        let seen = Seen::new(0);
        let toggled = HashSet::new();
        let of = named(&table);
        let plain = folders(
            &refs,
            roots,
            &Inputs {
                thread_of: &of,
                ..inputs(&seen, &toggled)
            },
        );
        assert_eq!(plain[0].rows.len(), 8);
    }

    #[test]
    fn archived_threads_leave_their_folder_and_are_listed_apart_most_recent_first() {
        let (runs, table) = many(14);
        let m = marks(&[], &["t3", "t1"]);
        let l = list(&runs, &table, &m, &HashSet::new(), true);
        let f = &l.folders[0];
        assert!(
            f.rows
                .iter()
                .all(|r| r.run.as_str() != "run_r3" && r.run.as_str() != "run_r1")
        );
        assert_eq!(f.hidden, 12 - 8, "the archive is not counted as hidden");
        let archived: Vec<_> = l.archived.iter().map(|r| r.run.as_str()).collect();
        assert_eq!(archived, ["run_r3", "run_r1"]);
        assert_eq!(l.archived[0].project, "reno");
    }

    #[test]
    fn pinned_threads_lead_their_folder_or_stand_apart_in_pin_order() {
        let (runs, table) = many(14);
        let m = marks(&["t2", "t9"], &[]);
        // B: first in the folder, past the page cap, marked.
        let inline = list(&runs, &table, &m, &HashSet::new(), false);
        let f = &inline.folders[0];
        let lead: Vec<_> = f
            .rows
            .iter()
            .take(2)
            .map(|r| (r.run.as_str(), r.pinned))
            .collect();
        assert_eq!(lead, [("run_r2", true), ("run_r9", true)]);
        assert_eq!(f.rows.len(), 8);
        assert!(inline.pinned.is_empty());
        // A and C: out of the folder, in the pin order.
        let apart = list(&runs, &table, &m, &HashSet::new(), true);
        let pins: Vec<_> = apart.pinned.iter().map(|r| r.run.as_str()).collect();
        assert_eq!(pins, ["run_r2", "run_r9"]);
        assert!(apart.folders[0].rows.iter().all(|r| !r.pinned));
        assert_eq!(apart.folders[0].hidden, 12 - 8);
    }

    #[test]
    fn a_renamed_thread_shows_its_own_title() {
        let (runs, table) = many(2);
        let mut m = ThreadMarks::default();
        m.titles.insert(
            ThreadId::from_suffix("t1").unwrap(),
            "ACP, properly".to_owned(),
        );
        let l = list(&runs, &table, &m, &HashSet::new(), true);
        let titles: Vec<_> = l.folders[0].rows.iter().map(|r| r.title.as_str()).collect();
        assert!(titles.contains(&"ACP, properly") && titles.contains(&"title r0"));
    }
}
