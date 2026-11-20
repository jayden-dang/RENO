//! The fake daemon's side of the mocked places contract (see [`crate::places`]): three projects
//! (`reno` has 34 branches and two worktrees), two recents, and a clone that succeeds for a
//! GitHub-looking URL and fails for anything else.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use reno_proto::RunId;

use super::FakeDaemon;
use crate::places::{Branch, BranchPage, CloneError, Place, PlaceDesk, Project, Worktree};

/// A clone that works takes this long; one that fails is found out sooner.
const CLONE_MS: u64 = 1_500;
const CLONE_FAIL_MS: u64 = 700;

#[derive(Debug, Default)]
struct Inner {
    cloned: Vec<Project>,
    placed: HashMap<RunId, Place>,
}

#[derive(Debug, Default)]
pub(super) struct Sim(Mutex<Inner>);

fn project(name: &str) -> Project {
    Project {
        name: name.to_owned(),
        path: PathBuf::from(format!("/demo/{name}")),
    }
}

/// The names of a project's branches, default first.
fn branch_names(project: &str) -> Vec<String> {
    let mut names: Vec<String> = match project {
        "reno" => ["main", "feat/run-activity", "fix/model-chip", "release/0.4"]
            .map(str::to_owned)
            .into(),
        "web-console" => ["main", "feat/dark-mode", "fix/login-redirect"]
            .map(str::to_owned)
            .into(),
        "atlas-api" => ["main", "fix/pagination", "release/2.1"]
            .map(str::to_owned)
            .into(),
        _ => vec!["main".to_owned()],
    };
    let more = match project {
        "reno" => 30,
        "web-console" => 9,
        "atlas-api" => 4,
        _ => 0,
    };
    names.extend((1..=more).map(|n| format!("feat/topic-{n:02}")));
    names
}

fn worktrees_of(project: &str) -> Vec<Worktree> {
    if project != "reno" {
        return Vec::new();
    }
    [
        ("reno-run-activity", "feat/run-activity"),
        ("reno-model-chip", "fix/model-chip"),
    ]
    .into_iter()
    .map(|(name, branch)| Worktree {
        name: name.to_owned(),
        path: PathBuf::from(format!("/demo/reno.worktrees/{name}")),
        branch: branch.to_owned(),
    })
    .collect()
}

/// `https://github.com/owner/repo(.git)` or `git@github.com:owner/repo(.git)`: the repo's name.
fn github_repo(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let (owner, repo) = rest.trim_end_matches('/').split_once('/')?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/')).then(|| repo.to_owned())
}

