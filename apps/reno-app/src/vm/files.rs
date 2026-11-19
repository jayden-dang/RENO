//! The Files tab as a pure model: a lazily loaded tree, one open document, a search, git
//! decorations and what a watch tells it. No window, no clock, no client: each method that needs
//! the daemon returns the [`Request`]s to send, tagged with a ticket, and each answer is applied
//! with the ticket it was asked under, so a late answer to a question that was asked again is
//! dropped. Debounce and subscription are the caller's (the screen's).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use reno_proto::{
    FS_IMAGE_MAX_BYTES, FS_READ_MAX_BYTES, FsChange, FsContentMatch, FsEntry, FsEntryKind,
    FsFileMatch, FsListResult, FsReadResult, GitChange, GitStatusResult, LineEnding,
};
use reno_syntax::HighlightedDocument;

/// Why a request failed, as far as the model cares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Fail {
    /// The run's worktree is gone or the run has no folder (`WorkspaceUnavailable`).
    Unavailable(String),
    /// The path does not exist (any more).
    NotFound(String),
    /// An image over the daemon's limit.
    TooLarge,
    /// Anything else, in the daemon's words.
    Other(String),
}

/// What the app asks the daemon. The screen sends it and answers with the same ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    List {
        dir: String,
        cursor: Option<String>,
        ticket: u64,
    },
    Read {
        path: String,
        ticket: u64,
    },
    Image {
        path: String,
        ticket: u64,
    },
    Status {
        ticket: u64,
    },
    SearchFiles {
        query: String,
        ticket: u64,
    },
    SearchContent {
        query: String,
        ticket: u64,
    },
}

// --- git decorations ----------------------------------------------------------------------------

/// What a decorated path shows, by the stronger of its two sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Deco {
    Added,
    Modified,
    Untracked,
    Renamed,
    Deleted,
    Conflicted,
}

impl Deco {
    /// The letter beside the name.
    pub(crate) const fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Untracked => 'U',
            Self::Renamed => 'R',
            Self::Deleted => 'D',
            Self::Conflicted => '!',
        }
    }

    /// What `git status` says about one path: the index side and the working-tree side.
    pub(crate) fn of(index: GitChange, worktree: GitChange) -> Option<Self> {
        let either = |c: GitChange| index == c || worktree == c;
        if either(GitChange::Conflicted) {
            Some(Self::Conflicted)
        } else if either(GitChange::Untracked) {
            Some(Self::Untracked)
        } else if either(GitChange::Deleted) {
            Some(Self::Deleted)
        } else if either(GitChange::Added) {
            Some(Self::Added)
        } else if either(GitChange::Renamed) || either(GitChange::Copied) {
            Some(Self::Renamed)
        } else if either(GitChange::Modified) || either(GitChange::TypeChanged) {
            Some(Self::Modified)
        } else {
            None
        }
    }
}

/// What `git.status` last said.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Git {
    /// An answer arrived.
    pub(crate) loaded: bool,
    pub(crate) repo: bool,
    pub(crate) branch: Option<String>,
    revision: String,
    decorated: BTreeMap<String, Deco>,
    /// Every directory that holds a changed path, at any depth.
    dirty: BTreeSet<String>,
}

impl Git {
    pub(crate) fn changed(&self) -> usize {
        self.decorated.len()
    }
}

