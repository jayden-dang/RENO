//! ROAD-31, the composer draft: everything the three variants share and none of them draws.
//! Which variant is showing, which `@` `/` `$` token the caret is in, the entries a trigger
//! offers (grouped, filtered, capped), the tokens that read as coloured words in the text, and
//! the sentences that state what the composer found ("Runs in reno on main, in a new worktree",
//! "2 messages wait for this turn to end"). The variants differ only in how they draw these.

use std::ops::Range;

use reno_client::completions::{CommandKind, CommandsError, HarnessCommand, ProjectFile};
use reno_client::places::Checkout;

// ---- the switch ----------------------------------------------------------------------------

/// The three directions under review.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Variant {
    /// Popovers: every choice is a small popover next to what it changes.
    #[default]
    A,
    /// Command sheet: one place to look before you type.
    B,
    /// Sentence: the place reads as a sentence, the message's parts as coloured words.
    C,
}

impl Variant {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::A => "Popovers",
            Self::B => "Command sheet",
            Self::C => "Sentence",
        }
    }

    /// "A \u{2014} Popovers".
    pub(crate) fn label(self) -> String {
        format!("{} \u{2014} {}", self.key(), self.name())
    }

    pub(crate) fn next(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::C,
            Self::C => Self::A,
        }
    }

    pub(crate) fn prev(self) -> Self {
        match self {
            Self::A => Self::C,
            Self::B => Self::A,
            Self::C => Self::B,
        }
    }

    /// `RENO_DRAFT31=a|b|c`, either case.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "a" => Some(Self::A),
            "b" => Some(Self::B),
            "c" => Some(Self::C),
            _ => None,
        }
    }
}

// ---- the trigger under the caret -----------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// `@`: a file.
    At,
    /// `/`: a command.
    Slash,
    /// `$`: a skill.
    Dollar,
}

impl Trigger {
    pub(crate) const ALL: [Self; 3] = [Self::At, Self::Slash, Self::Dollar];

    pub(crate) fn glyph(self) -> char {
        match self {
            Self::At => '@',
            Self::Slash => '/',
            Self::Dollar => '$',
        }
    }

    /// The command sheet's tab.
    pub(crate) fn tab(self) -> &'static str {
        match self {
            Self::At => "Files",
            Self::Slash => "Commands",
            Self::Dollar => "Skills",
        }
    }

    fn of(c: char) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.glyph() == c)
    }
}

/// The token the caret is in, when it starts with a trigger at a place a trigger counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Active {
    pub(crate) trigger: Trigger,
    /// Byte offset of the trigger character.
    pub(crate) start: usize,
    /// Byte offset just after the token (the caret may be inside it).
    pub(crate) end: usize,
    /// What is typed after the trigger, up to the caret.
    pub(crate) query: String,
}

/// A query this long is a path, not a search.
const QUERY_MAX: usize = 80;

/// A `@` or `$` counts at the start of the text or after whitespace; a `/` only at the start of a
/// line, so `src/main.rs` and "and/or" never open a list.
pub(crate) fn find_active(text: &str, caret: usize) -> Option<Active> {
    if caret > text.len() || !text.is_char_boundary(caret) {
        return None;
    }
    let before = &text[..caret];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(at, c)| at + c.len_utf8());
    let token = &before[start..];
    let trigger = Trigger::of(token.chars().next()?)?;
    if trigger == Trigger::Slash && start != 0 && !text[..start].ends_with('\n') {
        return None;
    }
    let query = &token[1..];
    if query.chars().count() > QUERY_MAX {
        return None;
    }
    let end = caret
        + text[caret..]
            .find(char::is_whitespace)
            .unwrap_or(text.len() - caret);
    Some(Active {
        trigger,
        start,
        end,
        query: query.to_owned(),
    })
}

/// Whether `trigger` may stand at `start` of `text` (for a tab that changes the trigger).
pub(crate) fn trigger_fits(text: &str, start: usize, trigger: Trigger) -> bool {
    trigger != Trigger::Slash || start == 0 || text[..start].ends_with('\n')
}

// ---- the entries ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    /// A command of this app: `/model`, `/new`.
    App,
    /// A command of the harness: `/compact`.
    Harness,
    Skill,
}

