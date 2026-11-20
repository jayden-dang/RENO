//! The ⌘K palette (brief: docs/design/2026-11-19-palette-ui-brief.md): which tab, section and
//! row it shows, from the threads the sidebar lists, the focused thread's file search and the
//! command list. Pure: the screen fetches, this decides.

use std::ops::Range;

use reno_proto::RunId;

use crate::ui::palette::{Entry, entries, filter};
use crate::vm::projects::{Badge, Dot, Folder};

/// What the palette searches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Tab {
    All,
    Threads,
    Files,
    Text,
    Commands,
}

impl Tab {
    /// In the order the tab row shows them; ⌘1–⌘5 pick them by place.
    pub(crate) const ALL: [Self; 5] = [
        Self::All,
        Self::Threads,
        Self::Files,
        Self::Text,
        Self::Commands,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Threads => "Threads",
            Self::Files => "Files",
            Self::Text => "Text",
            Self::Commands => "Commands",
        }
    }

    /// The tab ⌘`n` picks (1-based).
    pub(crate) fn from_digit(n: usize) -> Option<Self> {
        n.checked_sub(1).and_then(|i| Self::ALL.get(i).copied())
    }

    /// The tab after (or before) this one, wrapping: ⇥ and ⇧⇥.
    pub(crate) fn step(self, forward: bool) -> Self {
        let n = Self::ALL.len();
        let at = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[if forward {
            (at + 1) % n
        } else {
            (at + n - 1) % n
        }]
    }
}

/// One thread the palette can open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Thread {
    /// Its latest run: the one ↵ opens.
    pub(crate) run: RunId,
    /// The title without its bot and repository prefix, as the sidebar shows it.
    pub(crate) title: String,
    pub(crate) project: String,
    pub(crate) executor: String,
    pub(crate) badge: Option<Badge>,
    pub(crate) dot: Dot,
    pub(crate) age: String,
    pub(crate) updated_ms: u64,
}

/// One matching line of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) path: String,
    pub(crate) line: u32,
    pub(crate) text: String,
}

/// Where a file search stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Found<T> {
    /// No thread is focused: there is no project to search.
    NoThread,
    /// Asked, not answered yet.
    Pending,
    Done(Vec<T>),
    /// The daemon refused; the reason in words.
    Failed(String),
}

/// What the palette is drawn from.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Inputs<'a> {
    pub(crate) threads: &'a [Thread],
    /// The focused thread's project: the one whose files are searched.
    pub(crate) project: Option<&'a str>,
    pub(crate) files: &'a Found<String>,
    pub(crate) lines: &'a Found<Line>,
}

/// What a section is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    NeedsYou,
    Recent,
    Threads,
    Files,
    Text,
    Commands,
}

impl Kind {
    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs you",
            Self::Recent => "Recent",
            Self::Threads => "Threads",
            Self::Files => "Files",
            Self::Text => "In files",
            Self::Commands => "Commands",
        }
    }
}

/// One selectable row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Item {
    Thread(Thread),
    File(String),
    Line(Line),
    Command(Entry),
}

/// A note under a section: why it has nothing, or that it is not finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Note {
    pub(crate) text: String,
    /// A search is in flight: the note carries a spinner.
    pub(crate) busy: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Section {
    pub(crate) kind: Kind,
    /// Drawn with a head (the All tab); a single tab lists rows only.
    pub(crate) head: bool,
    /// The finding the head states ("3 threads need you").
    pub(crate) finding: Option<String>,
    /// The count on the head's right.
    pub(crate) count: Option<usize>,
    pub(crate) items: Vec<Item>,
    /// "+4 commands · ⇥ to see all".
    pub(crate) more: Option<String>,
    pub(crate) note: Option<Note>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TabView {
    pub(crate) tab: Tab,
    pub(crate) count: Option<usize>,
    pub(crate) active: bool,
}

/// Everything the palette draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sheet {
    /// The tab in force (a prefix in the query wins over the clicked tab).
    pub(crate) tab: Tab,
    pub(crate) tabs: Vec<TabView>,
    pub(crate) sections: Vec<Section>,
    /// "Nothing matches “qzx”" when typing found nothing.
    pub(crate) nothing: Option<String>,
}

