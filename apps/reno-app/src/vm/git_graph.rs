//! The History tab's facts: the lane graph of a commit log, the ref badges, paging, search,
//! selection and which commits can open a diff. Pure: the screen sends the [`Request`]s the model
//! returns and applies each answer with the ticket it was asked under, so a late answer to a
//! question that was asked again is dropped (the same shape as `vm::files`).
//!
//! The lane layout is zeron's `layout_graph` and `collapse_branch_runs` (MIT), adapted to the
//! wire type [`GitCommit`]. `git.log` answers newest first in topological order (a commit before
//! all its parents), so the lanes are laid out from `parents` alone.
//!
//! (`vm::history` is the approvals history of the Project page; this is the commit log.)

use std::collections::{HashMap, HashSet};

use reno_proto::{GitCommit, GitLogResult, GitLogSearchResult, GitRefKind};

/// Commits per `git.log` page.
pub(crate) const PAGE: u32 = 100;
/// The most commits a search shows.
pub(crate) const SEARCH_LIMIT: u32 = 100;

// --- the lane graph -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SegmentShape {
    /// A lane that passes the row.
    Through,
    /// A lane that ends at this row's node (a child above it).
    Incoming,
    /// A lane that starts at this row's node (a parent below it).
    Outgoing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphSegment {
    pub(crate) from_lane: usize,
    pub(crate) to_lane: usize,
    pub(crate) color_id: usize,
    pub(crate) shape: SegmentShape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphRow {
    pub(crate) sha: String,
    pub(crate) node_lane: usize,
    pub(crate) node_color_id: usize,
    pub(crate) segments: Vec<GraphSegment>,
    pub(crate) is_head: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GraphLayout {
    pub(crate) rows: Vec<GraphRow>,
    pub(crate) max_lane_count: usize,
}

#[derive(Debug, Clone)]
struct ActiveLane {
    id: usize,
    color_id: usize,
    target_sha: String,
}

fn lane_index(lanes: &[ActiveLane], id: usize) -> usize {
    lanes.iter().position(|lane| lane.id == id).unwrap_or(0)
}

/// The lanes of `commits`, newest first. Active lanes point at the parent commit that will
/// eventually resolve each path; a parent that is not in the list is a line that leaves.
pub(crate) fn layout_graph(commits: &[GitCommit], head_sha: Option<&str>) -> GraphLayout {
    let mut active_lanes: Vec<ActiveLane> = Vec::new();
    let mut next_lane_id = 0usize;
    let mut next_color_id = 0usize;
    let mut max_lane_count = 0usize;
    let mut rows = Vec::with_capacity(commits.len());

    for commit in commits {
        let before = active_lanes.clone();
        let incoming: Vec<usize> = before
            .iter()
            .enumerate()
            .filter_map(|(index, lane)| (lane.target_sha == commit.sha).then_some(index))
            .collect();
        let primary_incoming = incoming.first().copied();
        let node_lane = primary_incoming.unwrap_or(before.len());
        let primary_lane = primary_incoming.and_then(|index| before.get(index));
        let node_color_id = primary_lane.map(|lane| lane.color_id).unwrap_or_else(|| {
            let color = next_color_id;
            next_color_id += 1;
            color
        });
        let resolved_ids: HashSet<usize> = incoming.iter().map(|&index| before[index].id).collect();
        let mut next_lanes: Vec<ActiveLane> = before
            .iter()
            .filter(|lane| !resolved_ids.contains(&lane.id))
            .cloned()
            .collect();
        let mut outgoing: Vec<(usize, usize)> = Vec::new();

        let mut primary_outgoing_id = None;
        if let Some(first_parent) = commit.parents.first() {
            let id = primary_lane.map(|lane| lane.id).unwrap_or_else(|| {
                let id = next_lane_id;
                next_lane_id += 1;
                id
            });
            let lane = ActiveLane {
                id,
                color_id: node_color_id,
                target_sha: first_parent.clone(),
            };
            next_lanes.insert(node_lane.min(next_lanes.len()), lane.clone());
            primary_outgoing_id = Some(id);
            outgoing.push((id, lane.color_id));
        }

        let mut parent_offset = 1usize;
        for parent_sha in commit.parents.iter().skip(1) {
            if let Some(existing) = next_lanes
                .iter()
                .find(|lane| lane.target_sha == *parent_sha)
            {
                outgoing.push((existing.id, existing.color_id));
                continue;
            }
            let lane = ActiveLane {
                id: next_lane_id,
                color_id: next_color_id,
                target_sha: parent_sha.clone(),
            };
            next_lane_id += 1;
            next_color_id += 1;
            let primary_index = primary_outgoing_id
                .map(|id| lane_index(&next_lanes, id))
                .unwrap_or_else(|| node_lane.min(next_lanes.len()));
            next_lanes.insert(
                (primary_index + parent_offset).min(next_lanes.len()),
                lane.clone(),
            );
            parent_offset += 1;
            outgoing.push((lane.id, lane.color_id));
        }

        let mut segments: Vec<GraphSegment> = before
            .iter()
            .enumerate()
            .filter(|(_, lane)| !resolved_ids.contains(&lane.id))
            .map(|(from_lane, lane)| GraphSegment {
                from_lane,
                to_lane: lane_index(&next_lanes, lane.id),
                color_id: lane.color_id,
                shape: SegmentShape::Through,
            })
            .collect();
        segments.extend(incoming.iter().map(|&from_lane| GraphSegment {
            from_lane,
            to_lane: node_lane,
            color_id: before[from_lane].color_id,
            shape: SegmentShape::Incoming,
        }));
        segments.extend(outgoing.into_iter().map(|(id, color_id)| GraphSegment {
            from_lane: node_lane,
            to_lane: lane_index(&next_lanes, id),
            color_id,
            shape: SegmentShape::Outgoing,
        }));

        max_lane_count = max_lane_count
            .max(before.len())
            .max(next_lanes.len())
            .max(node_lane + 1);
        rows.push(GraphRow {
            sha: commit.sha.clone(),
            node_lane,
            node_color_id,
            segments,
            is_head: head_sha == Some(commit.sha.as_str()),
        });
        active_lanes = next_lanes;
    }

    GraphLayout {
        rows,
        max_lane_count,
    }
}

/// What a person collapses by: a branch or a remote branch (a tag is no lane).
pub(crate) fn branch_key(kind: GitRefKind, name: &str) -> Option<String> {
    match kind {
        GitRefKind::Branch => Some(format!("local:{name}")),
        GitRefKind::Remote => Some(format!("remote:{name}")),
        GitRefKind::Tag | GitRefKind::Head => None,
    }
}

fn has_collapsed_ref(commit: &GitCommit, collapsed: &HashSet<String>) -> bool {
    commit
        .refs
        .iter()
        .any(|r| branch_key(r.kind, &r.name).is_some_and(|key| collapsed.contains(&key)))
}

/// Removes the linear portions of the collapsed branches' lanes, keeping their refs, roots,
/// merges and branch points. Parents that cross a hidden run are contracted to the nearest
/// visible ancestors, so the graph never draws a line toward a row that is gone. Also returns
/// how many commits each collapsed branch hid.
pub(crate) fn collapse_branch_runs(
    commits: &[GitCommit],
    collapsed: &HashSet<String>,
    head_sha: Option<&str>,
) -> (Vec<GitCommit>, HashMap<String, usize>) {
    if collapsed.is_empty() || commits.is_empty() {
        return (commits.to_vec(), HashMap::new());
    }
    let source = layout_graph(commits, head_sha);
    let mut colors_by_ref = HashMap::new();
    for (commit, row) in commits.iter().zip(&source.rows) {
        for r in &commit.refs {
            if let Some(key) = branch_key(r.kind, &r.name)
                && collapsed.contains(&key)
            {
                colors_by_ref.insert(key, row.node_color_id);
            }
        }
    }
    if colors_by_ref.is_empty() {
        return (commits.to_vec(), HashMap::new());
    }
    let collapsed_colors: HashSet<_> = colors_by_ref.values().copied().collect();
    let mut child_counts: HashMap<&str, usize> = HashMap::new();
    for commit in commits {
        for parent in &commit.parents {
            *child_counts.entry(parent.as_str()).or_default() += 1;
        }
    }
    let mut visible = HashSet::new();
    let mut hidden_counts: HashMap<String, usize> = HashMap::new();
    for (commit, row) in commits.iter().zip(&source.rows) {
        let is_selected_tip = has_collapsed_ref(commit, collapsed);
        let is_junction = commit.parents.len() != 1
            || child_counts
                .get(commit.sha.as_str())
                .copied()
                .unwrap_or_default()
                > 1;
        let hide = collapsed_colors.contains(&row.node_color_id)
            && !is_selected_tip
            && commit.refs.is_empty()
            && !is_junction;
        if hide {
            for (key, color) in &colors_by_ref {
                if *color == row.node_color_id {
                    *hidden_counts.entry(key.clone()).or_default() += 1;
                }
            }
        } else {
            visible.insert(commit.sha.clone());
        }
    }
    (compact_commits_to_visible(commits, &visible), hidden_counts)
}

/// Keeps a subset in its order and contracts parent edges across the omitted commits.
pub(crate) fn compact_commits_to_visible(
    commits: &[GitCommit],
    visible: &HashSet<String>,
) -> Vec<GitCommit> {
    let by_sha: HashMap<&str, &GitCommit> = commits.iter().map(|c| (c.sha.as_str(), c)).collect();

    // The nearest visible ancestors of `sha`, by an explicit stack (a gap may be thousands deep).
    fn nearest(
        sha: &str,
        visible: &HashSet<String>,
        by_sha: &HashMap<&str, &GitCommit>,
        memo: &mut HashMap<String, Vec<String>>,
    ) -> Vec<String> {
        if visible.contains(sha) || !by_sha.contains_key(sha) {
            return vec![sha.to_owned()];
        }
        if let Some(cached) = memo.get(sha) {
            return cached.clone();
        }
        struct Frame {
            sha: String,
            next_parent: usize,
            resolved: Vec<String>,
            seen: HashSet<String>,
        }
        impl Frame {
            fn new(sha: String) -> Self {
                Self {
                    sha,
                    next_parent: 0,
                    resolved: Vec::new(),
                    seen: HashSet::new(),
                }
            }
            fn extend(&mut self, parents: &[String]) {
                for parent in parents {
                    if self.seen.insert(parent.clone()) {
                        self.resolved.push(parent.clone());
                    }
                }
            }
        }
        let mut visiting = HashSet::from([sha.to_owned()]);
        let mut stack = vec![Frame::new(sha.to_owned())];
        loop {
            let Some(frame) = stack.last_mut() else {
                return Vec::new();
            };
            let parents = by_sha
                .get(frame.sha.as_str())
                .map_or(&[][..], |c| c.parents.as_slice());
            let next_parent = (frame.next_parent < parents.len()).then(|| {
                let parent = parents[frame.next_parent].clone();
                frame.next_parent += 1;
                parent
            });
            let Some(parent) = next_parent else {
                let Some(frame) = stack.pop() else {
                    return Vec::new();
                };
                visiting.remove(&frame.sha);
                let resolved = frame.resolved;
                memo.insert(frame.sha, resolved.clone());
                if let Some(caller) = stack.last_mut() {
                    caller.extend(&resolved);
                    continue;
                }
                return resolved;
            };
            let resolved = if visible.contains(&parent) || !by_sha.contains_key(parent.as_str()) {
                Some(vec![parent.clone()])
            } else if let Some(cached) = memo.get(&parent) {
                Some(cached.clone())
            } else if visiting.contains(&parent) {
                Some(Vec::new())
            } else {
                None
            };
            match resolved {
                Some(resolved) => {
                    if let Some(frame) = stack.last_mut() {
                        frame.extend(&resolved);
                    }
                }
                None => {
                    visiting.insert(parent.clone());
                    stack.push(Frame::new(parent));
                }
            }
        }
    }

    let mut memo = HashMap::new();
    commits
        .iter()
        .filter(|c| visible.contains(&c.sha))
        .cloned()
        .map(|mut commit| {
            let mut seen = HashSet::new();
            commit.parents = commit
                .parents
                .iter()
                .flat_map(|p| nearest(p, visible, &by_sha, &mut memo))
                .filter(|p| seen.insert(p.clone()))
                .collect();
            commit
        })
        .collect()
}

// --- badges, authors, labels -----------------------------------------------------------------------

/// One ref badge of a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Badge {
    pub(crate) label: String,
    pub(crate) kind: GitRefKind,
    /// What clicking the badge collapses (a branch), when it can.
    pub(crate) key: Option<String>,
    /// How many commits it hides now, when collapsed.
    pub(crate) hidden: usize,
}

fn kind_order(kind: GitRefKind) -> u8 {
    match kind {
        GitRefKind::Head => 0,
        GitRefKind::Branch => 1,
        GitRefKind::Remote => 2,
        GitRefKind::Tag => 3,
    }
}

/// The badges of a commit: HEAD, branches, remote branches, then tags.
pub(crate) fn badges(commit: &GitCommit, hidden: &HashMap<String, usize>) -> Vec<Badge> {
    let mut out: Vec<Badge> = commit
        .refs
        .iter()
        .map(|r| {
            let key = branch_key(r.kind, &r.name);
            Badge {
                label: r.name.clone(),
                kind: r.kind,
                hidden: key
                    .as_ref()
                    .and_then(|k| hidden.get(k))
                    .copied()
                    .unwrap_or(0),
                key,
            }
        })
        .collect();
    out.sort_by_key(|b| kind_order(b.kind));
    out
}

/// A colour for an author, deterministic from the wire's `authorKey` (a SHA-256 hex): a hue in
/// `[0, 1)`. No avatars are fetched.
pub(crate) fn author_hue(key: &str) -> f32 {
    let n = key
        .get(..8)
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .unwrap_or_else(|| {
            key.bytes()
                .fold(0u32, |a, b| a.wrapping_mul(31) ^ u32::from(b))
        });
    (n % 360) as f32 / 360.0
}

/// The first letter of a name, upper-cased.
pub(crate) fn initial(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".to_owned())
}

/// The seven-character name of a commit.
pub(crate) fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

// --- the model -----------------------------------------------------------------------------------

/// What the model asks the daemon. The screen answers with the same ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    /// `git.log`, one page, from `cursor`.
    Page {
        cursor: Option<String>,
        limit: u32,
        ticket: u64,
    },
    /// `git.log_search`.
    Search { query: String, ticket: u64 },
    /// `git.log` over the thread's own commits (`rev: thread`): which rows can open a diff.
    Thread { ticket: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// Not asked yet.
    Idle,
    Loading,
    Ready,
    Failed(String),
}

/// Whether a row's commit can open its diff: the daemon refuses commits before the thread's base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Openable {
    Yes,
    /// Older than the thread's base: no diff is offered.
    BeforeBase,
    /// The log could not say (no worktree, or its answer failed): opening shows the daemon's
    /// word inline if it refuses.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ThreadSet {
    Unknown,
    Asking,
    Known { shas: HashSet<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SearchState {
    Loading,
    Ready { truncated: bool },
    Failed(String),
}

/// What the tab knows about one run's commit log.
#[derive(Clone, Debug)]
pub(crate) struct HistoryModel {
    page_size: u32,
    pub(crate) status: Status,
    commits: Vec<GitCommit>,
    next_cursor: Option<String>,
    head: Option<String>,
    pub(crate) repo: bool,
    pub(crate) in_place: bool,
    pub(crate) shallow: bool,
    /// A next page is being fetched, and why the last attempt failed.
    pub(crate) loading_more: bool,
    pub(crate) more_error: Option<String>,
    /// What the search field holds, and what was sent (`None`: the log shows).
    typed: String,
    search: Option<(String, SearchState, Vec<GitCommit>)>,
    selected: Option<String>,
    collapsed: HashSet<String>,
    thread: ThreadSet,
    tickets: u64,
    page_ticket: u64,
    search_ticket: u64,
    thread_ticket: u64,
    // What the list shows, rebuilt when anything above changes.
    shown: Vec<GitCommit>,
    layout: GraphLayout,
    hidden: HashMap<String, usize>,
}

impl Default for HistoryModel {
    fn default() -> Self {
        Self::with_page_size(PAGE)
    }
}

impl HistoryModel {
    pub(crate) fn with_page_size(page_size: u32) -> Self {
        Self {
            page_size,
            status: Status::Idle,
            commits: Vec::new(),
            next_cursor: None,
            head: None,
            repo: true,
            in_place: false,
            shallow: false,
            loading_more: false,
            more_error: None,
            typed: String::new(),
            search: None,
            selected: None,
            collapsed: HashSet::new(),
            thread: ThreadSet::Unknown,
            tickets: 0,
            page_ticket: 0,
            search_ticket: 0,
            thread_ticket: 0,
            shown: Vec::new(),
            layout: GraphLayout::default(),
            hidden: HashMap::new(),
        }
    }

    fn ticket(&mut self) -> u64 {
        self.tickets += 1;
        self.tickets
    }

    /// Asks for the first page and the thread's own commits. Also how the log is read again.
    pub(crate) fn start(&mut self) -> Vec<Request> {
        let (page, thread) = (self.ticket(), self.ticket());
        self.page_ticket = page;
        self.thread_ticket = thread;
        // What was read stays on screen while the new answer comes.
        if self.status != Status::Ready {
            self.status = Status::Loading;
        }
        self.loading_more = false;
        self.more_error = None;
        self.thread = ThreadSet::Asking;
        let mut out = vec![
            Request::Page {
                cursor: None,
                limit: self.page_size,
                ticket: page,
            },
            Request::Thread { ticket: thread },
        ];
        if let Some(query) = self.sent_query() {
            out.extend(self.search_request(query));
        }
        out
    }

    fn sent_query(&self) -> Option<String> {
        self.search.as_ref().map(|(q, _, _)| q.clone())
    }

    pub(crate) fn apply_page(
        &mut self,
        ticket: u64,
        first: bool,
        result: Result<GitLogResult, String>,
    ) {
        if ticket != self.page_ticket {
            return;
        }
        match result {
            Ok(log) => {
                self.repo = log.repo;
                self.in_place = log.in_place;
                self.shallow = log.shallow;
                self.head = log.head;
                self.next_cursor = log.next_cursor;
                if first {
                    self.commits = log.commits;
                } else {
                    // A page can repeat a commit the last one ended on: keep each once.
                    let have: HashSet<&str> = self.commits.iter().map(|c| c.sha.as_str()).collect();
                    let fresh: Vec<GitCommit> = log
                        .commits
                        .into_iter()
                        .filter(|c| !have.contains(c.sha.as_str()))
                        .collect();
                    self.commits.extend(fresh);
                }
                self.status = Status::Ready;
                self.loading_more = false;
                self.more_error = None;
            }
            Err(why) => {
                if first {
                    self.status = Status::Failed(why);
                } else {
                    self.more_error = Some(why);
                }
                self.loading_more = false;
            }
        }
        self.rebuild();
    }

    /// The next page, when there is one and nothing is in flight; none while a search shows.
    pub(crate) fn load_more(&mut self) -> Option<Request> {
        if self.search.is_some() || self.loading_more || self.status != Status::Ready {
            return None;
        }
        let cursor = self.next_cursor.clone()?;
        self.loading_more = true;
        self.more_error = None;
        let ticket = self.ticket();
        self.page_ticket = ticket;
        Some(Request::Page {
            cursor: Some(cursor),
            limit: self.page_size,
            ticket,
        })
    }

    pub(crate) fn has_more(&self) -> bool {
        self.search.is_none() && self.next_cursor.is_some()
    }

    pub(crate) fn apply_thread(&mut self, ticket: u64, result: Result<GitLogResult, String>) {
        if ticket != self.thread_ticket {
            return;
        }
        self.thread = match result {
            // A page that did not hold them all cannot say what is missing.
            Ok(log) if log.repo && !log.in_place && log.next_cursor.is_none() => ThreadSet::Known {
                shas: log.commits.into_iter().map(|c| c.sha).collect(),
            },
            _ => ThreadSet::Unknown,
        };
    }

    /// Whether `sha` can open a diff.
    pub(crate) fn openable(&self, sha: &str) -> Openable {
        match &self.thread {
            ThreadSet::Known { shas } if shas.contains(sha) => Openable::Yes,
            ThreadSet::Known { .. } => Openable::BeforeBase,
            ThreadSet::Unknown | ThreadSet::Asking => Openable::Unknown,
        }
    }

    // --- search -------------------------------------------------------------------------------

    /// The search field changed. Returns whether the text is new.
    pub(crate) fn set_query(&mut self, text: &str) -> bool {
        if self.typed == text {
            return false;
        }
        self.typed = text.to_owned();
        true
    }

    pub(crate) fn query(&self) -> &str {
        &self.typed
    }

    fn search_request(&mut self, query: String) -> Option<Request> {
        let ticket = self.ticket();
        self.search_ticket = ticket;
        Some(Request::Search { query, ticket })
    }

    /// Sends what is typed (the caller waits out the debounce). A blank query leaves search:
    /// the daemon refuses one.
    pub(crate) fn commit_search(&mut self) -> Option<Request> {
        let mut query = self.typed.trim().to_owned();
        if query.is_empty() {
            if self.search.take().is_some() {
                self.search_ticket = self.ticket();
                self.rebuild();
            }
            return None;
        }
        // The daemon refuses a longer one.
        while query.len() > reno_proto::GIT_LOG_QUERY_MAX_BYTES {
            query.pop();
        }
        let keep = self
            .search
            .as_ref()
            .filter(|(q, _, _)| *q == query)
            .map(|(_, _, found)| found.clone())
            .unwrap_or_default();
        self.search = Some((query.clone(), SearchState::Loading, keep));
        self.rebuild();
        self.search_request(query)
    }

    pub(crate) fn apply_search(&mut self, ticket: u64, result: Result<GitLogSearchResult, String>) {
        if ticket != self.search_ticket {
            return;
        }
        let Some((_, state, found)) = self.search.as_mut() else {
            return;
        };
        match result {
            Ok(r) => {
                *found = r.commits;
                *state = SearchState::Ready {
                    truncated: r.truncated,
                };
            }
            Err(why) => *state = SearchState::Failed(why),
        }
        self.rebuild();
    }

    #[cfg(test)]
    pub(crate) fn searching(&self) -> bool {
        self.search.is_some()
    }

    pub(crate) fn search_state(&self) -> Option<&SearchState> {
        self.search.as_ref().map(|(_, s, _)| s)
    }

    // --- selection, collapse --------------------------------------------------------------------

    pub(crate) fn select(&mut self, sha: Option<&str>) {
        self.selected = sha.map(str::to_owned);
    }

    pub(crate) fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Moves the selection `by` rows (the first row when nothing is selected).
    pub(crate) fn step(&mut self, by: isize) -> Option<usize> {
        if self.shown.is_empty() {
            return None;
        }
        let at = self
            .selected
            .as_deref()
            .and_then(|s| self.shown.iter().position(|c| c.sha == s));
        let to = match at {
            Some(i) => i.saturating_add_signed(by).min(self.shown.len() - 1),
            None => 0,
        };
        self.selected = Some(self.shown[to].sha.clone());
        Some(to)
    }

    /// Collapses or expands a branch's linear run of commits.
    pub(crate) fn toggle_branch(&mut self, key: &str) {
        if !self.collapsed.remove(key) {
            self.collapsed.insert(key.to_owned());
        }
        self.rebuild();
    }

    #[cfg(test)]
    pub(crate) fn is_collapsed(&self, key: &str) -> bool {
        self.collapsed.contains(key)
    }

    // --- what shows -----------------------------------------------------------------------------

    fn rebuild(&mut self) {
        match &self.search {
            Some((_, _, found)) => {
                // Matches are by time, not topology: no lines, only nodes.
                self.shown = found
                    .iter()
                    .cloned()
                    .map(|mut c| {
                        c.parents.clear();
                        c
                    })
                    .collect();
                self.hidden.clear();
            }
            None => {
                let (shown, hidden) =
                    collapse_branch_runs(&self.commits, &self.collapsed, self.head.as_deref());
                self.shown = shown;
                self.hidden = hidden;
            }
        }
        self.layout = layout_graph(&self.shown, self.head.as_deref());
    }

    pub(crate) fn rows(&self) -> &[GitCommit] {
        &self.shown
    }

    pub(crate) fn graph(&self) -> &GraphLayout {
        &self.layout
    }

    pub(crate) fn hidden(&self) -> &HashMap<String, usize> {
        &self.hidden
    }

    pub(crate) fn commit(&self, sha: &str) -> Option<&GitCommit> {
        self.commits
            .iter()
            .chain(self.search.iter().flat_map(|(_, _, f)| f.iter()))
            .find(|c| c.sha == sha)
    }

    /// How many commits of the log are loaded.
    #[cfg(test)]
    pub(crate) fn loaded(&self) -> usize {
        self.commits.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reno_proto::GitRef;

    fn commit(sha: &str, parents: &[&str]) -> GitCommit {
        GitCommit {
            sha: sha.to_owned(),
            parents: parents.iter().map(|p| (*p).to_owned()).collect(),
            subject: format!("subject {sha}"),
            author_name: "Ada".to_owned(),
            author_key: reno_proto::author_key("ada@x.invalid"),
            time_ms: 0,
            refs: Vec::new(),
        }
    }

    fn with_ref(mut c: GitCommit, kind: GitRefKind, name: &str) -> GitCommit {
        c.refs.push(GitRef {
            name: name.to_owned(),
            kind,
        });
        c
    }

    fn lanes(layout: &GraphLayout) -> Vec<usize> {
        layout.rows.iter().map(|r| r.node_lane).collect()
    }

    fn log(commits: Vec<GitCommit>, cursor: Option<&str>) -> GitLogResult {
        GitLogResult {
            head: commits.first().map(|c| c.sha.clone()),
            commits,
            next_cursor: cursor.map(str::to_owned),
            repo: true,
            shallow: false,
            in_place: false,
        }
    }

    #[test]
    fn a_straight_history_is_one_lane() {
        let commits = vec![commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])];
        let g = layout_graph(&commits, Some("c"));
        assert_eq!(lanes(&g), [0, 0, 0]);
        assert_eq!(g.max_lane_count, 1);
        assert!(g.rows[0].is_head && !g.rows[1].is_head);
        // The root has a line coming in and none leaving.
        assert!(
            g.rows[2]
                .segments
                .iter()
                .all(|s| s.shape == SegmentShape::Incoming)
        );
    }

    #[test]
    fn the_demo_merge_forks_into_two_lanes_and_joins_at_the_base() {
        // M(F,S) F(B) S(B) B(C) C(A) A
        let commits = vec![
            commit("m", &["f", "s"]),
            commit("f", &["b"]),
            commit("s", &["b"]),
            commit("b", &["c"]),
            commit("c", &["a"]),
            commit("a", &[]),
        ];
        let g = layout_graph(&commits, Some("m"));
        assert_eq!(lanes(&g), [0, 0, 1, 0, 0, 0]);
        assert_eq!(g.max_lane_count, 2);
        // The merge sends a line to each parent.
        let out: Vec<_> = g.rows[0]
            .segments
            .iter()
            .filter(|s| s.shape == SegmentShape::Outgoing)
            .map(|s| (s.from_lane, s.to_lane))
            .collect();
        assert_eq!(out, [(0, 0), (0, 1)]);
        // The base receives both lanes.
        let incoming: Vec<_> = g.rows[3]
            .segments
            .iter()
            .filter(|s| s.shape == SegmentShape::Incoming)
            .map(|s| s.from_lane)
            .collect();
        assert_eq!(incoming, [0, 1]);
        // The two lines have two colours; the merge keeps its first parent's.
        let colors: HashSet<usize> = g.rows[0].segments.iter().map(|s| s.color_id).collect();
        assert_eq!(colors.len(), 2);
    }

    #[test]
    fn a_fork_that_never_joins_keeps_its_lane_open() {
        // Two tips on one base: x(b) y(b) b.
        let commits = vec![commit("x", &["b"]), commit("y", &["b"]), commit("b", &[])];
        let g = layout_graph(&commits, None);
        assert_eq!(lanes(&g), [0, 1, 0]);
        assert_eq!(g.max_lane_count, 2);
    }

    #[test]
    fn a_parent_missing_from_the_page_is_a_line_that_leaves() {
        let commits = vec![commit("b", &["gone"])];
        let g = layout_graph(&commits, None);
        assert_eq!(lanes(&g), [0]);
        assert!(
            g.rows[0]
                .segments
                .iter()
                .any(|s| s.shape == SegmentShape::Outgoing)
        );
    }

    #[test]
    fn search_compaction_connects_matches_across_hidden_commits() {
        let commits = vec![
            commit("tip", &["middle"]),
            commit("middle", &["base"]),
            commit("base", &["root"]),
            commit("root", &[]),
        ];
        let visible = HashSet::from(["tip".to_owned(), "base".to_owned()]);
        let compact = compact_commits_to_visible(&commits, &visible);
        let shas: Vec<_> = compact.iter().map(|c| c.sha.as_str()).collect();
        assert_eq!(shas, ["tip", "base"]);
        assert_eq!(compact[0].parents, ["base"]);
        assert!(compact[1].parents.is_empty());
    }

    #[test]
    fn compaction_handles_a_twenty_thousand_commit_gap() {
        const DEPTH: usize = 20_000;
        let commits: Vec<GitCommit> = (0..DEPTH)
            .rev()
            .map(|i| {
                let sha = format!("c{i:05}");
                if i == 0 {
                    commit(&sha, &[])
                } else {
                    commit(&sha, &[format!("c{:05}", i - 1).as_str()])
                }
            })
            .collect();
        let newest = format!("c{:05}", DEPTH - 1);
        let visible = HashSet::from([newest.clone(), "c00000".to_owned()]);
        let compact = compact_commits_to_visible(&commits, &visible);
        assert_eq!(compact.len(), 2);
        assert_eq!(compact[0].parents, ["c00000"]);
    }

    #[test]
    fn collapsing_a_branch_contracts_linear_parents_and_keeps_junctions() {
        let commits = vec![
            with_ref(
                commit("feature", &["middle"]),
                GitRefKind::Branch,
                "feature",
            ),
            commit("middle", &["base"]),
            with_ref(commit("main", &["base"]), GitRefKind::Branch, "main"),
            commit("base", &["root"]),
            commit("root", &[]),
        ];
        let collapsed = HashSet::from(["local:feature".to_owned()]);
        let (visible, counts) = collapse_branch_runs(&commits, &collapsed, Some("main"));
        let shas: Vec<_> = visible.iter().map(|c| c.sha.as_str()).collect();
        assert_eq!(shas, ["feature", "main", "base", "root"]);
        assert_eq!(visible[0].parents, ["base"]);
        assert_eq!(counts.get("local:feature"), Some(&1));
    }

    #[test]
    fn badges_come_head_first_then_branches_remotes_and_tags() {
        let mut c = commit("x", &[]);
        for (kind, name) in [
            (GitRefKind::Tag, "v1"),
            (GitRefKind::Remote, "origin/main"),
            (GitRefKind::Branch, "main"),
            (GitRefKind::Head, "HEAD"),
        ] {
            c = with_ref(c, kind, name);
        }
        let b = badges(&c, &HashMap::new());
        let labels: Vec<_> = b.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, ["HEAD", "main", "origin/main", "v1"]);
        assert_eq!(b[1].key.as_deref(), Some("local:main"));
        assert_eq!(b[3].key, None, "a tag collapses nothing");
    }

    #[test]
    fn an_author_gets_the_same_colour_every_time_and_no_email_is_needed() {
        let key = reno_proto::author_key("ada@x.invalid");
        assert_eq!(author_hue(&key), author_hue(&key));
        assert!((0.0..1.0).contains(&author_hue(&key)));
        assert_ne!(
            author_hue(&reno_proto::author_key("a@x.invalid")),
            author_hue(&reno_proto::author_key("someone.else@y.invalid"))
        );
        assert_eq!(initial("ada lovelace"), "A");
        assert_eq!(initial(""), "?");
        assert_eq!(short("abcdef0123"), "abcdef0");
    }

    #[test]
    fn paging_appends_pages_until_the_cursor_ends_and_asks_once_at_a_time() {
        let mut m = HistoryModel::with_page_size(2);
        let reqs = m.start();
        let Request::Page { ticket, limit, .. } = reqs[0].clone() else {
            panic!("a page first");
        };
        assert_eq!(limit, 2);
        assert!(matches!(reqs[1], Request::Thread { .. }));
        assert_eq!(m.status, Status::Loading);
        m.apply_page(
            ticket,
            true,
            Ok(log(
                vec![commit("c", &["b"]), commit("b", &["a"])],
                Some("n1"),
            )),
        );
        assert_eq!(m.status, Status::Ready);
        assert!(m.has_more());
        let Some(Request::Page {
            cursor, ticket: t2, ..
        }) = m.load_more()
        else {
            panic!("the next page");
        };
        assert_eq!(cursor.as_deref(), Some("n1"));
        assert!(m.load_more().is_none(), "one request at a time");
        m.apply_page(t2, false, Ok(log(vec![commit("a", &[])], None)));
        assert_eq!(m.loaded(), 3);
        assert!(!m.has_more());
        assert_eq!(lanes(m.graph()), [0, 0, 0]);
    }

    #[test]
    fn a_late_answer_to_an_old_question_is_dropped() {
        let mut m = HistoryModel::default();
        let first = m.start();
        let second = m.start();
        let (Request::Page { ticket: t1, .. }, Request::Page { ticket: t2, .. }) =
            (first[0].clone(), second[0].clone())
        else {
            panic!()
        };
        m.apply_page(t1, true, Ok(log(vec![commit("old", &[])], None)));
        assert_eq!(m.loaded(), 0);
        m.apply_page(t2, true, Ok(log(vec![commit("new", &[])], None)));
        assert_eq!(m.rows()[0].sha, "new");
    }

    #[test]
    fn a_failed_first_page_says_so_and_a_failed_next_page_keeps_the_log() {
        let mut m = HistoryModel::default();
        let Request::Page { ticket, .. } = m.start()[0].clone() else {
            panic!()
        };
        m.apply_page(ticket, true, Err("git is gone".to_owned()));
        assert_eq!(m.status, Status::Failed("git is gone".to_owned()));

        let Request::Page { ticket, .. } = m.start()[0].clone() else {
            panic!()
        };
        m.apply_page(ticket, true, Ok(log(vec![commit("a", &[])], Some("n"))));
        let Some(Request::Page { ticket, .. }) = m.load_more() else {
            panic!()
        };
        m.apply_page(ticket, false, Err("timed out".to_owned()));
        assert_eq!(m.status, Status::Ready);
        assert_eq!(m.more_error.as_deref(), Some("timed out"));
        assert_eq!(m.loaded(), 1);
        assert!(m.load_more().is_some(), "it can be asked again");
    }

    #[test]
    fn search_replaces_the_list_by_matches_with_no_lines_and_a_blank_query_leaves_it() {
        let mut m = HistoryModel::default();
        let Request::Page { ticket, .. } = m.start()[0].clone() else {
            panic!()
        };
        m.apply_page(
            ticket,
            true,
            Ok(log(vec![commit("b", &["a"]), commit("a", &[])], Some("n"))),
        );
        assert!(m.set_query("lease"));
        assert!(!m.set_query("lease"));
        let Some(Request::Search { query, ticket }) = m.commit_search() else {
            panic!("a search");
        };
        assert_eq!(query, "lease");
        assert!(m.searching());
        assert!(m.load_more().is_none(), "a search has no next page");
        m.apply_search(
            ticket,
            Ok(GitLogSearchResult {
                commits: vec![commit("a", &[]), commit("b", &["a"])],
                truncated: true,
                repo: true,
                in_place: false,
            }),
        );
        assert_eq!(
            m.search_state(),
            Some(&SearchState::Ready { truncated: true })
        );
        assert_eq!(
            lanes(m.graph()),
            [0, 0],
            "matches are not laid out as a graph"
        );
        assert!(m.rows().iter().all(|c| c.parents.is_empty()));

        // Typing nothing goes back to the log without asking the daemon.
        m.set_query("   ");
        assert!(m.commit_search().is_none());
        assert!(!m.searching());
        assert_eq!(m.rows().len(), 2);
        assert_eq!(m.rows()[0].sha, "b");
    }

    #[test]
    fn an_old_search_answer_does_not_replace_a_newer_one() {
        let mut m = HistoryModel::default();
        m.set_query("a");
        let Some(Request::Search { ticket: t1, .. }) = m.commit_search() else {
            panic!()
        };
        m.set_query("ab");
        let Some(Request::Search { ticket: t2, .. }) = m.commit_search() else {
            panic!()
        };
        let found = |sha: &str| GitLogSearchResult {
            commits: vec![commit(sha, &[])],
            truncated: false,
            repo: true,
            in_place: false,
        };
        m.apply_search(t2, Ok(found("two")));
        m.apply_search(t1, Ok(found("one")));
        assert_eq!(m.rows()[0].sha, "two");
    }

    #[test]
    fn a_long_query_is_cut_to_what_the_daemon_takes() {
        let mut m = HistoryModel::default();
        m.set_query(&"x".repeat(400));
        let Some(Request::Search { query, .. }) = m.commit_search() else {
            panic!()
        };
        assert_eq!(query.len(), reno_proto::GIT_LOG_QUERY_MAX_BYTES);
    }

    #[test]
    fn the_threads_own_commits_open_and_older_ones_do_not() {
        let mut m = HistoryModel::default();
        let reqs = m.start();
        let Request::Thread { ticket } = reqs[1].clone() else {
            panic!()
        };
        assert_eq!(m.openable("m"), Openable::Unknown, "not answered yet");
        m.apply_thread(ticket, Ok(log(vec![commit("m", &[])], None)));
        assert_eq!(m.openable("m"), Openable::Yes);
        assert_eq!(m.openable("old"), Openable::BeforeBase);

        // A thread log that did not fit in one page cannot say what is older.
        let Request::Thread { ticket } = m.start()[1].clone() else {
            panic!()
        };
        m.apply_thread(ticket, Ok(log(vec![commit("m", &[])], Some("more"))));
        assert_eq!(m.openable("old"), Openable::Unknown);

        // Neither can one that failed, or one for a run with no worktree.
        let Request::Thread { ticket } = m.start()[1].clone() else {
            panic!()
        };
        m.apply_thread(ticket, Err("refused".to_owned()));
        assert_eq!(m.openable("m"), Openable::Unknown);
        let Request::Thread { ticket } = m.start()[1].clone() else {
            panic!()
        };
        let mut in_place = log(vec![], None);
        in_place.in_place = true;
        m.apply_thread(ticket, Ok(in_place));
        assert_eq!(m.openable("m"), Openable::Unknown);
    }

    #[test]
    fn selection_steps_through_the_rows_and_stops_at_the_ends() {
        let mut m = HistoryModel::default();
        let Request::Page { ticket, .. } = m.start()[0].clone() else {
            panic!()
        };
        m.apply_page(
            ticket,
            true,
            Ok(log(
                vec![commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])],
                None,
            )),
        );
        assert_eq!(m.step(1), Some(0), "the first row when nothing is selected");
        assert_eq!(m.step(1), Some(1));
        assert_eq!(m.step(5), Some(2));
        assert_eq!(m.step(-1), Some(1));
        assert_eq!(m.selected(), Some("b"));
        m.select(None);
        assert_eq!(m.selected(), None);
    }

    #[test]
    fn collapsing_a_branch_hides_its_run_and_counts_it() {
        let mut m = HistoryModel::default();
        let Request::Page { ticket, .. } = m.start()[0].clone() else {
            panic!()
        };
        m.apply_page(
            ticket,
            true,
            Ok(log(
                vec![
                    with_ref(commit("f2", &["f1"]), GitRefKind::Branch, "feature"),
                    commit("f1", &["b"]),
                    with_ref(commit("m", &["b"]), GitRefKind::Branch, "main"),
                    commit("b", &[]),
                ],
                None,
            )),
        );
        assert_eq!(m.rows().len(), 4);
        m.toggle_branch("local:feature");
        assert_eq!(m.rows().len(), 3);
        assert_eq!(m.hidden().get("local:feature"), Some(&1));
        assert!(m.is_collapsed("local:feature"));
        m.toggle_branch("local:feature");
        assert_eq!(m.rows().len(), 4);
    }
}
