//! What a click on a file in a reply does (TRANSCRIPT-8.1): the reply's files are looked up in the
//! run's worktree (one `fs.list` per directory, once), and a link or an inline-code path that
//! names one opens in the side panel's Files tab at its line. Until a directory has answered, or
//! when it has no such file, the text stays unlinked.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use gpui::{App, Context, WeakEntity, Window};
use reno_markdown::BlockTree;
use reno_proto::FsListParams;

use super::RunScreen;
use crate::transcript::Key;
use crate::ui::markdown_view::FileLinks;
use crate::ui::markdown_view::link::{self, FileTarget};
use crate::ui::markdown_view::lru::Lru;

/// Pages of a directory read before the rest is taken as not there.
const PAGES: usize = 5;

enum Dir {
    Loading,
    Files(HashSet<String>),
    Failed,
}

/// Directories kept before the least recently asked about go; one asked about again is listed
/// again.
const DIRS: usize = 256;

/// The files of the directories a reply named.
pub(super) struct FileIndex {
    dirs: Lru<String, Dir>,
}

impl Default for FileIndex {
    fn default() -> Self {
        Self {
            dirs: Lru::new(DIRS, usize::MAX),
        }
    }
}

pub(super) type Index = Rc<RefCell<FileIndex>>;

impl FileIndex {
    fn has(&mut self, path: &str) -> bool {
        let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
        matches!(self.dirs.get(&dir.to_owned()), Some(Dir::Files(files)) if files.contains(path))
    }
}

struct OpenInFiles {
    screen: WeakEntity<RunScreen>,
    index: Index,
}

impl FileLinks for OpenInFiles {
    fn resolves(&self, path: &str, _cx: &App) -> bool {
        self.index.borrow_mut().has(path)
    }

    fn open(&self, target: FileTarget, window: &mut Window, cx: &mut App) {
        self.screen
            .update(cx, |screen, cx| {
                screen.open_file_at(&target.path, target.line, window, cx);
            })
            .ok();
    }
}

impl RunScreen {
    /// How many directories the index holds.
    #[cfg(test)]
    pub(super) fn probed_dirs(&self) -> usize {
        self.file_index.borrow().dirs.len()
    }

    /// Whether the worktree is known to have `path`.
    #[cfg(test)]
    pub(super) fn file_resolves(&self, path: &str, cx: &App) -> bool {
        self.file_links_for_test().resolves(path, cx)
    }

    #[cfg(test)]
    fn file_links_for_test(&self) -> OpenInFiles {
        OpenInFiles {
            screen: WeakEntity::new_invalid(),
            index: self.file_index.clone(),
        }
    }

    /// The host's side of file links for the replies of this run.
    pub(super) fn file_links(&self, cx: &mut Context<Self>) -> Rc<dyn FileLinks> {
        Rc::new(OpenInFiles {
            screen: cx.weak_entity(),
            index: self.file_index.clone(),
        })
    }

    /// Asks the worktree about the files `tree` names, the first time its text is seen.
    pub(super) fn probe_files(
        &mut self,
        key: Key,
        text_len: usize,
        tree: &BlockTree,
        cx: &mut Context<Self>,
    ) {
        if self.probed.get(&key) == Some(&text_len) {
            return;
        }
        self.probed.insert(key, text_len, 1);
        let mut dirs: Vec<String> = link::paths_in(tree)
            .iter()
            .map(|p| {
                p.rsplit_once('/')
                    .map_or(String::new(), |(d, _)| d.to_owned())
            })
            .collect();
        dirs.sort();
        dirs.dedup();
        for dir in dirs {
            if self.file_index.borrow().dirs.contains_key(&dir) {
                continue;
            }
            self.file_index
                .borrow_mut()
                .dirs
                .insert(dir.clone(), Dir::Loading, 1);
            let (ask, named) = (dir.clone(), dir);
            self.files_call(
                cx,
                move |client, run| async move {
                    let mut files = HashSet::new();
                    let mut cursor = None;
                    for _ in 0..PAGES {
                        let page = client
                            .fs_list(FsListParams {
                                run_id: run.clone(),
                                dir: ask.clone(),
                                cursor: cursor.clone(),
                                limit: None,
                            })
                            .await?;
                        files.extend(
                            page.entries
                                .into_iter()
                                .filter(|e| e.kind == reno_proto::FsEntryKind::File)
                                .map(|e| e.path),
                        );
                        cursor = page.next_cursor;
                        if cursor.is_none() {
                            break;
                        }
                    }
                    Ok(files)
                },
                move |this, result, cx| {
                    let found = result.map_or(Dir::Failed, Dir::Files);
                    this.file_index.borrow_mut().dirs.insert(named, found, 1);
                    cx.notify();
                },
            );
        }
    }
}