// --- the tree -----------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum DirLoad {
    #[default]
    Unloaded,
    Loading,
    Ready,
    Failed(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Dir {
    pub(crate) load: DirLoad,
    pub(crate) entries: Vec<FsEntry>,
    /// The cursor of the next page; `None` on the last.
    pub(crate) next: Option<String>,
    /// The next page is on its way.
    pub(crate) paging: bool,
    /// Something changed in it since it was listed; it is listed again when it shows.
    stale: bool,
    /// The newest request for this directory.
    ticket: u64,
}

/// One line of the tree as drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    Entry(EntryRow),
    /// Under an open directory: it is loading, empty, failed, or has more pages.
    Note {
        dir: String,
        depth: usize,
        note: Note,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Note {
    Loading,
    Empty,
    Failed(String),
    /// "Show more": the next page. `paging` while it is on its way.
    More {
        paging: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EntryRow {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) depth: usize,
    pub(crate) is_dir: bool,
    pub(crate) expanded: bool,
    /// Git ignores it: drawn dimmed.
    pub(crate) ignored: bool,
    pub(crate) deco: Option<Deco>,
    /// A directory that holds a changed path.
    pub(crate) dirty: bool,
    pub(crate) selected: bool,
}

// --- the open document --------------------------------------------------------------------------

/// How a markdown file shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
    Preview,
    Source,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Highlight {
    /// On a background task.
    Pending,
    Ready(Arc<HighlightedDocument>),
    /// No grammar, or too long: plain text.
    Plain,
}

/// A text file as read.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Text {
    pub(crate) text: Arc<str>,
    /// Byte offset of each line's start.
    starts: Vec<usize>,
    pub(crate) truncated: bool,
    pub(crate) size: u64,
    pub(crate) encoding: String,
    pub(crate) line_ending: LineEnding,
    pub(crate) hash: Option<String>,
    pub(crate) highlight: Highlight,
    /// The index of the longest line (by characters), for the viewer's width.
    pub(crate) longest: usize,
}

impl Text {
    fn new(read: &FsReadResult, text: String) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        // A final newline does not start another line.
        if starts.len() > 1 && starts.last() == Some(&text.len()) {
            starts.pop();
        }
        let mut model = Self {
            text: text.into(),
            starts,
            truncated: read.truncated,
            size: read.size,
            encoding: read.encoding.clone(),
            line_ending: read.line_ending,
            hash: read.content_hash.clone(),
            highlight: Highlight::Pending,
            longest: 0,
        };
        model.longest = (0..model.lines())
            .max_by_key(|i| model.line(*i).chars().count())
            .unwrap_or(0);
        model
    }

    pub(crate) fn lines(&self) -> usize {
        self.starts.len()
    }

    /// Line `ix` (from 0) without its line ending.
    pub(crate) fn line(&self, ix: usize) -> &str {
        let Some(start) = self.starts.get(ix) else {
            return "";
        };
        let end = self.starts.get(ix + 1).copied().unwrap_or(self.text.len());
        let line = &self.text[*start..end];
        let line = line.strip_suffix('\n').unwrap_or(line);
        line.strip_suffix('\r').unwrap_or(line)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DocState {
    Loading,
    Text(Text),
    /// Not text: no preview.
    Binary {
        size: u64,
    },
    /// An image over the daemon's limit.
    TooLarge {
        size: Option<u64>,
        limit: u64,
    },
    Image {
        mime: String,
        bytes: Arc<Vec<u8>>,
    },
    /// Not there: removed, or the worktree is. Not worth trying again.
    Unavailable(String),
    /// Could not be read; trying again may work.
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Doc {
    pub(crate) path: String,
    pub(crate) state: DocState,
    pub(crate) view: View,
    /// A line (from 1) to show and mark, from a search result.
    pub(crate) line: Option<u32>,
    jump: bool,
    ticket: u64,
}

impl Doc {
    pub(crate) fn is_markdown(&self) -> bool {
        is_markdown(&self.path)
    }
}

/// The extensions drawn as markdown.
pub(crate) fn is_markdown(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    name.ends_with(".md") || name.ends_with(".markdown") || name.ends_with(".mdx")
}

/// Whether the daemon serves `path` as an image.
pub(crate) fn is_image(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    ["png", "jpg", "jpeg", "gif", "webp", "svg"]
        .iter()
        .any(|ext| name.rsplit_once('.').is_some_and(|(_, e)| e == *ext))
}

// --- search -------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SearchMode {
    /// Fuzzy on file paths.
    #[default]
    Files,
    /// Lines of text.
    Content,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Results {
    #[default]
    Idle,
    Loading,
    Files {
        files: Vec<FsFileMatch>,
        truncated: bool,
    },
    Content {
        matches: Vec<FsContentMatch>,
        truncated: bool,
    },
    Failed(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Search {
    text: String,
    /// The toggle; a leading `>` in the text means content whatever it says.
    toggle: SearchMode,
    pub(crate) results: Results,
    ticket: u64,
}

impl Search {
    pub(crate) fn mode(&self) -> SearchMode {
        if self.text.trim_start().starts_with('>') {
            SearchMode::Content
        } else {
            self.toggle
        }
    }

    /// What is searched for: the text without a leading `>`, trimmed.
    pub(crate) fn query(&self) -> &str {
        let text = self.text.trim_start();
        text.strip_prefix('>').unwrap_or(text).trim()
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// There is something to search for, so the results replace the tree.
    pub(crate) fn is_active(&self) -> bool {
        !self.query().is_empty()
    }
}

// --- live updates -------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Live {
    /// Not subscribed (the tab is not showing).
    #[default]
    Off,
    On,
    /// The daemon's watcher is down and retrying.
    Degraded,
    /// Could not subscribe (the daemon watches as many trees as it will).
    Unavailable(String),
}

impl Live {
    /// Whether the tab says "live updates unavailable".
    pub(crate) fn is_down(&self) -> bool {
        matches!(self, Self::Degraded | Self::Unavailable(_))
    }
}

// --- the model ----------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub(crate) struct FilesModel {
    /// Settings › General › Show hidden files is off: dotfiles are left out of the tree.
    hide_hidden: bool,
    dirs: BTreeMap<String, Dir>,
    expanded: BTreeSet<String>,
    selected: Option<String>,
    doc: Option<Doc>,
    pub(crate) git: Git,
    pub(crate) search: Search,
    pub(crate) live: Live,
    /// The run works in a folder, not a worktree of its own.
    pub(crate) in_place: bool,
    /// The whole tab is unavailable, and why.
    unavailable: Option<String>,
    /// Narrow panel: the viewer is the page, not the tree.
    pub(crate) show_viewer: bool,
    next_ticket: u64,
    status_ticket: u64,
    /// How many times a directory was listed (a test reads it to see a watch cause a reload).
    #[cfg(test)]
    pub(crate) lists: usize,
}

impl FilesModel {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn ticket(&mut self) -> u64 {
        self.next_ticket += 1;
        self.next_ticket
    }

    /// The first requests: the root, and the git status.
    pub(crate) fn start(&mut self) -> Vec<Request> {
        let mut out = self.list(String::new(), None);
        out.push(self.status());
        out
    }

    /// Everything shown is read again: the tab was hidden, so no watch saw what changed.
    pub(crate) fn rescan(&mut self) -> Vec<Request> {
        self.apply_change(&FsChange {
            overflow: true,
            git: true,
            ..FsChange::default()
        })
    }

    pub(crate) fn unavailable(&self) -> Option<&str> {
        self.unavailable.as_deref()
    }

    /// Takes the tab out of use, with the daemon's reason.
    fn make_unavailable(&mut self, reason: String) {
        self.unavailable = Some(reason);
    }

    #[cfg(test)]
    pub(crate) fn dir(&self, path: &str) -> Option<&Dir> {
        self.dirs.get(path)
    }

    #[cfg(test)]
    pub(crate) fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub(crate) fn doc(&self) -> Option<&Doc> {
        self.doc.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn is_expanded(&self, dir: &str) -> bool {
        self.expanded.contains(dir)
    }

    // --- listing ---

    fn list(&mut self, dir: String, cursor: Option<String>) -> Vec<Request> {
        let ticket = self.ticket();
        #[cfg(test)]
        {
            self.lists += 1;
        }
        let node = self.dirs.entry(dir.clone()).or_default();
        node.ticket = ticket;
        node.stale = false;
        if cursor.is_some() {
            node.paging = true;
        } else if node.load != DirLoad::Ready {
            // A directory that was read keeps showing what it has while it is read again.
            node.load = DirLoad::Loading;
        }
        vec![Request::List {
            dir,
            cursor,
            ticket,
        }]
    }

    /// Opens or closes a directory. Opening one that was never read, or changed since, reads it.
    pub(crate) fn toggle(&mut self, dir: &str) -> Vec<Request> {
        if !self.expanded.remove(dir) {
            return self.expand(dir);
        }
        Vec::new()
    }

    fn expand(&mut self, dir: &str) -> Vec<Request> {
        self.expanded.insert(dir.to_owned());
        let needs = self
            .dirs
            .get(dir)
            .is_none_or(|d| d.load == DirLoad::Unloaded || d.stale);
        if needs {
            self.list(dir.to_owned(), None)
        } else {
            Vec::new()
        }
    }

    /// The next page of an open directory.
    pub(crate) fn load_more(&mut self, dir: &str) -> Vec<Request> {
        let Some(node) = self.dirs.get(dir) else {
            return Vec::new();
        };
        match (&node.next, node.paging) {
            (Some(cursor), false) => self.list(dir.to_owned(), Some(cursor.clone())),
            _ => Vec::new(),
        }
    }

    /// Reads a directory that failed again.
    pub(crate) fn retry(&mut self, dir: &str) -> Vec<Request> {
        if let Some(node) = self.dirs.get_mut(dir) {
            node.load = DirLoad::Unloaded;
        }
        self.list(dir.to_owned(), None)
    }

    pub(crate) fn apply_list(
        &mut self,
        dir: &str,
        ticket: u64,
        cursor: Option<&str>,
        result: Result<FsListResult, Fail>,
    ) {
        match self.dirs.get_mut(dir) {
            Some(node) if node.ticket == ticket => node.paging = false,
            _ => return,
        }
        match result {
            Ok(page) => {
                if page.in_place {
                    self.in_place = true;
                }
                let Some(node) = self.dirs.get_mut(dir) else {
                    return;
                };
                if cursor.is_none() {
                    node.entries.clear();
                }
                let known: BTreeSet<String> = node.entries.iter().map(|e| e.path.clone()).collect();
                node.entries.extend(
                    page.entries
                        .into_iter()
                        .filter(|e| !known.contains(&e.path)),
                );
                // Directories first. Stable, so the daemon's order holds inside each group and
                // across pages.
                node.entries.sort_by_key(|e| e.kind != FsEntryKind::Dir);
                node.next = page.next_cursor;
                node.load = DirLoad::Ready;
            }
            Err(Fail::Unavailable(reason)) => self.make_unavailable(reason),
            Err(fail) => {
                let message = fail.message();
                let Some(node) = self.dirs.get_mut(dir) else {
                    return;
                };
                if cursor.is_none() && node.load == DirLoad::Loading {
                    node.load = DirLoad::Failed(message);
                } else if cursor.is_none() {
                    // It was read before: keep showing that, and look again when it is opened.
                    node.stale = true;
                } else {
                    node.load = DirLoad::Failed(message);
                }
            }
        }
    }

    /// Settings › General › Show hidden files.
    pub(crate) fn set_show_hidden(&mut self, show: bool) {
        self.hide_hidden = !show;
    }

    /// The tree as drawn: the root's entries and what is open under them, in order.
    pub(crate) fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        self.push_rows("", 0, &mut out);
        out
    }

    fn push_rows(&self, dir: &str, depth: usize, out: &mut Vec<Row>) {
        let note = |note| Row::Note {
            dir: dir.to_owned(),
            depth,
            note,
        };
        let Some(node) = self.dirs.get(dir) else {
            out.push(note(Note::Loading));
            return;
        };
        match &node.load {
            DirLoad::Unloaded | DirLoad::Loading if node.entries.is_empty() => {
                out.push(note(Note::Loading));
                return;
            }
            DirLoad::Failed(message) if node.entries.is_empty() => {
                out.push(note(Note::Failed(message.clone())));
                return;
            }
            _ => {}
        }
        if node.entries.is_empty() {
            out.push(note(Note::Empty));
            return;
        }
        for entry in &node.entries {
            if self.hide_hidden && entry.name.starts_with('.') {
                continue;
            }
            let is_dir = entry.kind == FsEntryKind::Dir;
            let expanded = is_dir && self.expanded.contains(&entry.path);
            out.push(Row::Entry(EntryRow {
                path: entry.path.clone(),
                name: entry.name.clone(),
                depth,
                is_dir,
                expanded,
                ignored: entry.ignored,
                deco: (!is_dir)
                    .then(|| self.git.decorated.get(&entry.path).copied())
                    .flatten(),
                dirty: is_dir && self.git.dirty.contains(&entry.path),
                selected: self.selected.as_deref() == Some(entry.path.as_str()),
            }));
            if expanded {
                self.push_rows(&entry.path, depth + 1, out);
            }
        }
        match (&node.next, &node.load) {
            (_, DirLoad::Failed(message)) => out.push(note(Note::Failed(message.clone()))),
            (Some(_), _) => out.push(note(Note::More {
                paging: node.paging,
            })),
            _ => {}
        }
    }

    // --- git ---

    fn status(&mut self) -> Request {
        let ticket = self.ticket();
        self.status_ticket = ticket;
        Request::Status { ticket }
    }

    /// Applies `git.status`. Returns whether anything shown changed (the daemon's `revision` is
    /// the same while nothing did).
    pub(crate) fn apply_status(
        &mut self,
        ticket: u64,
        result: Result<GitStatusResult, Fail>,
    ) -> bool {
        if ticket != self.status_ticket {
            return false;
        }
        let status = match result {
            Ok(status) => status,
            Err(Fail::Unavailable(reason)) => {
                self.make_unavailable(reason);
                return true;
            }
            // Decorations are a convenience: without them the tree is still the tree.
            Err(_) => return false,
        };
        if status.in_place {
            self.in_place = true;
        }
        if self.git.loaded && self.git.revision == status.revision && !status.revision.is_empty() {
            return false;
        }
        let mut decorated = BTreeMap::new();
        let mut dirty = BTreeSet::new();
        for file in &status.files {
            // A deleted path is not in the tree, but its folders changed.
            if let Some(deco) = Deco::of(file.index, file.worktree) {
                decorated.insert(file.path.clone(), deco);
                let mut dir = file.path.as_str();
                while let Some((parent, _)) = dir.rsplit_once('/') {
                    dirty.insert(parent.to_owned());
                    dir = parent;
                }
            }
        }
        self.git = Git {
            loaded: true,
            repo: status.repo,
            branch: status.branch,
            revision: status.revision,
            decorated,
            dirty,
        };
        true
    }

    // --- selecting and opening ---

    /// A click on a row: a directory opens or closes, a file opens in the viewer.
    pub(crate) fn click(&mut self, path: &str, is_dir: bool) -> Vec<Request> {
        if is_dir {
            self.selected = Some(path.to_owned());
            self.toggle(path)
        } else {
            self.open_path(path, None)
        }
    }

    /// Shows `path`: opens every folder above it, selects it and reads it. `line` (from 1) is
    /// scrolled to and marked.
    pub(crate) fn open_path(&mut self, path: &str, line: Option<u32>) -> Vec<Request> {
        let path = path.trim_matches('/').to_owned();
        let mut out = Vec::new();
        let mut prefix = String::new();
        let parts: Vec<&str> = path.split('/').collect();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            out.extend(self.expand_only(&prefix));
        }
        self.selected = Some(path.clone());
        self.show_viewer = true;
        if let Some(doc) = self.doc.as_mut()
            && doc.path == path
            && !matches!(doc.state, DocState::Failed(_) | DocState::Loading)
        {
            doc.line = line;
            doc.jump = line.is_some();
            return out;
        }
        out.extend(self.read(&path, line));
        out
    }

    /// Opens a directory for good (a revealed file's ancestors stay open when toggled twice).
    fn expand_only(&mut self, dir: &str) -> Vec<Request> {
        if self.expanded.contains(dir) {
            return Vec::new();
        }
        self.expand(dir)
    }

    fn read(&mut self, path: &str, line: Option<u32>) -> Option<Request> {
        let ticket = self.ticket();
        let previous_view = self
            .doc
            .as_ref()
            .filter(|d| is_markdown(&d.path))
            .map(|d| d.view);
        self.doc = Some(Doc {
            path: path.to_owned(),
            state: DocState::Loading,
            view: previous_view.unwrap_or(View::Preview),
            line,
            jump: line.is_some(),
            ticket,
        });
        if is_image(path) {
            let size = self.entry_size(path);
            if size.is_some_and(|s| s > FS_IMAGE_MAX_BYTES) {
                // Known to be over the limit: no point asking.
                self.set_state(DocState::TooLarge {
                    size,
                    limit: FS_IMAGE_MAX_BYTES,
                });
                return None;
            }
            Some(Request::Image {
                path: path.to_owned(),
                ticket,
            })
        } else {
            Some(Request::Read {
                path: path.to_owned(),
                ticket,
            })
        }
    }

    fn set_state(&mut self, state: DocState) {
        if let Some(doc) = self.doc.as_mut() {
            doc.state = state;
        }
    }

    fn entry_size(&self, path: &str) -> Option<u64> {
        let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
        self.dirs
            .get(parent)?
            .entries
            .iter()
            .find(|e| e.path == path)?
            .size
    }

    /// Reads the open document again, keeping what shows until the answer (a failed Failed
    /// document, or the retry button).
    pub(crate) fn reload_doc(&mut self) -> Vec<Request> {
        let ticket = self.ticket();
        let Some(doc) = self.doc.as_mut() else {
            return Vec::new();
        };
        let path = doc.path.clone();
        doc.ticket = ticket;
        if matches!(doc.state, DocState::Failed(_)) {
            doc.state = DocState::Loading;
        }
        vec![if is_image(&path) {
            Request::Image { path, ticket }
        } else {
            Request::Read { path, ticket }
        }]
    }

    pub(crate) fn apply_read(&mut self, ticket: u64, result: Result<FsReadResult, Fail>) {
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        if doc.ticket != ticket {
            return;
        }
        match result {
            Ok(read) => {
                if read.in_place {
                    self.in_place = true;
                }
                if read.binary || read.text.is_none() {
                    doc.state = DocState::Binary { size: read.size };
                    return;
                }
                // The same bytes again (a watch re-read): nothing to redraw or highlight.
                if let DocState::Text(old) = &doc.state
                    && old.hash.is_some()
                    && old.hash == read.content_hash
                {
                    return;
                }
                let text = read.text.clone().unwrap_or_default();
                doc.state = DocState::Text(Text::new(&read, text));
            }
            Err(fail) => self.fail_doc(fail),
        }
    }

    pub(crate) fn apply_image(&mut self, ticket: u64, result: Result<(String, Vec<u8>), Fail>) {
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        if doc.ticket != ticket {
            return;
        }
        match result {
            Ok((mime, bytes)) => {
                if let DocState::Image { bytes: old, .. } = &doc.state
                    && **old == bytes
                {
                    return;
                }
                doc.state = DocState::Image {
                    mime,
                    bytes: Arc::new(bytes),
                };
            }
            Err(fail) => self.fail_doc(fail),
        }
    }

    fn fail_doc(&mut self, fail: Fail) {
        let state = match &fail {
            Fail::Unavailable(reason) => {
                let reason = reason.clone();
                self.make_unavailable(reason);
                return;
            }
            Fail::NotFound(_) => DocState::Unavailable("This file is no longer there.".to_owned()),
            Fail::TooLarge => {
                let size = self.doc.as_ref().and_then(|d| self.entry_size(&d.path));
                DocState::TooLarge {
                    size,
                    limit: FS_IMAGE_MAX_BYTES,
                }
            }
            Fail::Other(message) => DocState::Failed(message.clone()),
        };
        self.set_state(state);
    }

    /// The highlight of the open text, from the background task. Dropped when the document
    /// moved on.
    pub(crate) fn apply_highlight(
        &mut self,
        path: &str,
        hash: Option<&str>,
        doc: Option<Arc<HighlightedDocument>>,
    ) {
        let Some(open) = self.doc.as_mut() else {
            return;
        };
        let DocState::Text(text) = &mut open.state else {
            return;
        };
        if open.path != path || text.hash.as_deref() != hash {
            return;
        }
        text.highlight = doc.map_or(Highlight::Plain, Highlight::Ready);
    }

    /// The line to scroll to, once: the viewer calls it when it has the text.
    pub(crate) fn take_jump(&mut self) -> Option<u32> {
        let doc = self.doc.as_mut()?;
        if !matches!(doc.state, DocState::Text(_)) || !std::mem::take(&mut doc.jump) {
            return None;
        }
        doc.line
    }

    pub(crate) fn set_view(&mut self, view: View) {
        if let Some(doc) = self.doc.as_mut() {
            doc.view = view;
        }
    }

    /// Narrow panel: back from the viewer to the tree.
    pub(crate) fn back(&mut self) {
        self.show_viewer = false;
    }

    // --- search ---

    /// The text of the search field changed. Returns whether it did; the caller then waits out
    /// its debounce and calls [`Self::commit_search`].
    pub(crate) fn set_search_text(&mut self, text: &str) -> bool {
        if self.search.text == text {
            return false;
        }
        self.search.text = text.to_owned();
        if !self.search.is_active() {
            // Nothing to search for: the tree comes back, and a late answer is dropped.
            self.search.results = Results::Idle;
            self.search.ticket = self.ticket();
        }
        true
    }

    /// Flips between searching names and searching text.
    pub(crate) fn toggle_search_mode(&mut self) {
        self.search.toggle = match self.search.toggle {
            SearchMode::Files => SearchMode::Content,
            SearchMode::Content => SearchMode::Files,
        };
        // The old results answered another question.
        if self.search.is_active() {
            self.search.results = Results::Loading;
        }
    }

    /// The debounce ended: ask. Nothing for an empty query.
    pub(crate) fn commit_search(&mut self) -> Vec<Request> {
        if !self.search.is_active() {
            return Vec::new();
        }
        let ticket = self.ticket();
        self.search.ticket = ticket;
        self.search.results = Results::Loading;
        let query = self.search.query().to_owned();
        vec![match self.search.mode() {
            SearchMode::Files => Request::SearchFiles { query, ticket },
            SearchMode::Content => Request::SearchContent { query, ticket },
        }]
    }

    pub(crate) fn apply_search_files(
        &mut self,
        ticket: u64,
        result: Result<reno_proto::FsSearchFilesResult, Fail>,
    ) {
        if ticket != self.search.ticket {
            return;
        }
        self.search.results = match result {
            Ok(found) => {
                if found.in_place {
                    self.in_place = true;
                }
                Results::Files {
                    files: found.files,
                    truncated: found.truncated,
                }
            }
            Err(Fail::Unavailable(reason)) => {
                self.make_unavailable(reason);
                return;
            }
            Err(fail) => Results::Failed(fail.message()),
        };
    }

    pub(crate) fn apply_search_content(
        &mut self,
        ticket: u64,
        result: Result<reno_proto::FsSearchContentResult, Fail>,
    ) {
        if ticket != self.search.ticket {
            return;
        }
        self.search.results = match result {
            Ok(found) => {
                if found.in_place {
                    self.in_place = true;
                }
                Results::Content {
                    matches: found.matches,
                    truncated: found.truncated,
                }
            }
            Err(Fail::Unavailable(reason)) => {
                self.make_unavailable(reason);
                return;
            }
            Err(fail) => Results::Failed(fail.message()),
        };
    }

    // --- watching ---

    pub(crate) fn set_live(&mut self, live: Live) {
        self.live = live;
    }

    /// What an `fs.watch` item asks for. `dirs` read again the directories that are open (the
    /// others are marked, and read when opened); `git` reads the status; `overflow` reads
    /// everything shown; `degraded` only says live updates are down.
    pub(crate) fn apply_change(&mut self, change: &FsChange) -> Vec<Request> {
        if change.degraded {
            self.live = Live::Degraded;
            return Vec::new();
        }
        if self.live == Live::Degraded {
            // The item after a degraded spell carries `overflow`: the watch is back.
            self.live = Live::On;
        }
        let mut out = Vec::new();
        let mut reread_doc = false;
        let doc_dir = self
            .doc
            .as_ref()
            .map(|d| d.path.rsplit_once('/').map_or("", |(p, _)| p).to_owned());
        let touched: Vec<String> = if change.overflow {
            self.dirs.keys().cloned().collect()
        } else {
            change
                .dirs
                .iter()
                .filter(|d| self.dirs.contains_key(*d))
                .cloned()
                .collect()
        };
        for dir in touched {
            if doc_dir.as_deref() == Some(dir.as_str()) {
                reread_doc = true;
            }
            let open = dir.is_empty() || self.expanded.contains(&dir);
            let loaded = self
                .dirs
                .get(&dir)
                .is_some_and(|d| d.load != DirLoad::Unloaded);
            if open && loaded {
                out.extend(self.list(dir, None));
            } else if let Some(node) = self.dirs.get_mut(&dir) {
                node.stale = true;
            }
        }
        // A file written in a folder that is not in the tree still shows in the viewer.
        if doc_dir.as_ref().is_some_and(|d| change.dirs.contains(d)) || change.overflow {
            reread_doc = true;
        }
        if reread_doc && self.doc.is_some() {
            out.extend(self.reload_doc());
        }
        if change.git || change.overflow {
            out.push(self.status());
        }
        out
    }

    /// The rows of the results as `(path, line, text)`; for content, one per match.
    #[cfg(test)]
    pub(crate) fn result_paths(&self) -> Vec<String> {
        match &self.search.results {
            Results::Files { files, .. } => files.iter().map(|f| f.path.clone()).collect(),
            Results::Content { matches, .. } => matches.iter().map(|m| m.path.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// The text cut length the daemon stops at (shown in "cut" notes).
    pub(crate) const READ_LIMIT: usize = FS_READ_MAX_BYTES;
}

impl Fail {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Unavailable(m) | Self::NotFound(m) | Self::Other(m) => m.clone(),
            Self::TooLarge => "too large to show".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use reno_proto::{FsSearchContentResult, FsSearchFilesResult, GitFileStatus};

    use super::*;

    fn entry(path: &str, kind: FsEntryKind) -> FsEntry {
        FsEntry {
            name: path.rsplit('/').next().unwrap().to_owned(),
            path: path.to_owned(),
            kind,
            size: (kind == FsEntryKind::File).then_some(10),
            mtime_ms: None,
            ignored: false,
        }
    }

    fn page(entries: Vec<FsEntry>, next: Option<&str>) -> FsListResult {
        FsListResult {
            entries,
            next_cursor: next.map(str::to_owned),
            in_place: false,
        }
    }

    fn list_of(requests: &[Request]) -> Vec<(String, Option<String>, u64)> {
        requests
            .iter()
            .filter_map(|r| match r {
                Request::List {
                    dir,
                    cursor,
                    ticket,
                } => Some((dir.clone(), cursor.clone(), *ticket)),
                _ => None,
            })
            .collect()
    }

    /// A model whose root holds `docs/` and `a.rs`.
    fn with_root() -> FilesModel {
        let mut m = FilesModel::new();
        let reqs = m.start();
        let (_, _, ticket) = list_of(&reqs).remove(0);
        m.apply_list(
            "",
            ticket,
            None,
            Ok(page(
                vec![
                    entry("a.rs", FsEntryKind::File),
                    entry("docs", FsEntryKind::Dir),
                ],
                None,
            )),
        );
        m
    }

    fn names(m: &FilesModel) -> Vec<String> {
        m.rows()
            .into_iter()
            .map(|r| match r {
                Row::Entry(e) => format!("{}{}", "  ".repeat(e.depth), e.name),
                Row::Note { note, depth, .. } => format!("{}<{note:?}>", "  ".repeat(depth)),
            })
            .collect()
    }

    #[test]
    fn hidden_files_are_listed_unless_settings_hide_them() {
        let mut m = FilesModel::new();
        let reqs = m.start();
        let (_, _, ticket) = list_of(&reqs).remove(0);
        m.apply_list(
            "",
            ticket,
            None,
            Ok(page(
                vec![
                    entry(".env.example", FsEntryKind::File),
                    entry(".github", FsEntryKind::Dir),
                    entry("a.rs", FsEntryKind::File),
                ],
                None,
            )),
        );
        assert_eq!(names(&m), [".github", ".env.example", "a.rs"], "listed by default");
        m.set_show_hidden(false);
        assert_eq!(names(&m), ["a.rs"]);
        m.set_show_hidden(true);
        assert_eq!(names(&m).len(), 3);
    }

    #[test]
    fn start_reads_the_root_and_the_status_and_shows_loading_meanwhile() {
        let mut m = FilesModel::new();
        let reqs = m.start();
        assert_eq!(list_of(&reqs).len(), 1);
        assert!(matches!(reqs[1], Request::Status { .. }));
        assert_eq!(names(&m), ["<Loading>"]);
    }

    #[test]
    fn directories_sort_first_whatever_the_order_they_came_in() {
        let m = with_root();
        assert_eq!(names(&m), ["docs", "a.rs"]);
    }

    #[test]
    fn a_directory_loads_when_first_opened_and_not_again_when_reopened() {
        let mut m = with_root();
        let reqs = m.toggle("docs");
        let (dir, cursor, ticket) = list_of(&reqs).remove(0);
        assert_eq!((dir.as_str(), cursor), ("docs", None));
        assert_eq!(names(&m), ["docs", "  <Loading>", "a.rs"]);
        m.apply_list(
            "docs",
            ticket,
            None,
            Ok(page(vec![entry("docs/g.md", FsEntryKind::File)], None)),
        );
        assert_eq!(names(&m), ["docs", "  g.md", "a.rs"]);
        // Closing hides it; opening again does not read it again.
        assert!(m.toggle("docs").is_empty());
        assert_eq!(names(&m), ["docs", "a.rs"]);
        assert!(m.toggle("docs").is_empty());
        assert_eq!(names(&m), ["docs", "  g.md", "a.rs"]);
    }

    #[test]
    fn an_empty_and_a_failed_directory_say_so_and_retry_reads_it_again() {
        let mut m = with_root();
        let (_, _, t) = list_of(&m.toggle("docs")).remove(0);
        m.apply_list("docs", t, None, Ok(page(vec![], None)));
        assert_eq!(names(&m), ["docs", "  <Empty>", "a.rs"]);
        let mut m = with_root();
        let (_, _, t) = list_of(&m.toggle("docs")).remove(0);
        m.apply_list("docs", t, None, Err(Fail::Other("boom".into())));
        assert_eq!(names(&m)[1], "  <Failed(\"boom\")>");
        let again = m.retry("docs");
        assert_eq!(list_of(&again).len(), 1);
        assert_eq!(names(&m)[1], "  <Loading>");
    }

    #[test]
    fn pages_append_through_the_cursor_without_duplicates() {
        let mut m = FilesModel::new();
        let (_, _, t) = list_of(&m.start()).remove(0);
        let first: Vec<FsEntry> = (0..3)
            .map(|n| entry(&format!("f{n}"), FsEntryKind::File))
            .collect();
        m.apply_list("", t, None, Ok(page(first, Some("c1"))));
        assert_eq!(names(&m), ["f0", "f1", "f2", "<More { paging: false }>"]);
        let (_, cursor, t2) = list_of(&m.load_more("")).remove(0);
        assert_eq!(cursor.as_deref(), Some("c1"));
        assert_eq!(names(&m).last().unwrap(), "<More { paging: true }>");
        // Asked while one is on its way: nothing.
        assert!(m.load_more("").is_empty());
        let second: Vec<FsEntry> = (2..5)
            .map(|n| entry(&format!("f{n}"), FsEntryKind::File))
            .collect();
        m.apply_list("", t2, Some("c1"), Ok(page(second, None)));
        assert_eq!(names(&m), ["f0", "f1", "f2", "f3", "f4"]);
    }

    #[test]
    fn a_late_answer_for_a_list_asked_again_is_dropped() {
        let mut m = with_root();
        let (_, _, old) = list_of(&m.toggle("docs")).remove(0);
        let (_, _, new) = list_of(&m.retry("docs")).remove(0);
        m.apply_list(
            "docs",
            old,
            None,
            Ok(page(vec![entry("docs/old", FsEntryKind::File)], None)),
        );
        assert_eq!(names(&m)[1], "  <Loading>");
        m.apply_list(
            "docs",
            new,
            None,
            Ok(page(vec![entry("docs/new", FsEntryKind::File)], None)),
        );
        assert_eq!(names(&m)[1], "  new");
    }

    #[test]
    fn ignored_entries_are_marked_for_dimming() {
        let mut m = FilesModel::new();
        let (_, _, t) = list_of(&m.start()).remove(0);
        let mut target = entry("target", FsEntryKind::Dir);
        target.ignored = true;
        m.apply_list("", t, None, Ok(page(vec![target], None)));
        let Row::Entry(e) = &m.rows()[0] else {
            panic!()
        };
        assert!(e.ignored);
    }

    fn status(files: Vec<(&str, GitChange, GitChange)>, revision: &str) -> GitStatusResult {
        GitStatusResult {
            branch: Some("main".into()),
            repo: true,
            files: files
                .into_iter()
                .map(|(path, index, worktree)| GitFileStatus {
                    path: path.into(),
                    old_path: None,
                    index,
                    worktree,
                })
                .collect(),
            revision: revision.into(),
            in_place: false,
        }
    }

    #[test]
    fn git_states_map_to_decorations_and_mark_every_ancestor() {
        use GitChange::*;
        assert_eq!(Deco::of(Unmodified, Modified), Some(Deco::Modified));
        assert_eq!(Deco::of(Modified, Unmodified), Some(Deco::Modified));
        assert_eq!(Deco::of(Added, Unmodified), Some(Deco::Added));
        assert_eq!(Deco::of(Added, Modified), Some(Deco::Added));
        assert_eq!(Deco::of(Untracked, Untracked), Some(Deco::Untracked));
        assert_eq!(Deco::of(Renamed, Unmodified), Some(Deco::Renamed));
        assert_eq!(Deco::of(Copied, Unmodified), Some(Deco::Renamed));
        assert_eq!(Deco::of(Unmodified, Deleted), Some(Deco::Deleted));
        assert_eq!(Deco::of(Conflicted, Conflicted), Some(Deco::Conflicted));
        assert_eq!(Deco::of(Unmodified, Unmodified), None);

        let mut m = with_root();
        let (_, _, t) = list_of(&m.toggle("docs")).remove(0);
        m.apply_list(
            "docs",
            t,
            None,
            Ok(page(vec![entry("docs/g.md", FsEntryKind::File)], None)),
        );
        let Request::Status { ticket } = m.status() else {
            panic!()
        };
        assert!(m.apply_status(
            ticket,
            Ok(status(
                vec![
                    ("a.rs", Unmodified, Modified),
                    ("docs/g.md", Untracked, Untracked),
                    ("docs/deep/x.rs", Added, Unmodified),
                ],
                "r1"
            ))
        ));
        let rows = m.rows();
        let Row::Entry(docs) = &rows[0] else { panic!() };
        assert!(docs.dirty && docs.deco.is_none(), "{docs:?}");
        let Row::Entry(g) = &rows[1] else { panic!() };
        assert_eq!(g.deco, Some(Deco::Untracked));
        let Row::Entry(a) = &rows[2] else { panic!() };
        assert_eq!(a.deco, Some(Deco::Modified));
        assert!(m.git.dirty.contains("docs/deep"));
        assert_eq!(m.git.changed(), 3);
        assert_eq!(m.git.branch.as_deref(), Some("main"));
    }

    #[test]
    fn a_status_with_the_same_revision_changes_nothing_and_a_stale_ticket_is_dropped() {
        let mut m = with_root();
        let Request::Status { ticket } = m.status() else {
            panic!()
        };
        assert!(m.apply_status(ticket, Ok(status(vec![], "r1"))));
        let Request::Status { ticket: t2 } = m.status() else {
            panic!()
        };
        assert!(!m.apply_status(
            ticket,
            Ok(status(
                vec![("a.rs", GitChange::Added, GitChange::Unmodified)],
                "r2"
            ))
        ));
        assert!(!m.apply_status(t2, Ok(status(vec![], "r1"))));
        assert!(m.apply_status(t2, Ok(status(vec![], "r2"))));
    }

    fn read(text: &str) -> FsReadResult {
        FsReadResult {
            path: "x".into(),
            text: Some(text.to_owned()),
            truncated: false,
            encoding: "utf-8".into(),
            line_ending: LineEnding::Lf,
            size: text.len() as u64,
            binary: false,
            content_hash: Some(format!("h{}", text.len())),
            mtime_ms: None,
            in_place: false,
        }
    }

    fn read_ticket(reqs: &[Request]) -> u64 {
        reqs.iter()
            .find_map(|r| match r {
                Request::Read { ticket, .. } | Request::Image { ticket, .. } => Some(*ticket),
                _ => None,
            })
            .expect("a read")
    }

    #[test]
    fn opening_a_file_reads_it_and_a_text_answer_has_lines_without_endings() {
        let mut m = with_root();
        let reqs = m.click("a.rs", false);
        assert!(matches!(&reqs[0], Request::Read { path, .. } if path == "a.rs"));
        assert!(m.show_viewer);
        assert_eq!(m.doc().unwrap().state, DocState::Loading);
        m.apply_read(read_ticket(&reqs), Ok(read("one\r\ntwo\n\nlast")));
        let DocState::Text(t) = &m.doc().unwrap().state else {
            panic!()
        };
        assert_eq!(t.lines(), 4);
        assert_eq!(
            (0..4).map(|i| t.line(i)).collect::<Vec<_>>(),
            ["one", "two", "", "last"]
        );
        assert_eq!(t.highlight, Highlight::Pending);
        assert_eq!(t.longest, 3);
        // A trailing newline does not make another line.
        let reqs = m.click("a.rs", false);
        assert!(reqs.is_empty() || matches!(reqs[0], Request::Read { .. }));
    }

    #[test]
    fn binary_and_unreadable_files_get_their_own_states() {
        let mut m = with_root();
        let t = read_ticket(&m.click("a.rs", false));
        let mut bin = read("");
        bin.text = None;
        bin.binary = true;
        bin.size = 4;
        m.apply_read(t, Ok(bin));
        assert_eq!(m.doc().unwrap().state, DocState::Binary { size: 4 });
        let t = read_ticket(&m.click("docs/other.rs", false));
        m.apply_read(t, Err(Fail::NotFound("gone".into())));
        assert!(matches!(m.doc().unwrap().state, DocState::Unavailable(_)));
        let t = read_ticket(&m.click("docs/other2.rs", false));
        m.apply_read(t, Err(Fail::Other("io".into())));
        assert_eq!(m.doc().unwrap().state, DocState::Failed("io".into()));
        // Failed documents can be read again.
        assert!(matches!(m.reload_doc()[0], Request::Read { .. }));
        assert_eq!(m.doc().unwrap().state, DocState::Loading);
    }

    #[test]
    fn a_removed_workspace_makes_the_whole_tab_unavailable_with_the_reason() {
        let mut m = FilesModel::new();
        let (_, _, t) = list_of(&m.start()).remove(0);
        assert!(m.unavailable().is_none());
        m.apply_list(
            "",
            t,
            None,
            Err(Fail::Unavailable("its worktree was removed".into())),
        );
        assert_eq!(m.unavailable(), Some("its worktree was removed"));
    }

    #[test]
    fn images_are_read_as_images_and_over_the_limit_say_so_without_a_request_result() {
        let mut m = FilesModel::new();
        let (_, _, t) = list_of(&m.start()).remove(0);
        let mut big = entry("big.png", FsEntryKind::File);
        big.size = Some(FS_IMAGE_MAX_BYTES + 1);
        m.apply_list(
            "",
            t,
            None,
            Ok(page(vec![big, entry("a.png", FsEntryKind::File)], None)),
        );
        let reqs = m.click("a.png", false);
        assert!(matches!(&reqs[0], Request::Image { path, .. } if path == "a.png"));
        m.apply_image(read_ticket(&reqs), Ok(("image/png".into(), vec![1, 2])));
        assert!(matches!(m.doc().unwrap().state, DocState::Image { .. }));
        // Known to be over the limit from the listing: no request.
        assert!(m.click("big.png", false).is_empty());
        assert!(matches!(
            m.doc().unwrap().state,
            DocState::TooLarge { size: Some(_), .. }
        ));
        // Opened from a search, with no size known: the daemon's refusal says the same.
        let reqs = m.click("elsewhere/huge.png", false);
        m.apply_image(read_ticket(&reqs), Err(Fail::TooLarge));
        assert!(matches!(
            m.doc().unwrap().state,
            DocState::TooLarge { size: None, .. }
        ));
    }

    #[test]
    fn opening_a_path_reveals_its_folders_and_selects_it() {
        let mut m = with_root();
        let reqs = m.open_path("docs/deep/x.rs", Some(3));
        let lists = list_of(&reqs);
        assert_eq!(
            lists.iter().map(|l| l.0.as_str()).collect::<Vec<_>>(),
            ["docs", "docs/deep"]
        );
        assert_eq!(m.selected(), Some("docs/deep/x.rs"));
        assert!(m.is_expanded("docs") && m.is_expanded("docs/deep"));
        assert_eq!(m.doc().unwrap().line, Some(3));
        // The jump is taken once, and only when the text is there.
        assert_eq!(m.take_jump(), None);
        m.apply_read(read_ticket(&reqs), Ok(read("a\nb\nc\nd")));
        assert_eq!(m.take_jump(), Some(3));
        assert_eq!(m.take_jump(), None);
        // Opening the same file at another line moves the mark without reading it again.
        assert!(m.open_path("docs/deep/x.rs", Some(1)).is_empty());
        assert_eq!(m.take_jump(), Some(1));
    }

    #[test]
    fn markdown_opens_as_preview_and_keeps_the_choice_between_markdown_files() {
        let mut m = with_root();
        let t = read_ticket(&m.click("a.md", false));
        m.apply_read(t, Ok(read("# hi")));
        assert_eq!(m.doc().unwrap().view, View::Preview);
        m.set_view(View::Source);
        m.click("b.md", false);
        assert_eq!(m.doc().unwrap().view, View::Source);
        m.click("c.rs", false);
        m.click("d.md", false);
        assert_eq!(m.doc().unwrap().view, View::Preview);
        assert!(is_markdown("README.MD") && is_markdown("a/b.markdown") && !is_markdown("a.rs"));
        assert!(is_image("x/y.PNG") && is_image("a.svg") && !is_image("png"));
    }

    #[test]
    fn a_highlight_lands_only_on_the_document_it_was_made_for() {
        let mut m = with_root();
        let t = read_ticket(&m.click("a.rs", false));
        m.apply_read(t, Ok(read("fn main() {}")));
        let hash = Some(m.doc().unwrap().clone()).and_then(|d| match d.state {
            DocState::Text(t) => t.hash,
            _ => None,
        });
        m.apply_highlight("other.rs", hash.as_deref(), None);
        assert!(matches!(
            &m.doc().unwrap().state,
            DocState::Text(t) if t.highlight == Highlight::Pending
        ));
        m.apply_highlight("a.rs", Some("stale"), None);
        assert!(matches!(
            &m.doc().unwrap().state,
            DocState::Text(t) if t.highlight == Highlight::Pending
        ));
        m.apply_highlight("a.rs", hash.as_deref(), None);
        assert!(matches!(
            &m.doc().unwrap().state,
            DocState::Text(t) if t.highlight == Highlight::Plain
        ));
    }

    #[test]
    fn search_prefix_and_toggle_choose_names_or_content_and_empty_asks_nothing() {
        let mut m = with_root();
        assert!(m.set_search_text("lib"));
        assert!(!m.set_search_text("lib"));
        assert_eq!(m.search.mode(), SearchMode::Files);
        assert_eq!(m.search.query(), "lib");
        let reqs = m.commit_search();
        assert!(matches!(&reqs[0], Request::SearchFiles { query, .. } if query == "lib"));
        m.set_search_text("> fn main ");
        assert_eq!(m.search.mode(), SearchMode::Content);
        assert_eq!(m.search.query(), "fn main");
        assert!(matches!(
            &m.commit_search()[0],
            Request::SearchContent { query, .. } if query == "fn main"
        ));
        // The toggle does the same without the prefix.
        m.set_search_text("alpha");
        m.toggle_search_mode();
        assert_eq!(m.search.mode(), SearchMode::Content);
        assert_eq!(m.search.results, Results::Loading);
        // Nothing typed: no request, and the tree is back.
        m.set_search_text("  ");
        assert!(m.commit_search().is_empty());
        assert!(!m.search.is_active());
        m.set_search_text(">");
        assert!(m.commit_search().is_empty());
    }

    #[test]
    fn search_answers_apply_by_ticket_and_clearing_drops_a_late_one() {
        let mut m = with_root();
        m.set_search_text("a");
        let Request::SearchFiles { ticket: first, .. } = m.commit_search().remove(0) else {
            panic!()
        };
        m.set_search_text("ab");
        let Request::SearchFiles { ticket: second, .. } = m.commit_search().remove(0) else {
            panic!()
        };
        let found = |path: &str| FsSearchFilesResult {
            files: vec![FsFileMatch {
                path: path.into(),
                score: 1,
            }],
            truncated: false,
            in_place: false,
        };
        m.apply_search_files(first, Ok(found("old")));
        assert_eq!(m.search.results, Results::Loading);
        m.apply_search_files(second, Ok(found("new")));
        assert_eq!(m.result_paths(), ["new"]);
        // Cleared: a late answer does not bring the results back.
        m.set_search_text("");
        assert_eq!(m.search.results, Results::Idle);
        m.apply_search_files(second, Ok(found("late")));
        assert_eq!(m.search.results, Results::Idle);

        m.set_search_text("> x");
        let Request::SearchContent { ticket, .. } = m.commit_search().remove(0) else {
            panic!()
        };
        m.apply_search_content(
            ticket,
            Ok(FsSearchContentResult {
                matches: vec![FsContentMatch {
                    path: "a.rs".into(),
                    line: 4,
                    column: 0,
                    text: "x".into(),
                }],
                truncated: true,
                in_place: true,
            }),
        );
        assert!(m.in_place);
        assert_eq!(m.result_paths(), ["a.rs"]);
        m.set_search_text("> y");
        let Request::SearchContent { ticket, .. } = m.commit_search().remove(0) else {
            panic!()
        };
        m.apply_search_content(ticket, Err(Fail::Other("bad pattern".into())));
        assert_eq!(m.search.results, Results::Failed("bad pattern".into()));
    }

    #[test]
    fn a_watch_item_reads_open_directories_marks_closed_ones_and_asks_for_git() {
        let mut m = with_root();
        let (_, _, t) = list_of(&m.toggle("docs")).remove(0);
        m.apply_list(
            "docs",
            t,
            None,
            Ok(page(vec![entry("docs/g.md", FsEntryKind::File)], None)),
        );
        m.toggle("docs");
        let (_, _, t2) = list_of(&m.toggle("docs/deep")).remove(0);
        m.apply_list("docs/deep", t2, None, Ok(page(vec![], None)));
        m.toggle("docs/deep");
        // `docs` is open, `docs/deep` was read and closed.
        let reqs = m.apply_change(&FsChange {
            dirs: vec!["".into(), "docs/deep".into(), "never/read".into()],
            git: true,
            ..FsChange::default()
        });
        let listed: Vec<String> = list_of(&reqs).into_iter().map(|l| l.0).collect();
        assert_eq!(listed, [""], "only the open directory is read");
        assert!(matches!(reqs.last(), Some(Request::Status { .. })));
        // The closed one is read when it opens.
        assert_eq!(list_of(&m.toggle("docs/deep")).len(), 1);
        // A change that names no git and no known directory asks for nothing.
        assert!(
            m.apply_change(&FsChange {
                dirs: vec!["elsewhere".into()],
                ..FsChange::default()
            })
            .is_empty()
        );
    }

    #[test]
    fn a_directory_read_again_keeps_its_rows_until_the_answer() {
        let mut m = with_root();
        let reqs = m.apply_change(&FsChange {
            dirs: vec!["".into()],
            ..FsChange::default()
        });
        assert_eq!(names(&m), ["docs", "a.rs"]);
        let (_, _, t) = list_of(&reqs).remove(0);
        m.apply_list(
            "",
            t,
            None,
            Ok(page(vec![entry("new.rs", FsEntryKind::File)], None)),
        );
        assert_eq!(names(&m), ["new.rs"]);
        // A failed re-read keeps showing what it has.
        let reqs = m.apply_change(&FsChange {
            dirs: vec!["".into()],
            ..FsChange::default()
        });
        let (_, _, t) = list_of(&reqs).remove(0);
        m.apply_list("", t, None, Err(Fail::Other("busy".into())));
        assert_eq!(names(&m), ["new.rs"]);
    }

    #[test]
    fn a_watch_item_for_the_open_files_folder_reads_the_file_again_and_same_bytes_change_nothing() {
        let mut m = with_root();
        let t = read_ticket(&m.click("docs/g.md", false));
        m.apply_read(t, Ok(read("# one")));
        let before = m.doc().unwrap().clone();
        let reqs = m.apply_change(&FsChange {
            dirs: vec!["docs".into()],
            ..FsChange::default()
        });
        let again = read_ticket(&reqs);
        assert!(
            reqs.iter()
                .any(|r| matches!(r, Request::Read { path, .. } if path == "docs/g.md"))
        );
        // Still showing the old text while it is read.
        assert!(matches!(m.doc().unwrap().state, DocState::Text(_)));
        m.apply_read(again, Ok(read("# one")));
        assert_eq!(m.doc().unwrap().state, before.state);
        let reqs = m.apply_change(&FsChange {
            dirs: vec!["docs".into()],
            ..FsChange::default()
        });
        m.apply_read(read_ticket(&reqs), Ok(read("# two!")));
        assert!(matches!(&m.doc().unwrap().state, DocState::Text(t) if &*t.text == "# two!"));
        // The file is gone.
        let reqs = m.apply_change(&FsChange {
            dirs: vec!["docs".into()],
            ..FsChange::default()
        });
        m.apply_read(read_ticket(&reqs), Err(Fail::NotFound("x".into())));
        assert!(matches!(m.doc().unwrap().state, DocState::Unavailable(_)));
    }

    #[test]
    fn overflow_reads_everything_shown_and_degraded_only_says_updates_are_down() {
        let mut m = with_root();
        let (_, _, t) = list_of(&m.toggle("docs")).remove(0);
        m.apply_list("docs", t, None, Ok(page(vec![], None)));
        m.set_live(Live::On);
        let reqs = m.apply_change(&FsChange {
            overflow: true,
            ..FsChange::default()
        });
        let mut listed: Vec<String> = list_of(&reqs).into_iter().map(|l| l.0).collect();
        listed.sort();
        assert_eq!(listed, ["", "docs"]);
        assert!(matches!(reqs.last(), Some(Request::Status { .. })));

        let reqs = m.apply_change(&FsChange {
            degraded: true,
            ..FsChange::default()
        });
        assert!(reqs.is_empty());
        assert_eq!(m.live, Live::Degraded);
        assert!(m.live.is_down());
        // The item that ends a degraded spell has `overflow`: the watch is live again.
        m.apply_change(&FsChange {
            overflow: true,
            ..FsChange::default()
        });
        assert_eq!(m.live, Live::On);
        m.set_live(Live::Unavailable("too many".into()));
        assert!(m.live.is_down());
    }

    #[test]
    fn narrow_panel_state_follows_open_and_back() {
        let mut m = with_root();
        assert!(!m.show_viewer);
        m.click("a.rs", false);
        assert!(m.show_viewer);
        m.back();
        assert!(!m.show_viewer);
    }

    #[test]
    fn in_place_is_learnt_from_any_answer() {
        let mut m = FilesModel::new();
        let (_, _, t) = list_of(&m.start()).remove(0);
        assert!(!m.in_place);
        m.apply_list(
            "",
            t,
            None,
            Ok(FsListResult {
                entries: vec![],
                next_cursor: None,
                in_place: true,
            }),
        );
        assert!(m.in_place);
    }
}