/// One row the list offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) kind: EntryKind,
    /// What the row says: `src/main.rs`, `/model`, `$changelog`.
    pub(crate) label: String,
    /// The muted line: a command's description, a file's folder.
    pub(crate) detail: String,
    /// The group header: "Files", "Claude Code", "Reno", "Skills".
    pub(crate) group: String,
    /// What goes into the text, trigger included and a space after.
    pub(crate) insert: String,
    /// A file's first lines, for the sheet's preview.
    pub(crate) head: Vec<String>,
    /// How a command is used, when it takes an argument.
    pub(crate) usage: Option<String>,
    /// "from Claude Code", "from Reno", "in your project".
    pub(crate) source: String,
}

/// What a trigger shows now: rows, and why there are none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Listing {
    pub(crate) entries: Vec<Entry>,
    /// The words when a source could not be read ("Claude Code's commands could not be read").
    pub(crate) failure: Option<String>,
    /// Still asking (a live run's file search).
    pub(crate) pending: bool,
}

/// The app's own commands: real, handled here, never sent to the harness.
pub(crate) const APP_COMMANDS: &[(&str, &str)] = &[
    ("model", "Choose the harness and model"),
    ("new", "Start a new thread"),
    ("resume", "Go back to a recent thread"),
    ("settings", "Open Settings"),
    ("diff", "Show this run's changes"),
    ("files", "Browse this run's files"),
    ("terminal", "Open a terminal in this run"),
    ("rename", "Rename this thread"),
    ("stop", "Stop the run"),
];

/// `query` matches `name`: a prefix first, then anywhere in it.
fn name_rank(name: &str, query: &str) -> Option<u8> {
    let (name, query) = (name.to_lowercase(), query.to_lowercase());
    if name.starts_with(&query) {
        Some(0)
    } else {
        name.contains(&query).then_some(1)
    }
}

fn ranked<T>(items: impl IntoIterator<Item = T>, name: impl Fn(&T) -> &str, query: &str) -> Vec<T> {
    let mut hits: Vec<(u8, T)> = items
        .into_iter()
        .filter_map(|item| name_rank(name(&item), query).map(|rank| (rank, item)))
        .collect();
    hits.sort_by_key(|(rank, _)| *rank);
    hits.into_iter().map(|(_, item)| item).collect()
}

fn app_entry(name: &str, description: &str) -> Entry {
    Entry {
        kind: EntryKind::App,
        label: format!("/{name}"),
        detail: description.to_owned(),
        group: "Reno".to_owned(),
        insert: format!("/{name} "),
        head: Vec::new(),
        usage: None,
        source: "from Reno".to_owned(),
    }
}

fn harness_entry(command: &HarnessCommand, harness: &str) -> Entry {
    let skill = command.kind == CommandKind::Skill;
    let glyph = if skill { '$' } else { '/' };
    Entry {
        kind: if skill {
            EntryKind::Skill
        } else {
            EntryKind::Harness
        },
        label: format!("{glyph}{}", command.name),
        detail: command.description.clone(),
        group: if skill { "Skills" } else { harness }.to_owned(),
        insert: format!("{glyph}{} ", command.name),
        head: Vec::new(),
        usage: command.usage.clone(),
        source: format!("from {harness}"),
    }
}

/// `/`: the harness's commands (when this connection has them), then the app's.
/// `harness` is `None` when the connection has no such contract.
pub(crate) fn command_listing(
    query: &str,
    harness_name: &str,
    harness: Option<&Result<Vec<HarnessCommand>, CommandsError>>,
) -> Listing {
    let mut listing = Listing::default();
    match harness {
        Some(Ok(all)) => {
            let commands = all.iter().filter(|c| c.kind == CommandKind::Command);
            listing.entries.extend(
                ranked(commands, |c| c.name.as_str(), query)
                    .into_iter()
                    .map(|c| harness_entry(c, harness_name)),
            );
        }
        Some(Err(CommandsError::Unreadable)) => {
            listing.failure = Some(format!("{harness_name}'s commands could not be read"));
        }
        None => {}
    }
    listing.entries.extend(
        ranked(APP_COMMANDS.iter(), |(name, _)| name, query)
            .into_iter()
            .map(|(name, description)| app_entry(name, description)),
    );
    listing
}