impl Sheet {
    /// The selectable rows, in order.
    pub(crate) fn items(&self) -> Vec<&Item> {
        self.sections.iter().flat_map(|s| s.items.iter()).collect()
    }
}

/// The tab a leading `>` `/` `#` asks for, and the query without it.
pub(crate) fn scope(query: &str, tab: Tab) -> (Tab, &str) {
    for (sign, scoped) in [(">", Tab::Commands), ("/", Tab::Files), ("#", Tab::Text)] {
        if let Some(rest) = query.strip_prefix(sign) {
            return (scoped, rest);
        }
    }
    (tab, query)
}

/// Where the query first appears in `text`, case-insensitively: the part a row emphasises.
pub(crate) fn mark(text: &str, query: &str) -> Option<Range<usize>> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();
    // Lower-casing keeps byte offsets only when it keeps the length; otherwise mark nothing.
    if lower.len() != text.len() {
        return None;
    }
    let at = lower.find(&query.to_lowercase())?;
    Some(at..at + query.len())
}

/// The palette for `query` on `tab`.
pub(crate) fn sheet(inputs: &Inputs<'_>, query: &str, tab: Tab) -> Sheet {
    let (tab, query) = scope(query, tab);
    let query = query.trim();
    if query.is_empty() {
        return inbox(inputs, tab);
    }

    // Every kind's full result, in order.
    let threads = matching_threads(inputs.threads, query);
    let files = matching_files(inputs, query);
    let lines = matching_lines(inputs);
    let commands: Vec<Item> = filter(query, &entries())
        .into_iter()
        .map(Item::Command)
        .collect();
    let file_note = note(
        inputs,
        inputs.files,
        "Open a thread to search its project's files",
    );
    let line_note = note(
        inputs,
        inputs.lines,
        "Open a thread to search inside its files",
    );

    let tabs = Tab::ALL
        .into_iter()
        .map(|t| TabView {
            tab: t,
            count: match t {
                Tab::All => None,
                Tab::Threads => Some(threads.len()),
                Tab::Files => done(inputs.files).then_some(files.len()),
                Tab::Text => done(inputs.lines).then_some(lines.len()),
                Tab::Commands => Some(commands.len()),
            },
            active: t == tab,
        })
        .collect();

    let all = tab == Tab::All;
    let mut sections = vec![];
    for (kind, items, note) in [
        (Kind::Threads, threads, None),
        (Kind::Files, files, file_note),
        (Kind::Text, lines, line_note),
        (Kind::Commands, commands, None),
    ] {
        if !all && tab_of(kind) != tab {
            continue;
        }
        if items.is_empty() && note.is_none() {
            continue;
        }
        sections.push(if all {
            capped(kind, items, 4, note)
        } else {
            Section {
                kind,
                head: false,
                finding: None,
                count: None,
                items,
                more: None,
                note,
            }
        });
    }

    let found_any = sections.iter().any(|s| !s.items.is_empty());
    let busy = sections
        .iter()
        .any(|s| s.note.as_ref().is_some_and(|n| n.busy));
    let nothing = (!found_any && !busy).then(|| format!("Nothing matches \u{201c}{query}\u{201d}"));
    if nothing.is_some() {
        sections.retain(|s| s.note.is_some());
    }
    Sheet {
        tab,
        tabs,
        sections,
        nothing,
    }
}

