//! The app shell's pure parts: where the person has been (back and forward), what the thread chip
//! says, how many runs are live, and the gap under the status bar.
//!
//! None of this needs a window, so each rule is a unit test; `ui/bar.rs` and the root view only
//! draw what these say.

use std::path::{Path, PathBuf};

use reno_client::State;
use reno_proto::{RunId, RunStatus, RunSummary};

use super::projects::{self, FolderKey};
use crate::panel::layout::GUTTER;
use crate::summary;

/// The status bar's height: the title bar and the status line are one band.
pub(crate) const BAR_HEIGHT: f32 = 40.0;
/// The bar's controls (buttons, chips, the island).
pub(crate) const CONTROL: f32 = 28.0;
/// The widest the current-thread chip grows.
pub(crate) const CHIP_MAX: f32 = 300.0;

/// How far under the bar a canvas starts, so that its distance from the bar's controls is the
/// gutter: the one gap between and around canvases. The bar already leaves
/// `(BAR_HEIGHT - CONTROL) / 2` under its controls.
pub(crate) fn canvas_top_margin() -> f32 {
    (GUTTER - (BAR_HEIGHT - CONTROL) / 2.0).max(0.0)
}

/// Somewhere the back and forward buttons can return to. Settings is not one: it takes the
/// sidebar's place and "Back to app" leaves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// The new-thread state ("Start a task"), in a project when one was chosen.
    NewThread(Option<FolderKey>),
    /// All runs.
    AllRuns,
    Run(RunId),
    Project(FolderKey),
    Triggers,
}

/// The places visited, and where the person stands among them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct History {
    places: Vec<Place>,
    at: usize,
}

/// The most places kept; the oldest are forgotten first.
const KEEP: usize = 50;

impl History {
    /// A history that starts at `first`.
    pub(crate) fn new(first: Place) -> Self {
        Self {
            places: vec![first],
            at: 0,
        }
    }

    pub(crate) fn current(&self) -> &Place {
        &self.places[self.at]
    }

    /// The person went to `place` by their own action: what lay forward is dropped. Going to
    /// the place already shown records nothing.
    pub(crate) fn visit(&mut self, place: Place) {
        if self.current() == &place {
            return;
        }
        self.places.truncate(self.at + 1);
        self.places.push(place);
        if self.places.len() > KEEP {
            self.places.remove(0);
        }
        self.at = self.places.len() - 1;
    }

    pub(crate) fn can_back(&self) -> bool {
        self.at > 0
    }

    pub(crate) fn can_forward(&self) -> bool {
        self.at + 1 < self.places.len()
    }

    /// Steps back; the place to show, or `None` at the start.
    pub(crate) fn back(&mut self) -> Option<Place> {
        self.can_back().then(|| {
            self.at -= 1;
            self.places[self.at].clone()
        })
    }

    pub(crate) fn forward(&mut self) -> Option<Place> {
        self.can_forward().then(|| {
            self.at += 1;
            self.places[self.at].clone()
        })
    }
}

/// The current-thread chip: a letter on a tinted square, and a title.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Chip {
    pub(crate) initial: String,
    pub(crate) title: String,
}

fn initial_of(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect())
}

/// What the chip says at `place`: a thread by its first run's words and its project's initial.
pub(crate) fn chip(
    place: &Place,
    state: &State,
    root_of: impl Fn(&Path) -> Option<PathBuf>,
) -> Chip {
    match place {
        Place::NewThread(None) => Chip {
            initial: "+".to_owned(),
            title: "Start a task".to_owned(),
        },
        Place::NewThread(Some(key)) => Chip {
            initial: initial_of(&key.name()),
            title: format!("New thread \u{b7} {}", key.name()),
        },
        Place::AllRuns => Chip {
            initial: "A".to_owned(),
            title: "All runs".to_owned(),
        },
        Place::Triggers => Chip {
            initial: "T".to_owned(),
            title: "Triggers".to_owned(),
        },
        Place::Project(key) => Chip {
            initial: initial_of(&key.name()),
            title: key.name(),
        },
        Place::Run(run) => {
            let latest = state.run(run);
            let first = state
                .thread_of(run)
                .and_then(|t| state.thread(t))
                .and_then(|t| t.runs.first())
                .and_then(|r| state.run(&r.run_id))
                .or(latest);
            match first {
                Some(first) => Chip {
                    initial: initial_of(&projects::key_of(latest, root_of).name()),
                    title: summary::parse_title(&first.title).what.to_owned(),
                },
                None => Chip {
                    initial: "R".to_owned(),
                    title: format!("Run {run}"),
                },
            }
        }
    }
}

/// A run that is still going or waiting for the person: what "N live" counts and All runs lists.
/// (`summary::is_live` is narrower: the executor is working. A queued run and a run waiting
/// for you are live here because All runs must show them.)
pub(crate) fn counts_as_live(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Queued
            | RunStatus::Leased
            | RunStatus::Running
            | RunStatus::Evaluating
            | RunStatus::WaitingInput
            | RunStatus::WaitingApproval
    )
}

