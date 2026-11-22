//! The fake daemon's side of the mocked agent-posted-visuals contract (see [`crate::visuals`]):
//! the Render check run posts one image and one diagram, each with a caption, after its reply.

use std::collections::HashMap;

use reno_proto::{RunId, Seq};

use super::FakeDaemon;
use super::transcript::RENDER_RUN;
use crate::visuals::{Visual, VisualDesk, VisualImage, VisualKind};

/// The posted visuals, by run.
pub(super) type Sim = HashMap<RunId, Vec<Visual>>;

/// The seq of the Render check run's reply: `Started` is 1, the reply 2.
const RENDER_REPLY_SEQ: u64 = 2;

/// The mermaid source the Render check run posts.
const POSTED_DIAGRAM: &str = "flowchart TD
    A[Agent] -->|posts| B[Diagram]
    B --> C{Reads well?}
    C -->|yes| D[Keep]
    C -->|no| E[Redraw]";

pub(super) fn seed(on: bool) -> Sim {
    let mut sim = Sim::new();
    if !on {
        return sim;
    }
    if let Ok(run) = RunId::from_suffix(RENDER_RUN) {
        sim.insert(
            run,
            vec![
                Visual {
                    id: "v1".to_owned(),
                    caption: "The build graph, as the agent drew it".to_owned(),
                    after: Seq(RENDER_REPLY_SEQ),
                    kind: VisualKind::Image(VisualImage::Worktree("docs/graph.png".to_owned())),
                },
                Visual {
                    id: "v2".to_owned(),
                    caption: "How a posted diagram is reviewed".to_owned(),
                    after: Seq(RENDER_REPLY_SEQ),
                    kind: VisualKind::Diagram(POSTED_DIAGRAM.to_owned()),
                },
            ],
        );
    }
    sim
}

impl VisualDesk for FakeDaemon {
    fn visuals(&self, run: &RunId) -> Vec<Visual> {
        self.visuals.get(run).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeConfig;

    fn daemon(transcript_threads: bool) -> FakeDaemon {
        FakeDaemon::start(FakeConfig {
            transcript_threads,
            ..FakeConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn the_render_check_run_posts_an_image_and_a_diagram_with_captions() {
        let daemon = daemon(true);
        let run = RunId::from_suffix(RENDER_RUN).unwrap();
        let visuals = daemon.visuals(&run);
        assert_eq!(visuals.len(), 2);
        assert!(matches!(
            &visuals[0].kind,
            VisualKind::Image(VisualImage::Worktree(path)) if path == "docs/graph.png"
        ));
        assert!(
            matches!(&visuals[1].kind, VisualKind::Diagram(src) if src.starts_with("flowchart"))
        );
        assert!(visuals.iter().all(|v| !v.caption.is_empty()));
        assert_ne!(visuals[0].id, visuals[1].id);
    }

    #[test]
    fn visuals_are_anchored_after_an_event_the_run_has() {
        let daemon = daemon(true);
        let run = RunId::from_suffix(RENDER_RUN).unwrap();
        let journal = super::super::transcript::runs().unwrap();
        let events = journal
            .iter()
            .find(|r| r.summary.run_id == run)
            .map(|r| r.journal.len())
            .unwrap();
        for visual in daemon.visuals(&run) {
            assert!(visual.after.0 >= 1 && visual.after.0 <= events as u64);
        }
    }

    #[test]
    fn other_runs_and_a_world_without_the_demo_post_none() {
        let on = daemon(true);
        assert!(on.visuals(&RunId::from_suffix("acp1").unwrap()).is_empty());
        let off = daemon(false);
        assert!(
            off.visuals(&RunId::from_suffix(RENDER_RUN).unwrap())
                .is_empty()
        );
    }
}
