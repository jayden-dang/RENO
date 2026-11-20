//! The fake daemon's side of the mocked thread-desk contract (see [`crate::thread_desk`]). A
//! demo world started with `FakeConfig::sidebar_threads` begins with `plan1` ("fix the ACP v2
//! handshake") and `reviewer1` ("review PR 214") pinned and four threads archived. Whether a
//! thread has a live run is read from the world, so stopping the run lets the archive through.

use std::sync::Mutex;

use reno_proto::ThreadId;

use super::FakeDaemon;
use crate::thread_desk::{ThreadDesk, ThreadEditError, ThreadMarks};

#[derive(Debug, Default)]
struct Inner {
    marks: ThreadMarks,
}

#[derive(Debug, Default)]
pub(super) struct Sim(Mutex<Inner>);

/// The pinned threads of the demo, the most recently pinned first.
const PINNED: [&str; 2] = ["plan1", "reviewer1"];
/// The archived threads of the demo, the most recently archived first.
const ARCHIVED: [&str; 4] = ["arch1", "arch2", "arch3", "arch4"];

pub(super) fn seed(on: bool) -> Sim {
    let sim = Sim::default();
    if on {
        let mut inner = sim
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ids = |names: &[&str]| -> Vec<ThreadId> {
            names
                .iter()
                .filter_map(|n| ThreadId::from_suffix(n).ok())
                .collect()
        };
        inner.marks.pinned = ids(&PINNED);
        inner.marks.archived = ids(&ARCHIVED);
    }
    sim
}

impl FakeDaemon {
    fn desk(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.thread_desk
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `Some(live)` for a thread the world knows, `None` for one it does not.
    fn thread_live(&self, thread: &ThreadId) -> Option<bool> {
        self.shared.read(|w| w.thread_live(thread))
    }
}

impl ThreadDesk for FakeDaemon {
    fn marks(&self) -> ThreadMarks {
        self.desk().marks.clone()
    }

    fn archived(&self) -> Vec<ThreadId> {
        self.desk().marks.archived.clone()
    }

