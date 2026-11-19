//! Syntax colour for diffs: the cache, and the request that fills it.
//!
//! For each open file the screen asks `workspace.file_sources` once per patch (the diff's
//! `checksum` is part of the key, so a file that changed is read again), highlights both sides on
//! a background task, and keeps the result. Until it arrives, or on a daemon without feature
//! `history`, the lines draw plain.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{AppContext as _, Context};
use gpui_tokio::Tokio;
use reno_proto::{DiffScope, FileSourcesParams};

use super::RunScreen;
use crate::vm::diff_syntax::FileSyntax;

/// Entries kept; past this the cache starts over (a thread can touch many files).
const MAX_ENTRIES: usize = 256;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    /// Which list the file is in ("review:Thread", "commit:<sha>").
    origin: String,
    path: String,
    /// The checksum of the patch the colours were made for.
    checksum: String,
}

enum Entry {
    Loading,
    /// Nothing to colour: a binary file, no grammar, a stale answer or a failed read.
    Plain,
    Ready(Arc<FileSyntax>),
}

#[derive(Default)]
pub(super) struct Syntax {
    map: HashMap<Key, Entry>,
}

impl Syntax {
    /// The colours of `path` for the patch with `checksum`, once they are in.
    pub(super) fn ready(
        &self,
        origin: &str,
        path: &str,
        checksum: &str,
    ) -> Option<&Arc<FileSyntax>> {
        let key = Key {
            origin: origin.to_owned(),
            path: path.to_owned(),
            checksum: checksum.to_owned(),
        };
        match self.map.get(&key)? {
            Entry::Ready(syntax) => Some(syntax),
            Entry::Loading | Entry::Plain => None,
        }
    }

    #[cfg(test)]
    pub(super) fn ready_count(&self) -> usize {
        self.map
            .values()
            .filter(|e| matches!(e, Entry::Ready(_)))
            .count()
    }
}

impl RunScreen {
    /// Asks for the colours of `path` unless they are known or on their way. `scope` is what the
    /// diff compares; `checksum` is the one the patch came with (empty: the daemon sends none,
    /// so there is nothing to key the colours by).
    pub(super) fn ensure_syntax(
        &mut self,
        origin: &str,
        scope: Option<DiffScope>,
        path: &str,
        checksum: &str,
        cx: &mut Context<Self>,
    ) {
        if checksum.is_empty() || !self.store.read(cx).supports_history() {
            return;
        }
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        let key = Key {
            origin: origin.to_owned(),
            path: path.to_owned(),
            checksum: checksum.to_owned(),
        };
        if self.syntax.map.contains_key(&key) {
            return;
        }
        if self.syntax.map.len() >= MAX_ENTRIES {
            self.syntax.map.clear();
        }
        self.syntax.map.insert(key.clone(), Entry::Loading);
        let params = FileSourcesParams {
            run_id: self.run.clone(),
            path: path.to_owned(),
            scope,
            checksum: Some(checksum.to_owned()),
        };
        let task = Tokio::spawn(
            cx,
            async move { client.workspace_file_sources(params).await },
        );
        cx.spawn(async move |this, cx| {
            let answer = task.await;
            let mut stale = false;
            let built = match answer {
                Ok(Ok(sources)) if sources.stale => {
                    stale = true;
                    None
                }
                Ok(Ok(sources)) if !sources.binary => {
                    let at = key.path.clone();
                    Some(
                        cx.background_spawn(async move {
                            Arc::new(FileSyntax::build(&at, sources.old_text, sources.new_text))
                        })
                        .await,
                    )
                }
                _ => None,
            };
            this.update(cx, |screen, cx| {
                // The cache may have started over meanwhile.
                if let Some(entry) = screen.syntax.map.get_mut(&key) {
                    *entry = built.map_or(Entry::Plain, Entry::Ready);
                }
                if stale {
                    // The diff moved on under the patch held: read what it is now.
                    screen.reload_changes(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// How many lines of `path`'s patch in Review have syntax colour (tests).
    #[cfg(test)]
    pub(crate) fn coloured_lines(&self, path: &str, cx: &gpui::App) -> usize {
        use crate::vm::changes::PatchPane;
        let origin = self.review_origin(cx);
        let Some((loaded, PatchPane::Rows { rows, .. })) = self.patch_panes.get(path) else {
            return 0;
        };
        self.file_syntax(&origin, path, loaded)
            .map_or(0, |syntax| syntax.coloured(rows))
    }

    #[cfg(test)]
    pub(crate) fn syntax_ready(&self) -> usize {
        self.syntax.ready_count()
    }
}
