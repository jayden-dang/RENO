//! The mocked visuals contract: images or diagrams an agent posts into a run and images returned
//! by tool calls. The fake daemon implements [`VisualDesk`]; the app reads it through `Store`.

use reno_proto::{RunId, Seq};

/// Where an image visual's bytes are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisualImage {
    /// A file in the run's worktree, relative to its root, read through the daemon's file read.
    Worktree(String),
    /// The bytes themselves, with their MIME type (`image/png`).
    Data { mime: String, bytes: Vec<u8> },
}

/// What a visual shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisualKind {
    Image(VisualImage),
    /// A Mermaid diagram's source.
    Diagram(String),
}

/// One image or diagram a run posted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Visual {
    /// Stable within the run.
    pub id: String,
    /// Its title in the transcript.
    pub caption: String,
    /// The event of the run's journal it appears after.
    pub after: Seq,
    pub kind: VisualKind,
}

/// The posted visuals and tool-result images of a run.
pub trait VisualDesk: Send + Sync {
    /// The run's visuals in the order they were posted (and so by `after`). Empty when the run
    /// posted none.
    fn visuals(&self, run: &RunId) -> Vec<Visual>;

    fn tool_images(&self, run: &RunId, call: &str) -> Vec<VisualImage>;
}