/// The empty query: what needs you, what you did last, and the commands.
fn inbox(inputs: &Inputs<'_>, tab: Tab) -> Sheet {
    let mut waiting: Vec<&Thread> = inputs
        .threads
        .iter()
        .filter(|t| t.badge.is_some())
        .collect();
    waiting.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
    let mut recent: Vec<&Thread> = inputs
        .threads
        .iter()
        .filter(|t| t.badge.is_none())
        .collect();
    recent.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
    let commands: Vec<Item> = entries().into_iter().map(Item::Command).collect();
    let tabs = Tab::ALL
        .into_iter()
        .map(|t| TabView {
            tab: t,
            count: (t == Tab::Threads).then_some(inputs.threads.len()),
            active: t == tab,
        })
        .collect();

    let mut sections = vec![];
    match tab {
        Tab::All => {
            if !waiting.is_empty() {
                let n = waiting.len();
                sections.push(Section {
                    kind: Kind::NeedsYou,
                    head: true,
                    finding: Some(if n == 1 {
                        "1 thread needs you".to_owned()
                    } else {
                        format!("{n} threads need you")
                    }),
                    count: None,
                    items: waiting.into_iter().cloned().map(Item::Thread).collect(),
                    more: None,
                    note: None,
                });
            }
            if !recent.is_empty() {
                let items = recent.into_iter().cloned().map(Item::Thread).collect();
                let mut section = capped(Kind::Recent, items, 5, None);
                section.more = None;
                section.count = Some(section.items.len());
                sections.push(section);
            }
            sections.push(capped(Kind::Commands, commands, 3, None));
        }
        Tab::Threads => {
            let items = waiting
                .into_iter()
                .chain(recent)
                .cloned()
                .map(Item::Thread)
                .collect();
            sections.push(single(Kind::Threads, items, None));
        }
        Tab::Commands => sections.push(single(Kind::Commands, commands, None)),
        Tab::Files | Tab::Text => {}
    }
    Sheet {
        tab,
        tabs,
        sections,
        nothing: None,
    }
}

fn single(kind: Kind, items: Vec<Item>, note: Option<Note>) -> Section {
    Section {
        kind,
        head: false,
        finding: None,
        count: None,
        items,
        more: None,
        note,
    }
}

/// A headed section showing at most `cap` rows, the rest counted.
fn capped(kind: Kind, mut items: Vec<Item>, cap: usize, note: Option<Note>) -> Section {
    let total = items.len();
    items.truncate(cap);
    Section {
        kind,
        head: true,
        finding: None,
        count: Some(total),
        more: (total > cap).then(|| {
            format!(
                "+{} {} \u{b7} \u{21e5} to see all",
                total - cap,
                kind.title().to_lowercase()
            )
        }),
        items,
        note,
    }
}

fn tab_of(kind: Kind) -> Tab {
    match kind {
        Kind::NeedsYou | Kind::Recent | Kind::Threads => Tab::Threads,
        Kind::Files => Tab::Files,
        Kind::Text => Tab::Text,
        Kind::Commands => Tab::Commands,
    }
}

fn done<T>(found: &Found<T>) -> bool {
    matches!(found, Found::Done(_))
}

/// What a file section says instead of (or besides) its rows.
fn note<T>(inputs: &Inputs<'_>, found: &Found<T>, no_thread: &str) -> Option<Note> {
    match found {
        Found::NoThread => Some(Note {
            text: no_thread.to_owned(),
            busy: false,
        }),
        Found::Pending => Some(Note {
            text: format!(
                "Searching {}\u{2026}",
                inputs.project.unwrap_or("the project")
            ),
            busy: true,
        }),
        Found::Failed(reason) => Some(Note {
            text: reason.clone(),
            busy: false,
        }),
        Found::Done(_) => None,
    }
}

/// Threads where every word of the query is in the title or the project; waiting ones first,
/// then the most recently updated.
fn matching_threads(threads: &[Thread], query: &str) -> Vec<Item> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut hits: Vec<&Thread> = threads
        .iter()
        .filter(|t| {
            let hay = format!("{} {}", t.title, t.project).to_lowercase();
            words.iter().all(|w| hay.contains(w))
        })
        .collect();
    hits.sort_by_key(|t| (t.badge.is_none(), std::cmp::Reverse(t.updated_ms)));
    hits.into_iter().cloned().map(Item::Thread).collect()
}