/// `$`: the harness's skills. Without the contract there are none to offer.
pub(crate) fn skill_listing(
    query: &str,
    harness_name: &str,
    harness: Option<&Result<Vec<HarnessCommand>, CommandsError>>,
) -> Listing {
    let mut listing = Listing::default();
    match harness {
        Some(Ok(all)) => {
            let skills = all.iter().filter(|c| c.kind == CommandKind::Skill);
            listing.entries.extend(
                ranked(skills, |c| c.name.as_str(), query)
                    .into_iter()
                    .map(|c| harness_entry(c, harness_name)),
            );
        }
        Some(Err(CommandsError::Unreadable)) => {
            listing.failure = Some(format!("{harness_name}'s skills could not be read"));
        }
        None => {}
    }
    listing
}

/// `@`: files, as the daemon found them. A path's folder is its muted line.
pub(crate) fn file_entries(
    files: impl IntoIterator<Item = ProjectFile>,
    source: &str,
) -> Vec<Entry> {
    files
        .into_iter()
        .map(|file| {
            let folder = file
                .path
                .rsplit_once('/')
                .map_or_else(|| "project root".to_owned(), |(dir, _)| dir.to_owned());
            Entry {
                kind: EntryKind::File,
                label: file.path.clone(),
                detail: folder,
                group: "Files".to_owned(),
                insert: format!("@{} ", file.path),
                head: file.head,
                usage: None,
                source: source.to_owned(),
            }
        })
        .collect()
}

/// How many rows a list shows at once, and where its window starts so `selected` is in it.
pub(crate) fn window_start(selected: usize, top: usize, cap: usize) -> usize {
    if selected < top {
        selected
    } else if selected >= top + cap {
        selected + 1 - cap
    } else {
        top
    }
}

// ---- tokens read as words ------------------------------------------------------------------

/// What kind of word a finished token is, for colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenKind {
    File,
    Command,
    Skill,
}

/// The `@file`, `/command` and `$skill` words of `text`, found by scanning, never stored (an undo
/// cannot leave one behind). `except` is the token being typed: it is not coloured yet.
pub(crate) fn tokens(text: &str, except: Option<&Range<usize>>) -> Vec<(Range<usize>, TokenKind)> {
    let mut found = Vec::new();
    let mut at = 0;
    for word in text.split_inclusive(char::is_whitespace) {
        let body = word.trim_end();
        let range = at..at + body.len();
        at += word.len();
        if body.chars().count() < 2 || except.is_some_and(|e| e.start == range.start) {
            continue;
        }
        let kind = match body.chars().next().and_then(Trigger::of) {
            Some(Trigger::At) => TokenKind::File,
            Some(Trigger::Dollar) => TokenKind::Skill,
            Some(Trigger::Slash) if range.start == 0 || text[..range.start].ends_with('\n') => {
                TokenKind::Command
            }
            _ => continue,
        };
        found.push((range, kind));
    }
    found
}

// ---- where a task runs ---------------------------------------------------------------------

/// "New worktree", "This checkout", or the worktree's own name.
pub(crate) fn checkout_label(checkout: &Checkout) -> String {
    match checkout {
        Checkout::NewWorktree => "New worktree".to_owned(),
        Checkout::ThisCheckout => "This checkout".to_owned(),
        Checkout::Worktree(name) => name.clone(),
    }
}

/// "in a new worktree", "in this checkout", "in the worktree reno-model-chip".
pub(crate) fn checkout_phrase(checkout: &Checkout) -> String {
    match checkout {
        Checkout::NewWorktree => "in a new worktree".to_owned(),
        Checkout::ThisCheckout => "in this checkout".to_owned(),
        Checkout::Worktree(name) => format!("in the worktree {name}"),
    }
}

/// The finding for a new thread: "Runs in reno on main, in a new worktree".
pub(crate) fn place_finding(project: &str, branch: &str, checkout: &Checkout) -> String {
    format!(
        "Runs in {project} on {branch}, {}",
        checkout_phrase(checkout)
    )
}

/// The branch list's foot: "Showing 20 of 34", or the count when all are shown.
pub(crate) fn branches_foot(shown: usize, total: usize) -> String {
    if shown >= total {
        match total {
            1 => "1 branch".to_owned(),
            n => format!("{n} branches"),
        }
    } else {
        format!("Showing {shown} of {total}")
    }
}

// ---- the queue -----------------------------------------------------------------------------

/// The finding on a run with a queue; `None` when nothing waits.
pub(crate) fn queue_finding(waiting: usize) -> Option<String> {
    match waiting {
        0 => None,
        1 => Some("1 message waits for this turn to end".to_owned()),
        n => Some(format!("{n} messages wait for this turn to end")),
    }
}

