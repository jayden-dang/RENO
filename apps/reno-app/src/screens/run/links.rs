//! What a click on a file in a reply does (TRANSCRIPT-8.1): the reply's files are looked up in the
//! run's worktree (one `fs.list` per directory, once), and a link or an inline-code path that
//! names one opens in the side panel's Files tab at its line. Until a directory has answered, or
//! when it has no such file, the text stays unlinked.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;

use gpui::{App, Context, Task, WeakEntity, Window};
use gpui_tokio::Tokio;
use reno_client::ClientError;
use reno_markdown::BlockTree;
use reno_proto::{FsListParams, RunId};

use super::RunScreen;
use crate::transcript::{Key, ToolEntry};
use crate::ui::markdown_view::FileLinks;
use crate::ui::markdown_view::link::{self, FileTarget};
use crate::ui::markdown_view::lru::Lru;

const RESOURCE_LIMIT: usize = 16;

/// Candidate file resources named in a tool's retained output.
pub(super) fn result_paths(output: &str) -> Vec<FileTarget> {
    let mut unique = link::file_targets_in(&reno_markdown::parse_full(output));
    unique.truncate(RESOURCE_LIMIT);
    if unique.len() == RESOURCE_LIMIT {
        return unique;
    }
    for token in output.split_whitespace() {
        let token = token.trim_matches(['`', '"', '\u{27}', '(', ')', '[', ']', ',', ';', ':']);
        if !token.contains("](")
            && link::looks_like_path(token)
            && let Some(target) = link::file_target(token)
            && !unique.iter().any(|known: &FileTarget| *known == target)
        {
            unique.push(target);
            if unique.len() == RESOURCE_LIMIT {
                break;
            }
        }
    }
    unique
}

/// Pages of a directory read before the rest is taken as not there.
const PAGES: usize = 5;

enum Dir {
    Loading(Task<()>),
    Files(HashSet<String>),
    Failed,
}

/// Directories kept before the least recently asked about go; one asked about again is listed
/// again.
const DIRS: usize = 256;

const RESULT_CACHE_BYTES: usize = 4 * 1024 * 1024;
const NEW_DIRS_PER_GROUP: usize = 64;

struct ResultResources {
    version: u64,
    targets: HashMap<String, Rc<[FileTarget]>>,
}

/// The files of the directories a reply named.
pub(super) struct FileIndex {
    dirs: Lru<(RunId, String), Dir>,
    results: Lru<Key, ResultResources>,
}

impl Default for FileIndex {
    fn default() -> Self {
        Self {
            dirs: Lru::new(DIRS, usize::MAX),
            results: Lru::new(DIRS, RESULT_CACHE_BYTES),
        }
    }
}

pub(super) type Index = Rc<RefCell<FileIndex>>;

impl FileIndex {
    fn has(&mut self, run: &RunId, path: &str) -> bool {
        let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
        matches!(self.dirs.get(&(run.clone(), dir.to_owned())), Some(Dir::Files(files)) if files.contains(path))
    }
}

struct OpenInFiles {
    screen: WeakEntity<RunScreen>,
    index: Index,
    run: RunId,
}

impl FileLinks for OpenInFiles {
    fn resolves(&self, path: &str, _cx: &App) -> bool {
        self.index.borrow_mut().has(&self.run, path)
    }