/// The daemon's file matches, re-ranked: a name holding the query first, then a path holding
/// it, then shorter paths.
fn matching_files(inputs: &Inputs<'_>, query: &str) -> Vec<Item> {
    let Found::Done(files) = inputs.files else {
        return vec![];
    };
    let needle: String = query.split_whitespace().collect::<String>().to_lowercase();
    let mut files: Vec<&String> = files.iter().collect();
    files.sort_by_key(|path| {
        let lower = path.to_lowercase();
        let name = lower.rsplit('/').next().unwrap_or(&lower).to_owned();
        (
            !name.contains(&needle),
            !lower.contains(&needle),
            path.len(),
        )
    });
    files.into_iter().cloned().map(Item::File).collect()
}

fn matching_lines(inputs: &Inputs<'_>) -> Vec<Item> {
    match inputs.lines {
        Found::Done(lines) => lines.iter().cloned().map(Item::Line).collect(),
        _ => vec![],
    }
}

/// The selection after ↑ or ↓ over `len` rows, wrapping.
pub(crate) fn step(selected: usize, len: usize, down: bool) -> usize {
    match len {
        0 => 0,
        _ if down => (selected + 1) % len,
        _ => (selected + len - 1) % len,
    }
}

/// The palette's threads, from the sidebar's folders. `updated` reads the run.
pub(crate) fn threads(folders: &[Folder], updated: impl Fn(&RunId) -> u64) -> Vec<Thread> {
    folders
        .iter()
        .flat_map(|folder| {
            folder.rows.iter().map(|row| Thread {
                run: row.run.clone(),
                title: crate::summary::parse_title(&row.title).what.to_owned(),
                project: folder.name.clone(),
                executor: row.executor.to_string(),
                badge: row.badge,
                dot: row.dot,
                age: row.age.clone(),
                updated_ms: updated(&row.run),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::projects::{FolderKey, ThreadRow};

    fn run(n: &str) -> RunId {
        let suffix: String = n.chars().filter(char::is_ascii_alphanumeric).collect();
        RunId::from_suffix(&suffix).unwrap()
    }

    fn thread(title: &str, project: &str, badge: Option<Badge>, updated_ms: u64) -> Thread {
        Thread {
            run: run(title),
            title: title.to_owned(),
            project: project.to_owned(),
            executor: "claude-code".to_owned(),
            badge,
            dot: if badge.is_some() {
                Dot::Waiting
            } else {
                Dot::Seen
            },
            age: "1h".to_owned(),
            updated_ms,
        }
    }

    /// The fake world's threads, as the sidebar lists them.
    fn world() -> Vec<Thread> {
        vec![
            thread(
                "pick the retry policy",
                "web-console",
                Some(Badge::NeedsInput),
                90,
            ),
            thread(
                "check upstream advisories",
                "web-console",
                Some(Badge::NeedsApproval),
                80,
            ),
            thread(
                "bump minor dependencies",
                "atlas-api",
                Some(Badge::NeedsApproval),
                70,
            ),
            thread("add the run inspector to the app shell", "reno", None, 60),
            thread("draft the scheduler lease design", "reno", None, 95),
            thread("fix the flaky pagination test", "atlas-api", None, 50),
            thread("review PR 214, cache headers", "web-console", None, 10),
            thread("tidy the changelog", "reno", None, 5),
        ]
    }

    fn on_home(threads: &[Thread]) -> Inputs<'_> {
        Inputs {
            threads,
            project: None,
            files: &Found::NoThread,
            lines: &Found::NoThread,
        }
    }

    fn titles(section: &Section) -> Vec<String> {
        section
            .items
            .iter()
            .map(|i| match i {
                Item::Thread(t) => t.title.clone(),
                Item::File(p) => p.clone(),
                Item::Line(l) => format!("{}:{}", l.path, l.line),
                Item::Command(e) => e.label.to_owned(),
            })
            .collect()
    }

    fn kinds(sheet: &Sheet) -> Vec<Kind> {
        sheet.sections.iter().map(|s| s.kind).collect()
    }

    // --- Empty ⌘K -------------------------------------------------------------------------

    #[test]
    fn empty_palette_leads_with_the_threads_that_need_you() {
        let all = world();
        let s = sheet(&on_home(&all), "", Tab::All);
        assert_eq!(kinds(&s), [Kind::NeedsYou, Kind::Recent, Kind::Commands]);
        let needs = &s.sections[0];
        assert_eq!(needs.finding.as_deref(), Some("3 threads need you"));
        assert_eq!(needs.count, None, "the finding already says how many");
        assert_eq!(
            titles(needs),
            [
                "pick the retry policy",
                "check upstream advisories",
                "bump minor dependencies"
            ],
            "most recently updated first"
        );
        let first = s.items()[0];
        assert!(
            matches!(first, Item::Thread(t) if t.title == "pick the retry policy"),
            "↵ on an empty palette opens the first waiting thread"
        );
    }

    #[test]
    fn one_waiting_thread_is_worded_in_the_singular() {
        let all = vec![thread(
            "pick the retry policy",
            "web-console",
            Some(Badge::NeedsInput),
            1,
        )];
        let s = sheet(&on_home(&all), "", Tab::All);
        assert_eq!(s.sections[0].finding.as_deref(), Some("1 thread needs you"));
    }

    #[test]
    fn nothing_waiting_means_no_needs_you_section() {
        let all: Vec<Thread> = world().into_iter().filter(|t| t.badge.is_none()).collect();
        let s = sheet(&on_home(&all), "", Tab::All);
        assert_eq!(kinds(&s), [Kind::Recent, Kind::Commands]);
    }

    #[test]
    fn recent_lists_five_threads_and_commands_three_with_the_rest_counted() {
        let all = world();
        let s = sheet(&on_home(&all), "", Tab::All);
        let recent = &s.sections[1];
        assert_eq!(
            titles(recent),
            [
                "draft the scheduler lease design",
                "add the run inspector to the app shell",
                "fix the flaky pagination test",
                "review PR 214, cache headers",
                "tidy the changelog"
            ]
        );
        assert_eq!(recent.count, Some(5));
        let commands = &s.sections[2];
        assert_eq!(commands.items.len(), 3);
        assert_eq!(commands.count, Some(7));
        assert_eq!(commands.more.as_deref(), Some("+4 commands · ⇥ to see all"));
    }

    #[test]
    fn empty_palette_counts_only_the_threads_tab() {
        let all = world();
        let s = sheet(&on_home(&all), "", Tab::All);
        let counts: Vec<(Tab, Option<usize>)> = s.tabs.iter().map(|t| (t.tab, t.count)).collect();
        assert_eq!(
            counts,
            [
                (Tab::All, None),
                (Tab::Threads, Some(8)),
                (Tab::Files, None),
                (Tab::Text, None),
                (Tab::Commands, None)
            ]
        );
        assert!(s.tabs[0].active);
    }

    // --- Typing ---------------------------------------------------------------------------

    fn in_web_console<'a>(
        all: &'a [Thread],
        files: &'a Found<String>,
        lines: &'a Found<Line>,
    ) -> Inputs<'a> {
        Inputs {
            threads: all,
            project: Some("web-console"),
            files,
            lines,
        }
    }

    fn cache_files() -> Found<String> {
        Found::Done(
            [
                "src/cache/eviction.rs",
                "tests/cache_headers.rs",
                "src/cache.rs",
                "docs/caching.md",
            ]
            .map(str::to_owned)
            .to_vec(),
        )
    }

    fn cache_lines(n: u32) -> Found<Line> {
        Found::Done(
            (1..=n)
                .map(|i| Line {
                    path: "src/cache.rs".to_owned(),
                    line: i,
                    text: format!("    // cache line {i}"),
                })
                .collect(),
        )
    }

    #[test]
    fn typing_shows_threads_files_lines_and_commands_four_each() {
        let all = world();
        let (files, lines) = (cache_files(), cache_lines(18));
        let s = sheet(&in_web_console(&all, &files, &lines), "cache", Tab::All);
        assert_eq!(kinds(&s), [Kind::Threads, Kind::Files, Kind::Text]);
        assert_eq!(titles(&s.sections[0]), ["review PR 214, cache headers"]);
        assert_eq!(s.sections[2].items.len(), 4);
        assert_eq!(s.sections[2].count, Some(18));
        assert_eq!(
            s.sections[2].more.as_deref(),
            Some("+14 in files · ⇥ to see all")
        );
        assert!(s.sections.iter().all(|x| x.head));
        assert_eq!(s.nothing, None);
    }

    #[test]
    fn files_whose_name_holds_the_query_come_first_then_shorter_paths() {
        let all = world();
        let (files, lines) = (cache_files(), cache_lines(1));
        let s = sheet(&in_web_console(&all, &files, &lines), "cache", Tab::All);
        assert_eq!(
            titles(&s.sections[1]),
            [
                "src/cache.rs",
                "tests/cache_headers.rs",
                "src/cache/eviction.rs",
                "docs/caching.md"
            ]
        );
    }

    #[test]
    fn a_thread_matches_every_word_in_its_title_or_project_and_waiting_ones_lead() {
        let all = world();
        let s = sheet(&on_home(&all), "web console", Tab::Threads);
        assert_eq!(
            titles(&s.sections[0]),
            [
                "pick the retry policy",
                "check upstream advisories",
                "review PR 214, cache headers"
            ]
        );
        let s = sheet(&on_home(&all), "RETRY pick", Tab::Threads);
        assert_eq!(titles(&s.sections[0]), ["pick the retry policy"]);
    }

    #[test]
    fn typing_counts_every_tab() {
        let all = world();
        let (files, lines) = (cache_files(), cache_lines(18));
        let s = sheet(&in_web_console(&all, &files, &lines), "cache", Tab::All);
        let counts: Vec<Option<usize>> = s.tabs.iter().map(|t| t.count).collect();
        assert_eq!(counts, [None, Some(1), Some(4), Some(18), Some(0)]);
    }

    #[test]
    fn one_tab_lists_only_its_kind_in_full_without_heads() {
        let all = world();
        let (files, lines) = (cache_files(), cache_lines(18));
        let s = sheet(&in_web_console(&all, &files, &lines), "cache", Tab::Text);
        assert_eq!(kinds(&s), [Kind::Text]);
        assert_eq!(s.sections[0].items.len(), 18);
        assert!(!s.sections[0].head);
        assert_eq!(s.sections[0].more, None);
        assert!(s.tabs[3].active);
    }

    #[test]
    fn a_leading_sign_picks_the_tab() {
        assert_eq!(scope(">sett", Tab::All), (Tab::Commands, "sett"));
        assert_eq!(scope("/cache", Tab::Threads), (Tab::Files, "cache"));
        assert_eq!(scope("#MAX_AGE", Tab::All), (Tab::Text, "MAX_AGE"));
        assert_eq!(scope("cache", Tab::Text), (Tab::Text, "cache"));
        let all = world();
        let s = sheet(&on_home(&all), ">sett", Tab::All);
        assert_eq!(s.tab, Tab::Commands);
        assert_eq!(titles(&s.sections[0]), ["Settings"]);
    }

    // --- File search states -------------------------------------------------------------

    #[test]
    fn with_no_thread_open_the_file_sections_say_how_to_search() {
        let all = world();
        let s = sheet(&on_home(&all), "cache", Tab::All);
        let files = s.sections.iter().find(|x| x.kind == Kind::Files).unwrap();
        assert_eq!(
            files.note.as_ref().map(|n| n.text.as_str()),
            Some("Open a thread to search its project's files")
        );
        let text = s.sections.iter().find(|x| x.kind == Kind::Text).unwrap();
        assert_eq!(
            text.note.as_ref().map(|n| n.text.as_str()),
            Some("Open a thread to search inside its files")
        );
        let counts: Vec<Option<usize>> = s.tabs.iter().map(|t| t.count).collect();
        assert_eq!(counts[2..4], [None, None], "no count without a project");
    }

    #[test]
    fn a_search_in_flight_says_so_with_a_spinner() {
        let all = world();
        let s = sheet(
            &in_web_console(&all, &Found::Pending, &Found::Pending),
            "cache",
            Tab::Files,
        );
        assert_eq!(
            s.sections[0].note,
            Some(Note {
                text: "Searching web-console…".to_owned(),
                busy: true
            })
        );
        assert_eq!(s.tabs[2].count, None);
        assert_eq!(s.nothing, None, "not nothing yet");
    }

    #[test]
    fn a_refused_search_says_why() {
        let all = world();
        let failed = Found::Failed("This thread's worktree was removed".to_owned());
        let failed_lines = Found::Failed("This thread's worktree was removed".to_owned());
        let s = sheet(
            &in_web_console(&all, &failed, &failed_lines),
            "cache",
            Tab::Files,
        );
        assert_eq!(
            s.sections[0].note.as_ref().map(|n| n.text.as_str()),
            Some("This thread's worktree was removed")
        );
    }

    #[test]
    fn nothing_matches_names_the_query() {
        let all = world();
        let none = Found::Done(vec![]);
        let lines = Found::Done(vec![]);
        let s = sheet(&in_web_console(&all, &none, &lines), "qzx", Tab::All);
        assert!(s.sections.is_empty());
        assert_eq!(s.nothing.as_deref(), Some("Nothing matches “qzx”"));
    }

    // --- Rows and keys ------------------------------------------------------------------

    #[test]
    fn the_match_is_found_regardless_of_case() {
        assert_eq!(
            mark("use crate::headers::CachePolicy;", "cache"),
            Some(20..25)
        );
        assert_eq!(mark("Settings", "SETT"), Some(0..4));
        assert_eq!(mark("review PR 214", "zzz"), None);
        assert_eq!(mark("anything", "  "), None);
    }

    #[test]
    fn arrows_wrap_and_tabs_cycle() {
        assert_eq!(step(0, 3, true), 1);
        assert_eq!(step(2, 3, true), 0);
        assert_eq!(step(0, 3, false), 2);
        assert_eq!(step(0, 0, true), 0);
        assert_eq!(Tab::All.step(true), Tab::Threads);
        assert_eq!(Tab::Commands.step(true), Tab::All);
        assert_eq!(Tab::All.step(false), Tab::Commands);
        assert_eq!(Tab::from_digit(3), Some(Tab::Files));
        assert_eq!(Tab::from_digit(6), None);
    }

    #[test]
    fn threads_come_from_the_sidebar_rows_with_their_project() {
        let row = ThreadRow {
            thread: None,
            run: run("scout1"),
            members: vec![run("scout1")],
            title: "scout · web-console: check upstream advisories".to_owned(),
            executor: "codex".parse().unwrap(),
            dot: Dot::Waiting,
            badge: Some(Badge::NeedsApproval),
            age: "3h".to_owned(),
            project: "web-console".to_owned(),
        };
        let folders = [Folder {
            key: FolderKey::Repo("/demo/web-console".into()),
            name: "web-console".to_owned(),
            open: true,
            waiting: 1,
            rows: vec![row],
            hidden: 0,
            expanded: false,
        }];
        let out = threads(&folders, |_| 42);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "check upstream advisories");
        assert_eq!(out[0].project, "web-console");
        assert_eq!(out[0].executor, "codex");
        assert_eq!(out[0].updated_ms, 42);
    }
}