/// "1 queued", "2 queued".
pub(crate) fn queue_chip(waiting: usize) -> String {
    format!("{waiting} queued")
}

/// Where a dragged message lands: the place of the row it is dropped on.
pub(crate) fn drop_place(len: usize, target: usize) -> usize {
    target.min(len.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active(text: &str) -> Option<Active> {
        find_active(text, text.len())
    }

    #[test]
    fn the_variants_cycle_both_ways_and_read_as_key_dash_name() {
        assert_eq!(Variant::A.next(), Variant::B);
        assert_eq!(Variant::C.next(), Variant::A);
        assert_eq!(Variant::A.prev(), Variant::C);
        assert_eq!(Variant::B.label(), "B \u{2014} Command sheet");
        assert_eq!(Variant::parse(" C "), Some(Variant::C));
        assert_eq!(Variant::parse("d"), None);
    }

    #[test]
    fn a_trigger_opens_only_at_a_token_start() {
        let a = active("look at @src/ma").unwrap();
        assert_eq!((a.trigger, a.start, a.end), (Trigger::At, 8, 15));
        assert_eq!(a.query, "src/ma");
        assert_eq!(active("@").unwrap().query, "");
        assert_eq!(active("use $chan").unwrap().trigger, Trigger::Dollar);
        // Mid-word triggers and paths never open anything.
        assert_eq!(active("mail me@host"), None);
        assert_eq!(active("src/main.rs"), None);
        assert_eq!(active("and/or"), None);
        assert_eq!(active("plain text"), None);
        assert_eq!(active(""), None);
    }

    #[test]
    fn a_slash_is_a_command_only_at_the_start_of_a_line() {
        assert_eq!(active("/co").unwrap().trigger, Trigger::Slash);
        assert_eq!(active("hello\n/co").unwrap().trigger, Trigger::Slash);
        assert_eq!(active("hello /co"), None);
        // `@src/` is a file query, not a command.
        let file = active("@src/").unwrap();
        assert_eq!((file.trigger, file.query.as_str()), (Trigger::At, "src/"));
        assert!(trigger_fits("hello @x", 6, Trigger::At));
        assert!(!trigger_fits("hello @x", 6, Trigger::Slash));
        assert!(trigger_fits("@x", 0, Trigger::Slash));
    }

    #[test]
    fn the_caret_inside_a_token_replaces_the_whole_token() {
        let a = find_active("see @src/main.rs now", 8).unwrap();
        assert_eq!((a.start, a.end), (4, 15));
        assert_eq!(a.query, "src");
        // After the space the caret is in no token.
        assert_eq!(find_active("see @src/main.rs now", 16), None);
        assert_eq!(find_active("\u{e9}@x", 1), None);
    }

    fn commands() -> Vec<HarnessCommand> {
        let command = |name: &str, kind| HarnessCommand {
            name: name.to_owned(),
            kind,
            description: format!("does {name}"),
            usage: None,
        };
        vec![
            command("compact", CommandKind::Command),
            command("review", CommandKind::Command),
            command("changelog", CommandKind::Skill),
        ]
    }

    #[test]
    fn slash_lists_harness_commands_then_the_apps_and_never_skills() {
        let all = Ok(commands());
        let listing = command_listing("", "Claude Code", Some(&all));
        let labels: Vec<&str> = listing.entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(&labels[..3], ["/compact", "/review", "/model"]);
        assert_eq!(labels.len(), 2 + APP_COMMANDS.len());
        assert_eq!(listing.entries[0].group, "Claude Code");
        assert_eq!(listing.entries[2].group, "Reno");
        assert_eq!(listing.entries[2].insert, "/model ");
        assert_eq!(listing.failure, None);
    }

    #[test]
    fn a_query_ranks_prefixes_before_substrings() {
        let all = Ok(commands());
        let listing = command_listing("re", "Claude Code", Some(&all));
        let labels: Vec<&str> = listing.entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["/review", "/resume", "/rename"]);
        let none = command_listing("zzz", "Claude Code", Some(&all));
        assert!(none.entries.is_empty());
    }

    #[test]
    fn unreadable_harness_commands_say_so_and_the_apps_still_work() {
        let failed = Err(CommandsError::Unreadable);
        let listing = command_listing("", "Claude Code", Some(&failed));
        assert_eq!(
            listing.failure.as_deref(),
            Some("Claude Code's commands could not be read")
        );
        assert_eq!(listing.entries.len(), APP_COMMANDS.len());
        assert!(listing.entries.iter().all(|e| e.kind == EntryKind::App));
    }

    #[test]
    fn without_the_contract_slash_offers_the_apps_commands_and_dollar_nothing() {
        let listing = command_listing("", "Claude Code", None);
        assert_eq!(listing.entries.len(), APP_COMMANDS.len());
        assert_eq!(listing.failure, None);
        let skills = skill_listing("", "Claude Code", None);
        assert!(skills.entries.is_empty() && skills.failure.is_none());
    }

    #[test]
    fn dollar_lists_only_skills() {
        let all = Ok(commands());
        let listing = skill_listing("", "Claude Code", Some(&all));
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].label, "$changelog");
        assert_eq!(listing.entries[0].group, "Skills");
        assert_eq!(listing.entries[0].insert, "$changelog ");
    }

    #[test]
    fn a_file_row_inserts_its_path_and_shows_its_folder() {
        let file = ProjectFile {
            path: "src/ui/kit.rs".to_owned(),
            head: vec!["//! kit".to_owned()],
        };
        let entries = file_entries([file], "in reno");
        assert_eq!(entries[0].insert, "@src/ui/kit.rs ");
        assert_eq!(entries[0].detail, "src/ui");
        assert_eq!(entries[0].head, ["//! kit"]);
        let root = file_entries(
            [ProjectFile {
                path: "Cargo.toml".to_owned(),
                head: Vec::new(),
            }],
            "in reno",
        );
        assert_eq!(root[0].detail, "project root");
    }

    #[test]
    fn the_window_follows_the_selection() {
        assert_eq!(window_start(0, 0, 8), 0);
        assert_eq!(window_start(7, 0, 8), 0);
        assert_eq!(window_start(8, 0, 8), 1);
        assert_eq!(window_start(2, 5, 8), 2);
        assert_eq!(window_start(9, 5, 8), 5);
    }

    #[test]
    fn finished_tokens_are_found_by_scanning_and_the_one_being_typed_is_left_out() {
        let text = "/review @src/main.rs and $changelog then @sr";
        let found = tokens(text, None);
        let words: Vec<(&str, TokenKind)> =
            found.iter().map(|(r, k)| (&text[r.clone()], *k)).collect();
        assert_eq!(
            words,
            [
                ("/review", TokenKind::Command),
                ("@src/main.rs", TokenKind::File),
                ("$changelog", TokenKind::Skill),
                ("@sr", TokenKind::File),
            ]
        );
        let typing = tokens(text, Some(&(41..44)));
        assert_eq!(typing.len(), 3);
        // A slash mid-line and a lone trigger are plain text.
        assert!(tokens("and /not @ $", None).is_empty());
    }

    #[test]
    fn the_findings_are_sentences() {
        assert_eq!(
            place_finding("reno", "main", &Checkout::NewWorktree),
            "Runs in reno on main, in a new worktree"
        );
        assert_eq!(
            place_finding("reno", "main", &Checkout::ThisCheckout),
            "Runs in reno on main, in this checkout"
        );
        assert_eq!(
            place_finding(
                "reno",
                "fix/model-chip",
                &Checkout::Worktree("reno-model-chip".into())
            ),
            "Runs in reno on fix/model-chip, in the worktree reno-model-chip"
        );
        assert_eq!(queue_finding(0), None);
        assert_eq!(
            queue_finding(1).as_deref(),
            Some("1 message waits for this turn to end")
        );
        assert_eq!(
            queue_finding(2).as_deref(),
            Some("2 messages wait for this turn to end")
        );
        assert_eq!(queue_chip(2), "2 queued");
    }

    #[test]
    fn the_branch_foot_counts_what_is_shown_and_what_there_is() {
        assert_eq!(branches_foot(20, 34), "Showing 20 of 34");
        assert_eq!(branches_foot(7, 7), "7 branches");
        assert_eq!(branches_foot(1, 1), "1 branch");
    }

    #[test]
    fn a_drop_never_lands_past_the_last_place() {
        assert_eq!(drop_place(3, 1), 1);
        assert_eq!(drop_place(3, 9), 2);
        assert_eq!(drop_place(0, 0), 0);
    }
}