impl FakeDaemon {
    fn places(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.places
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl PlaceDesk for FakeDaemon {
    fn projects(&self) -> Vec<Project> {
        let mut all: Vec<Project> = ["reno", "web-console", "atlas-api"].map(project).into();
        all.extend(self.places().cloned.clone());
        all
    }

    fn recent_projects(&self) -> Vec<Project> {
        let mut recent = self.places().cloned.clone();
        recent.reverse();
        recent.extend(["reno", "atlas-api"].map(project));
        recent
    }

    fn branches(&self, project: &str, query: &str, limit: usize) -> BranchPage {
        let worktrees = worktrees_of(project);
        let query = query.to_lowercase();
        let matching: Vec<Branch> = branch_names(project)
            .into_iter()
            .enumerate()
            .filter(|(_, name)| name.to_lowercase().contains(&query))
            .map(|(at, name)| Branch {
                default: at == 0,
                worktree: worktrees
                    .iter()
                    .find(|w| w.branch == name)
                    .map(|w| w.name.clone()),
                name,
            })
            .collect();
        BranchPage {
            total: matching.len(),
            branches: matching.into_iter().take(limit).collect(),
        }
    }

    fn worktrees(&self, project: &str) -> Vec<Worktree> {
        worktrees_of(project)
    }

    fn clone_duration_ms(&self, url: &str) -> u64 {
        if github_repo(url).is_some() {
            CLONE_MS
        } else {
            CLONE_FAIL_MS
        }
    }

    fn clone_project(&self, url: &str) -> Result<Project, CloneError> {
        if url.trim().is_empty() {
            return Err(CloneError::Empty);
        }
        let name = github_repo(url).ok_or(CloneError::NotFound)?;
        let cloned = project(&name);
        let mut inner = self.places();
        if !inner.cloned.iter().any(|p| p.name == name) {
            inner.cloned.push(cloned.clone());
        }
        Ok(cloned)
    }

    fn record_place(&self, run: &RunId, place: Place) {
        self.places().placed.insert(run.clone(), place);
    }

    fn run_place(&self, run: &RunId) -> Option<Place> {
        self.places().placed.get(run).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::places::Checkout;

    fn daemon() -> FakeDaemon {
        FakeDaemon::start(super::super::FakeConfig::default()).unwrap()
    }

    #[test]
    fn reno_shows_20_of_34_branches_with_the_default_first() {
        let page = daemon().branches("reno", "", 20);
        assert_eq!((page.branches.len(), page.total), (20, 34));
        assert_eq!(page.branches[0].name, "main");
        assert!(page.branches[0].default);
        assert_eq!(
            page.branches[2].worktree.as_deref(),
            Some("reno-model-chip")
        );
    }

    #[test]
    fn the_total_follows_the_query_not_the_limit() {
        let d = daemon();
        let page = d.branches("reno", "topic", 20);
        assert_eq!((page.branches.len(), page.total), (20, 30));
        let one = d.branches("reno", "RELEASE", 20);
        assert_eq!((one.branches.len(), one.total), (1, 1));
        let small = d.branches("atlas-api", "", 20);
        assert_eq!((small.branches.len(), small.total), (7, 7));
    }

    #[test]
    fn projects_recents_and_worktrees() {
        let d = daemon();
        let names = |v: Vec<Project>| v.into_iter().map(|p| p.name).collect::<Vec<_>>();
        assert_eq!(names(d.projects()), ["reno", "web-console", "atlas-api"]);
        assert_eq!(names(d.recent_projects()), ["reno", "atlas-api"]);
        assert_eq!(d.worktrees("reno").len(), 2);
        assert!(d.worktrees("atlas-api").is_empty());
    }

    #[test]
    fn a_github_url_clones_after_1500_ms_and_the_project_is_known_after() {
        let d = daemon();
        let url = "https://github.com/jaydendang/atlas-docs.git";
        assert_eq!(d.clone_duration_ms(url), 1_500);
        let cloned = d.clone_project(url).unwrap();
        assert_eq!(cloned.name, "atlas-docs");
        assert!(d.projects().iter().any(|p| p.name == "atlas-docs"));
        assert_eq!(d.recent_projects()[0].name, "atlas-docs");
        // Twice does not list it twice.
        d.clone_project(url).unwrap();
        assert_eq!(
            d.projects()
                .iter()
                .filter(|p| p.name == "atlas-docs")
                .count(),
            1
        );
    }

    #[test]
    fn any_other_url_fails_with_a_reason_and_adds_nothing() {
        let d = daemon();
        for url in [
            "https://example.com/nope.git",
            "github.com/a/b",
            "https://github.com/solo",
        ] {
            assert_eq!(d.clone_project(url), Err(CloneError::NotFound), "{url}");
        }
        assert_eq!(
            CloneError::NotFound.to_string(),
            "Could not clone: repository not found"
        );
        assert_eq!(d.clone_project("  "), Err(CloneError::Empty));
        assert_eq!(d.projects().len(), 3);
    }

    #[test]
    fn a_started_runs_place_is_kept_for_the_session_tab() {
        let d = daemon();
        let run = RunId::from_suffix("plan1").unwrap();
        assert_eq!(d.run_place(&run), None);
        let place = Place {
            project: "reno".into(),
            branch: "main".into(),
            checkout: Checkout::NewWorktree,
        };
        d.record_place(&run, place.clone());
        assert_eq!(d.run_place(&run), Some(place));
    }
}