    fn open(&self, target: FileTarget, window: &mut Window, cx: &mut App) {
        self.screen
            .update(cx, |screen, cx| {
                screen.open_file_at_from(&self.run, &target.path, target.line, window, cx);
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
            run: self.run.clone(),
        }
    }

    /// The host's side of file links for the replies of this run.
    #[cfg(test)]
    pub(super) fn file_links(&self, cx: &mut Context<Self>) -> Rc<dyn FileLinks> {
        self.file_links_for(self.run.clone(), cx)
    }

    pub(super) fn file_links_for(&self, run: RunId, cx: &mut Context<Self>) -> Rc<dyn FileLinks> {
        Rc::new(OpenInFiles {
            screen: cx.weak_entity(),
            index: self.file_index.clone(),
            run,
        })
    }

    pub(super) fn result_targets(&self, key: Key, id: &str) -> Option<Rc<[FileTarget]>> {
        self.file_index
            .borrow_mut()
            .results
            .get(&key)?
            .targets
            .get(id)
            .cloned()
    }

    pub(super) fn result_directories(&self, key: Key, calls: &[ToolEntry]) -> Vec<String> {
        let outputs: Vec<_> = calls
            .iter()
            .filter(|call| self.outputs_open.contains(&self.tool_key(key, &call.id)))
            .filter_map(|call| Some((&call.id, call.output.as_ref()?)))
            .collect();
        let mut signature = DefaultHasher::new();
        for (id, output) in &outputs {
            id.hash(&mut signature);
            output.bytes.hash(&mut signature);
        }
        let version = signature.finish();
        let mut index = self.file_index.borrow_mut();
        if index
            .results
            .get(&key)
            .is_some_and(|known| known.version == version)
        {
            return Vec::new();
        }
        let old = index.results.remove(&key);
        let run = self.run_of(key);
        let old_targets = |id: &str| old.as_ref().and_then(|known| known.targets.get(id));
        let mut names = HashSet::new();
        let mut dirs = Vec::new();
        let mut targets = HashMap::new();
        let mut cost = 1;
        let mut ordered = outputs;
        // Give a newly opened output first access to the resource budget.
        ordered.sort_by_key(|(id, _)| old_targets(id).is_some());
        for (id, output) in ordered {
            let files: Rc<[FileTarget]> = old_targets(id)
                .cloned()
                .unwrap_or_else(|| Rc::from(result_paths(&output.text)));
            let bytes =
                id.len() + 32 + files.iter().map(|file| file.path.len() + 32).sum::<usize>();
            if cost + bytes > RESULT_CACHE_BYTES / 4 {
                break;
            }
            for file in files.iter() {
                if dirs.len() >= NEW_DIRS_PER_GROUP {
                    break;
                }
                let dir = file
                    .path
                    .rsplit_once('/')
                    .map_or("", |(dir, _)| dir)
                    .to_owned();
                if names.insert(dir.clone())
                    && !index.dirs.contains_key(&(run.clone(), dir.clone()))
                {
                    dirs.push(dir);
                }
            }
            cost += bytes;
            targets.insert(id.clone(), files);
        }
        index
            .results
            .insert(key, ResultResources { version, targets }, cost);
        dirs
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
        let mut dirs: Vec<_> = link::paths_in(tree)
            .into_iter()
            .map(|path| path.rsplit_once('/').map_or("", |(dir, _)| dir).to_owned())
            .collect();
        dirs.sort();
        dirs.dedup();
        self.probe_directories(self.run_of(key), dirs, cx);
    }

    pub(super) fn probe_directories(
        &mut self,
        run: RunId,
        dirs: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        for dir in dirs {
            let key = (run.clone(), dir.clone());
            if self.file_index.borrow_mut().dirs.get(&key).is_some() {
                continue;
            }
            let (client, run) = (client.clone(), run.clone());
            let read = Tokio::spawn(cx, async move {
                let mut files = HashSet::new();
                let mut cursor = None;
                for _ in 0..PAGES {
                    let page = client
                        .fs_list(FsListParams {
                            run_id: run.clone(),
                            dir: dir.clone(),
                            cursor,
                            limit: None,
                        })
                        .await?;
                    files.extend(
                        page.entries
                            .into_iter()
                            .filter(|entry| entry.kind == reno_proto::FsEntryKind::File)
                            .map(|entry| entry.path),
                    );
                    cursor = page.next_cursor;
                    if cursor.is_none() {
                        break;
                    }
                }
                Ok::<_, ClientError>(files)
            });
            let task = cx.spawn({
                let key = key.clone();
                async move |this, cx| {
                    let result = read
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .map_or(Dir::Failed, Dir::Files);
                    this.update(cx, |this, cx| {
                        let mut index = this.file_index.borrow_mut();
                        if let Some(Dir::Loading(done)) = index.dirs.remove(&key) {
                            done.detach();
                            index.dirs.insert(key, result, 1);
                            crate::ui::markdown_view::settled(cx);
                            cx.notify();
                        }
                    })
                    .ok();
                }
            });
            self.file_index
                .borrow_mut()
                .dirs
                .insert(key, Dir::Loading(task), 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_candidates_keep_link_lines_and_refuse_outside_paths() {
        let targets = result_paths(
            "[source](src/main.rs:42) and src/lib.rs:7: /etc/hosts ../sibling.txt https://example.net/r",
        );
        assert_eq!(
            targets,
            [
                FileTarget {
                    path: "src/main.rs".into(),
                    line: Some(42)
                },
                FileTarget {
                    path: "src/lib.rs".into(),
                    line: Some(7)
                },
            ]
        );
    }

    #[test]
    fn a_resource_directory_is_scoped_to_its_own_run() {
        let old = crate::tests::run_id("old");
        let new = crate::tests::run_id("new");
        let mut index = FileIndex::default();
        index.dirs.insert(
            (old.clone(), "src".into()),
            Dir::Files(HashSet::from(["src/lib.rs".into()])),
            1,
        );
        index.dirs.insert(
            (new.clone(), "src".into()),
            Dir::Files(HashSet::from(["src/main.rs".into()])),
            1,
        );
        assert!(index.has(&old, "src/lib.rs"));
        assert!(!index.has(&new, "src/lib.rs"));
        assert!(index.has(&new, "src/main.rs"));
    }
}