    fn rename(&self, thread: &ThreadId, title: &str) -> Result<String, ThreadEditError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(ThreadEditError::NeedsTitle);
        }
        self.thread_live(thread)
            .ok_or(ThreadEditError::UnknownThread)?;
        self.desk()
            .marks
            .titles
            .insert(thread.clone(), title.to_owned());
        Ok(title.to_owned())
    }

    fn set_pinned(&self, thread: &ThreadId, pinned: bool) -> Result<(), ThreadEditError> {
        self.thread_live(thread)
            .ok_or(ThreadEditError::UnknownThread)?;
        let mut inner = self.desk();
        let marks = &mut inner.marks;
        if pinned {
            if marks.archived.contains(thread) {
                return Err(ThreadEditError::Archived);
            }
            if !marks.pinned.contains(thread) {
                marks.pinned.insert(0, thread.clone());
            }
        } else {
            marks.pinned.retain(|t| t != thread);
        }
        Ok(())
    }

    fn set_archived(&self, thread: &ThreadId, archived: bool) -> Result<(), ThreadEditError> {
        let live = self
            .thread_live(thread)
            .ok_or(ThreadEditError::UnknownThread)?;
        let mut inner = self.desk();
        let marks = &mut inner.marks;
        if archived {
            if live {
                return Err(ThreadEditError::LiveRun);
            }
            marks.pinned.retain(|t| t != thread);
            if !marks.archived.contains(thread) {
                marks.archived.insert(0, thread.clone());
            }
        } else {
            marks.archived.retain(|t| t != thread);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeConfig;

    fn daemon() -> FakeDaemon {
        FakeDaemon::start(FakeConfig {
            activity_runs: true,
            question_run: true,
            question_demo_runs: true,
            sidebar_threads: true,
            ..FakeConfig::default()
        })
        .unwrap()
    }

    fn thread(suffix: &str) -> ThreadId {
        ThreadId::from_suffix(suffix).unwrap()
    }

    fn names(ids: Vec<ThreadId>) -> Vec<String> {
        ids.iter()
            .map(|t| t.as_str().trim_start_matches("thr_").to_owned())
            .collect()
    }

    #[test]
    fn the_demo_starts_with_two_pinned_and_four_archived() {
        let d = daemon();
        assert_eq!(names(d.marks().pinned), ["plan1", "reviewer1"]);
        assert_eq!(names(d.archived()), ["arch1", "arch2", "arch3", "arch4"]);
        // Off, the desk starts empty: existing worlds are unchanged.
        let plain = FakeDaemon::start(FakeConfig::default()).unwrap();
        assert_eq!(plain.marks(), ThreadMarks::default());
    }

    #[test]
    fn a_rename_keeps_the_trimmed_title_and_an_empty_one_is_refused() {
        let d = daemon();
        let t = thread("scout1");
        assert_eq!(
            d.rename(&t, "  Check advisories  ").unwrap(),
            "Check advisories"
        );
        assert_eq!(d.marks().titles[&t], "Check advisories");
        for blank in ["", "   ", "\n\t"] {
            assert_eq!(d.rename(&t, blank), Err(ThreadEditError::NeedsTitle));
        }
        assert_eq!(
            d.marks().titles[&t],
            "Check advisories",
            "the old title stays"
        );
        assert_eq!(
            ThreadEditError::NeedsTitle.to_string(),
            "A thread needs a title"
        );
        assert_eq!(
            d.rename(&thread("nosuch"), "x"),
            Err(ThreadEditError::UnknownThread)
        );
    }

    #[test]
    fn a_thread_with_a_live_run_cannot_be_archived() {
        let d = daemon();
        // plan1 is running; so is builder1, and keeper1 waits on an approval.
        for live in ["plan1", "builder1", "keeper1"] {
            assert_eq!(
                d.set_archived(&thread(live), true),
                Err(ThreadEditError::LiveRun),
                "{live}"
            );
        }
        assert_eq!(
            ThreadEditError::LiveRun.to_string(),
            "Stop the run before archiving"
        );
        assert_eq!(d.archived().len(), 4, "nothing was archived");
        assert_eq!(
            names(d.marks().pinned),
            ["plan1", "reviewer1"],
            "plan1 keeps its pin"
        );
    }

    #[test]
    fn stopping_the_run_lets_the_archive_through() {
        let d = daemon();
        let plan = thread("plan1");
        d.shared
            .with(|w| w.cancel(&reno_proto::RunId::from_suffix("plan1").unwrap(), None));
        d.set_archived(&plan, true).unwrap();
        assert_eq!(
            names(d.archived())[0],
            "plan1",
            "the newest archived is first"
        );
        assert!(!d.marks().pinned.contains(&plan), "archiving unpins");
    }

    #[test]
    fn pins_go_most_recent_first_and_pinning_twice_keeps_the_place() {
        let d = daemon();
        d.set_pinned(&thread("scout1"), true).unwrap();
        d.set_pinned(&thread("asker1"), true).unwrap();
        assert_eq!(
            names(d.marks().pinned),
            ["asker1", "scout1", "plan1", "reviewer1"]
        );
        d.set_pinned(&thread("scout1"), true).unwrap();
        assert_eq!(names(d.marks().pinned)[1], "scout1");
        d.set_pinned(&thread("scout1"), false).unwrap();
        d.set_pinned(&thread("scout1"), false).unwrap();
        assert_eq!(names(d.marks().pinned), ["asker1", "plan1", "reviewer1"]);
    }

    #[test]
    fn the_archived_list_follows_archive_and_unarchive() {
        let d = daemon();
        d.set_archived(&thread("reviewer1"), true).unwrap();
        assert_eq!(
            names(d.archived()),
            ["reviewer1", "arch1", "arch2", "arch3", "arch4"]
        );
        assert!(
            !d.marks().pinned.contains(&thread("reviewer1")),
            "an archived thread is not pinned"
        );
        d.set_archived(&thread("arch2"), false).unwrap();
        assert_eq!(
            names(d.archived()),
            ["reviewer1", "arch1", "arch3", "arch4"]
        );
        // Archiving twice does not list it twice.
        d.set_archived(&thread("arch1"), true).unwrap();
        assert_eq!(d.archived().len(), 4);
        assert_eq!(
            d.set_pinned(&thread("arch1"), true),
            Err(ThreadEditError::Archived)
        );
    }

    #[test]
    fn the_demo_has_fourteen_plain_atlas_api_threads() {
        let d = daemon();
        let runs = d.shared.read(|w| w.summaries());
        let in_atlas = runs
            .iter()
            .filter(|r| r.title.contains("atlas-api:"))
            .filter(|r| {
                let t = format!("thr_{}", r.run_id.as_str().trim_start_matches("run_"));
                let marks = d.marks();
                !marks.archived.iter().any(|a| a.as_str() == t)
                    && !marks.pinned.iter().any(|a| a.as_str() == t)
            })
            .count();
        assert_eq!(in_atlas, 14, "Show 6 more after the first eight");
    }
}