pub(crate) fn live_runs(state: &State) -> Vec<&RunSummary> {
    state
        .runs()
        .into_iter()
        .filter(|r| counts_as_live(r.status))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_client::Update;
    use reno_proto::TaskId;

    use super::*;

    fn run(id: &str, status: RunStatus, title: &str, cwd: Option<&str>) -> RunSummary {
        RunSummary {
            model_options: Default::default(),
            effort: None,
            run_id: RunId::from_suffix(id).unwrap(),
            task_id: TaskId::from_suffix(id).unwrap(),
            executor: "claude-code".parse().unwrap(),
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
            cwd: cwd.map(PathBuf::from),
            report: None,
            created_at_ms: 1,
            updated_at_ms: 2,
        }
    }

    fn state(runs: Vec<RunSummary>) -> State {
        let mut state = State::new();
        state.apply(Update::Runs(runs));
        state
    }

    fn repo(name: &str) -> FolderKey {
        FolderKey::Repo(PathBuf::from(format!("/w/{name}")))
    }

    #[test]
    fn canvases_start_one_gutter_under_the_controls() {
        // The bar leaves 6 pt under its 28 pt controls; the margin adds the rest of the 8 pt gutter.
        assert_eq!(BAR_HEIGHT, 40.0);
        assert_eq!(CONTROL, 28.0);
        assert_eq!((BAR_HEIGHT - CONTROL) / 2.0 + canvas_top_margin(), GUTTER);
    }

    #[test]
    fn back_and_forward_walk_the_places_visited() {
        let mut h = History::new(Place::NewThread(None));
        assert!(!h.can_back() && !h.can_forward());
        assert_eq!(h.back(), None);
        h.visit(Place::AllRuns);
        h.visit(Place::Triggers);
        assert!(h.can_back() && !h.can_forward());
        assert_eq!(h.back(), Some(Place::AllRuns));
        assert_eq!(h.back(), Some(Place::NewThread(None)));
        assert_eq!(h.back(), None);
        assert_eq!(h.forward(), Some(Place::AllRuns));
        assert_eq!(h.current(), &Place::AllRuns);
        assert!(h.can_forward());
    }

    #[test]
    fn going_somewhere_new_drops_what_lay_forward_and_the_same_place_twice_counts_once() {
        let mut h = History::new(Place::NewThread(None));
        h.visit(Place::AllRuns);
        h.visit(Place::Triggers);
        h.back();
        h.visit(Place::AllRuns);
        assert!(h.can_forward(), "the same place is not a new step");
        h.visit(Place::Project(repo("porch")));
        assert!(!h.can_forward(), "a new place drops what lay forward");
        assert_eq!(h.back(), Some(Place::AllRuns));
        h.visit(Place::Run(RunId::from_suffix("a").unwrap()));
        assert!(!h.can_forward());
        assert_eq!(h.back(), Some(Place::AllRuns));
    }

    #[test]
    fn the_history_keeps_the_latest_fifty_places() {
        let mut h = History::new(Place::NewThread(None));
        for i in 0..80 {
            h.visit(Place::Run(RunId::from_suffix(&format!("r{i}")).unwrap()));
        }
        let mut steps = 0;
        while h.back().is_some() {
            steps += 1;
        }
        assert_eq!(steps, 49);
    }

    #[test]
    fn the_chip_names_the_place_and_a_thread_by_its_project_initial() {
        let s = state(vec![run(
            "a",
            RunStatus::Running,
            "Fix the retry",
            Some("/w/porch/src"),
        )]);
        let root = |dir: &Path| {
            dir.starts_with("/w/porch")
                .then(|| PathBuf::from("/w/porch"))
        };
        let chip_at = |p: Place| chip(&p, &s, root);
        assert_eq!(
            chip_at(Place::NewThread(None)),
            Chip {
                initial: "+".into(),
                title: "Start a task".into()
            }
        );
        assert_eq!(
            chip_at(Place::NewThread(Some(repo("porch")))),
            Chip {
                initial: "P".into(),
                title: "New thread \u{b7} porch".into()
            }
        );
        assert_eq!(chip_at(Place::AllRuns).title, "All runs");
        assert_eq!(chip_at(Place::Triggers).title, "Triggers");
        assert_eq!(
            chip_at(Place::Run(RunId::from_suffix("a").unwrap())),
            Chip {
                initial: "P".into(),
                title: "Fix the retry".into()
            }
        );
        // A run the app has not heard of yet still has a chip.
        let unknown = chip_at(Place::Run(RunId::from_suffix("zz").unwrap()));
        assert_eq!(unknown.title, "Run run_zz");
    }

    #[test]
    fn live_counts_what_is_going_or_waiting_for_you() {
        let s = state(vec![
            run("q", RunStatus::Queued, "a", None),
            run("r", RunStatus::Running, "b", None),
            run("i", RunStatus::WaitingInput, "c", None),
            run("p", RunStatus::WaitingApproval, "d", None),
            run("d", RunStatus::Succeeded, "e", None),
            run("f", RunStatus::Failed, "f", None),
            run("c", RunStatus::Cancelled, "g", None),
            run("n", RunStatus::Interrupted, "h", None),
        ]);
        assert_eq!(live_runs(&s).len(), 4);
        assert!(state(vec![]).runs().is_empty());
        assert!(live_runs(&state(vec![])).is_empty());
    }
}
