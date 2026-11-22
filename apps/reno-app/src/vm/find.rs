//! Find in the conversation: a search over the rows' source text, and what a hit needs opened.
//!
//! The search reads data, not paint, so it reaches text nothing shows yet: prompts, replies,
//! interim prose, reasoning, tool commands and tool output, inside closed folds too. A hit is a
//! row's [`Key`], which of the row's texts it is in ([`Part`]) and a byte range in that text.
//! Ranges are on character boundaries, so a later selection or comment can anchor to them.
//!
//! [`Opens`] keeps which rows and outputs are open and who opened them, so that closing the bar
//! can close again exactly what search opened. Pure: no window, no clock.

use std::collections::HashSet;
use std::hash::Hash;
use std::ops::Range;

use regex::{Regex, RegexBuilder};

use crate::transcript::{Key, Row, ToolEntry};
use crate::vm::changes::{self as facts, OUTPUT_LINES};
use crate::vm::convo;

/// Which of a row's texts a hit is in. Most rows have one; a tool group has a command and an
/// output for each call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Part {
    /// The row's text (prompt, reply, reasoning).
    Whole,
    /// The command, path or URL of the group's `n`th call.
    Label(usize),
    /// What the group's `n`th call printed.
    Output(usize),
}

/// What kind of text a hit is in, for the results list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Prompt,
    Reply,
    Reasoning,
    Command,
    Output,
}

/// One match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hit {
    pub(crate) key: Key,
    pub(crate) part: Part,
    /// Bytes of that text.
    pub(crate) range: Range<usize>,
    /// The "Worked for" fold the row is in.
    pub(crate) fold: Option<Key>,
    /// The row's place in reading order (folds opened out): what puts hits in order.
    pub(crate) ordinal: usize,
    /// The turn it is in, from 1: the number of prompts up to it.
    pub(crate) turn: usize,
    pub(crate) source: Source,
}

impl Hit {
    /// Reading order: row, then text, then place in it.
    fn order(&self) -> (usize, Part, usize) {
        (self.ordinal, self.part, self.range.start)
    }

    /// The same match, found again after the rows changed.
    fn same_as(&self, other: &Self) -> bool {
        self.key == other.key && self.part == other.part && self.range.start == other.range.start
    }
}

/// What the strip asks for: the text, and the three options that change what it means.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct Query {
    pub(crate) text: String,
    /// Match case: `Cat` is not `cat`.
    pub(crate) case: bool,
    /// Whole word: `cat` is not in `concat`.
    pub(crate) word: bool,
    /// The text is a pattern, not the words to look for.
    pub(crate) regex: bool,
}

/// The most a compiled pattern may weigh, so a pasted pattern cannot cost the app its memory.
const PATTERN_LIMIT: usize = 1 << 20;

/// A [`Query`] ready to search with. Matching takes time linear in the text, whatever the
/// pattern. An empty query matches nothing.
#[derive(Clone, Debug, Default)]
pub(crate) struct Matcher {
    re: Option<Regex>,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Query {
    /// Compiles the query, or says in one line why it cannot be: a pattern that does not parse,
    /// or one over [`PATTERN_LIMIT`].
    pub(crate) fn compile(&self) -> Result<Matcher, String> {
        if self.text.is_empty() {
            return Ok(Matcher { re: None });
        }
        let pattern = if self.regex {
            // The wrapping group would close a stray paren and let `a)|(b` parse: the text must
            // parse as it is.
            RegexBuilder::new(&self.text)
                .unicode(true)
                .size_limit(PATTERN_LIMIT)
                .build()
                .map_err(|err| reason(&err))?;
            let body = format!("(?:{})", self.text);
            if self.word {
                format!("\\b{body}\\b")
            } else {
                body
            }
        } else {
            // A boundary only where the text ends in a word character: `.rs` has none to find
            // at its dot.
            let (first, last) = (self.text.chars().next(), self.text.chars().next_back());
            let edge = |c: Option<char>| {
                if self.word && c.is_some_and(is_word) {
                    "\\b"
                } else {
                    ""
                }
            };
            format!("{}{}{}", edge(first), regex::escape(&self.text), edge(last))
        };
        RegexBuilder::new(&pattern)
            .unicode(true)
            .case_insensitive(!self.case)
            .size_limit(PATTERN_LIMIT)
            .build()
            .map(|re| Matcher { re: Some(re) })
            .map_err(|err| reason(&err))
    }
}

/// The part of a build error worth showing in a strip: one line.
fn reason(err: &regex::Error) -> String {
    match err {
        regex::Error::CompiledTooBig(_) => "the pattern is too large".to_owned(),
        regex::Error::Syntax(text) => text
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix("error: "))
            .unwrap_or("the pattern does not parse")
            .to_owned(),
        other => other
            .to_string()
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
    }
}

impl Matcher {
    /// Where the query is in `hay`, left to right and not overlapping. Ranges are bytes of
    /// `hay` and fall on character boundaries; a match with nothing in it is not one.
    pub(crate) fn matches(&self, hay: &str) -> Vec<Range<usize>> {
        self.matches_up_to(hay, usize::MAX)
    }

    /// [`Self::matches`], stopping at `limit` of them.
    fn matches_up_to(&self, hay: &str, limit: usize) -> Vec<Range<usize>> {
        let Some(re) = &self.re else {
            return Vec::new();
        };
        re.find_iter(hay)
            .filter(|m| !m.is_empty())
            .map(|m| m.range())
            .take(limit)
            .collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.re.is_none()
    }
}

/// The searchable texts of a row, with what each is.
fn texts(row: &Row) -> Vec<(Part, Source, &str)> {
    match row {
        Row::User { text, .. } => vec![(Part::Whole, Source::Prompt, text)],
        Row::Prose { text, .. } => vec![(Part::Whole, Source::Reply, text)],
        Row::Reasoning { text, .. } => vec![(Part::Whole, Source::Reasoning, text)],
        Row::Tools { calls, .. } => calls
            .iter()
            .enumerate()
            .flat_map(|(n, call)| {
                let out = call
                    .output
                    .as_ref()
                    .map(|o| (Part::Output(n), Source::Output, o.text.as_str()));
                [
                    Some((Part::Label(n), Source::Command, call.label.as_str())),
                    out,
                ]
            })
            .flatten()
            .collect(),
        _ => Vec::new(),
    }
}

/// The text a hit is in.
pub(crate) fn text_of(row: &Row, part: Part) -> Option<&str> {
    texts(row)
        .into_iter()
        .find_map(|(p, _, text)| (p == part).then_some(text))
}

/// The most hits kept: a common word in a long thread would otherwise hold a hit for every
/// occurrence.
pub(crate) const HIT_CAP: usize = 10_000;

/// Where a run of rows sits in the thread: how many rows come before it in reading order, and
/// how many prompts. A run's hits are found once, as if it were the thread, and placed after.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Place {
    pub(crate) rows: usize,
    pub(crate) turns: usize,
}

impl Place {
    /// What `rows` take up (folds opened out).
    pub(crate) fn taken_by(rows: &[Row]) -> Self {
        rows.iter().fold(Self::default(), |at, row| match row {
            Row::Worked { rows, .. } => {
                let inner = Self::taken_by(rows);
                Self {
                    rows: at.rows + inner.rows,
                    turns: at.turns + inner.turns,
                }
            }
            Row::User { steer: false, .. } => Self {
                rows: at.rows + 1,
                turns: at.turns + 1,
            },
            _ => Self {
                rows: at.rows + 1,
                ..at
            },
        })
    }
}

/// The first [`HIT_CAP`] matches of `matcher` in `rows` (folds opened out), in reading order.
pub(crate) fn find_all(rows: &[Row], matcher: &Matcher) -> Vec<Hit> {
    let mut hits: Vec<Hit> = placed(&find_in_run(rows, matcher), Place::default()).collect();
    hits.truncate(HIT_CAP);
    hits
}

/// The matches in one run's `rows`, numbered from the run's own start: its first row is row 0,
/// and `turn` counts the prompts in the run so far (0 before its first). [`placed`] moves them
/// into the thread.
pub(crate) fn find_in_run(rows: &[Row], matcher: &Matcher) -> Vec<Hit> {
    let mut hits = Vec::new();
    if matcher.is_empty() {
        return hits;
    }
    let (mut ordinal, mut turn) = (0, 0);
    scan(rows, None, matcher, &mut ordinal, &mut turn, &mut hits);
    hits.truncate(HIT_CAP);
    hits
}

/// `hits` of [`find_in_run`] as they are once the run sits at `at` in the thread.
pub(crate) fn placed(hits: &[Hit], at: Place) -> impl Iterator<Item = Hit> + '_ {
    hits.iter().map(move |hit| Hit {
        ordinal: hit.ordinal + at.rows,
        turn: (hit.turn + at.turns).max(1),
        ..hit.clone()
    })
}

fn scan(
    rows: &[Row],
    fold: Option<Key>,
    matcher: &Matcher,
    ordinal: &mut usize,
    turn: &mut usize,
    hits: &mut Vec<Hit>,
) {
    for row in rows {
        if hits.len() >= HIT_CAP {
            return;
        }
        if let Row::Worked { key, rows, .. } = row {
            scan(rows, Some(*key), matcher, ordinal, turn, hits);
            continue;
        }
        if matches!(row, Row::User { steer: false, .. }) {
            *turn += 1;
        }
        for (part, source, text) in texts(row) {
            hits.extend(
                matcher
                    .matches_up_to(text, HIT_CAP - hits.len())
                    .into_iter()
                    .map(|range| Hit {
                        key: row.key(),
                        part,
                        range,
                        fold,
                        ordinal: *ordinal,
                        turn: *turn,
                        source,
                    }),
            );
        }
        *ordinal += 1;
    }
}

/// "1 of 7", the words the bar shows. `None` with no query; "No results" when nothing matches.
pub(crate) fn count_label(query: &str, hits: usize, current: Option<usize>) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    Some(match current {
        Some(at) if hits >= HIT_CAP => format!("{} of 10,000+", at + 1),
        Some(at) if hits > 0 => format!("{} of {hits}", at + 1),
        _ => "No results".to_owned(),
    })
}

/// The hits of the whole thread and which one is current.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) hits: Vec<Hit>,
    pub(crate) current: Option<usize>,
}

impl Found {
    /// A fresh search: the first hit is current.
    pub(crate) fn new(hits: Vec<Hit>) -> Self {
        let current = (!hits.is_empty()).then_some(0);
        Self { hits, current }
    }

    /// The same search over rows that changed: the current match stays the current match. When
    /// it is gone (its text was cut), the next one in reading order takes its place.
    pub(crate) fn recounted(&self, hits: Vec<Hit>) -> Self {
        let current = self
            .current
            .and_then(|at| self.hits.get(at))
            .and_then(|was| {
                hits.iter()
                    .position(|h| h.same_as(was))
                    .or_else(|| hits.iter().position(|h| h.order() >= was.order()))
                    .or_else(|| hits.len().checked_sub(1))
            });
        let current = current.or_else(|| (!hits.is_empty()).then_some(0));
        Self { hits, current }
    }

    /// The next (or previous) hit, wrapping at the ends.
    pub(crate) fn step(&mut self, forward: bool) -> Option<&Hit> {
        let n = self.hits.len();
        if n == 0 {
            return None;
        }
        let at = match (self.current, forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(at), true) => (at + 1) % n,
            (Some(at), false) => (at + n - 1) % n,
        };
        self.current = Some(at);
        self.hits.get(at)
    }

    #[cfg(test)]
    pub(crate) fn go_to(&mut self, at: usize) -> Option<&Hit> {
        let hit = self.hits.get(at)?;
        self.current = Some(at);
        Some(hit)
    }

    pub(crate) fn current_hit(&self) -> Option<&Hit> {
        self.hits.get(self.current?)
    }
}

/// What must be open for a hit to show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Reveal {
    /// The fold it is in, closed.
    pub(crate) fold: Option<Key>,
    /// The row itself (reasoning, a user message cut short).
    pub(crate) row: bool,
    /// The call whose output holds it, and whether the cut part of that output must show.
    pub(crate) output: Option<(String, bool)>,
}

/// What `hit` needs opened in `row`.
pub(crate) fn reveal(hit: &Hit, row: &Row, fold: Option<Key>) -> Reveal {
    let mut need = Reveal {
        fold,
        ..Reveal::default()
    };
    match (row, hit.part) {
        (Row::Reasoning { .. }, _) => need.row = true,
        (Row::User { text, .. }, _) => {
            let shown = convo::user_text(text, false);
            let visible = shown.shown.len().saturating_sub("\u{2026}".len());
            need.row = shown.toggle.is_some() && hit.range.end > visible;
        }
        (Row::Tools { calls, .. }, Part::Output(n)) => {
            if let Some(call) = calls.get(n)
                && let Some(out) = &call.output
            {
                let cut = hit.range.end > facts::fold(&out.text, OUTPUT_LINES).shown.len();
                need.output = Some((call.id.clone(), cut));
            }
        }
        _ => {}
    }
    need
}

/// Whether the output of `call` can open: only a call with its own step line does (a run of
/// edits is one line, and its calls' output has no line to open from).
pub(crate) fn output_can_open(calls: &[ToolEntry], call: &str) -> bool {
    convo::step_lines(calls)
        .iter()
        .any(|l| l.call.as_deref() == Some(call))
}

/// A set of open things, and who opened them: the owner, or search. Search's are closed again
/// when it ends; the owner's stay. A thing in both counts as the owner's.
#[derive(Clone, Debug)]
pub(crate) struct Opens<K: Eq + Hash + Clone> {
    owner: HashSet<K>,
    search: HashSet<K>,
}

impl<K: Eq + Hash + Clone> Default for Opens<K> {
    fn default() -> Self {
        Self {
            owner: HashSet::new(),
            search: HashSet::new(),
        }
    }
}

impl<K: Eq + Hash + Clone> Opens<K> {
    pub(crate) fn contains(&self, k: &K) -> bool {
        self.owner.contains(k) || self.search.contains(k)
    }

    /// The owner opens or closes `k`. A thing search had opened is the owner's from here on, so
    /// it is not search's to close again: clicking it closes it, and that stays closed.
    pub(crate) fn toggle(&mut self, k: K) {
        if self.contains(&k) {
            self.owner.remove(&k);
            self.search.remove(&k);
        } else {
            self.owner.insert(k);
        }
    }

    /// Search opens `k`. Returns whether it was closed: a thing already open (the owner's, or
    /// search's) is left as it is.
    pub(crate) fn search_open(&mut self, k: K) -> bool {
        if self.contains(&k) {
            return false;
        }
        self.search.insert(k);
        true
    }

    /// Search ends: what it opened closes. Returns those.
    pub(crate) fn end_search(&mut self) -> Vec<K> {
        self.search.drain().collect()
    }

    /// Whether search holds anything open.
    #[cfg(test)]
    pub(crate) fn search_holds(&self) -> bool {
        !self.search.is_empty()
    }

    /// The owner opens `k` (a demo hook, a restore). A no-op when it is open.
    pub(crate) fn owner_open(&mut self, k: K) {
        if !self.contains(&k) {
            self.owner.insert(k);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::transcript::{Outcome, ToolKind};
    use reno_proto::ToolOutput;

    fn query(text: &str) -> Query {
        Query {
            text: text.to_owned(),
            ..Query::default()
        }
    }

    fn search(rows: &[Row], needle: &str) -> Vec<Hit> {
        find_all(rows, &query(needle).compile().unwrap())
    }

    fn matches_of(hay: &str, needle: &str) -> Vec<Range<usize>> {
        query(needle).compile().unwrap().matches(hay)
    }

    fn count_in(rows: &[Row], query: &Query) -> usize {
        find_all(rows, &query.compile().unwrap()).len()
    }

    fn prose(n: u64, text: &str) -> Row {
        Row::Prose {
            key: Key(n),
            text: text.to_owned(),
        }
    }

    fn user(n: u64, text: &str) -> Row {
        Row::User {
            key: Key(n),
            text: text.to_owned(),
            steer: false,
            at_ms: None,
        }
    }

    fn call(id: &str, label: &str, out: Option<&str>) -> ToolEntry {
        ToolEntry {
            id: id.to_owned(),
            kind: ToolKind::Command,
            label: label.to_owned(),
            outcome: Outcome::Ok(None),
            output: out.map(|t| ToolOutput {
                text: t.to_owned(),
                bytes: t.len() as u64,
                truncated: false,
            }),
        }
    }

    fn fold(n: u64, rows: Vec<Row>) -> Row {
        Row::Worked {
            key: Key::fold(Key(n)),
            secs: None,
            rows,
        }
    }

    #[test]
    fn matches_ignore_case_do_not_overlap_and_stay_on_character_boundaries() {
        assert_eq!(
            matches_of("Supervisor.rs, supervisor.RS", "supervisor.rs"),
            vec![0..13, 15..28]
        );
        assert_eq!(matches_of("aaaa", "aa"), vec![0..2, 2..4]);
        assert_eq!(matches_of("abc", ""), Vec::<Range<usize>>::new());
        // Multi-byte text: the ranges are bytes of the haystack.
        let hay = "d\u{fc}sseldorf \u{c9}COLE \u{e9}cole";
        let found = matches_of(hay, "\u{e9}cole");
        assert_eq!(found.len(), 2);
        for r in &found {
            assert!(hay.is_char_boundary(r.start) && hay.is_char_boundary(r.end));
        }
        assert_eq!(&hay[found[0].clone()], "\u{c9}COLE");
    }

    #[test]
    fn match_case_whole_word_and_regex_each_change_the_count() {
        let rows = vec![prose(0, "cat Cat concat cat.")];
        let plain = query("cat");
        assert_eq!(count_in(&rows, &plain), 4, "case is ignored by default");
        let case = Query {
            case: true,
            ..plain.clone()
        };
        assert_eq!(count_in(&rows, &case), 3, "match case drops `Cat`");
        let word = Query {
            word: true,
            ..plain.clone()
        };
        assert_eq!(count_in(&rows, &word), 3, "whole word drops `concat`");
        let both = Query {
            case: true,
            word: true,
            ..plain
        };
        assert_eq!(count_in(&rows, &both), 2);

        let literal = query("c.t");
        assert_eq!(count_in(&rows, &literal), 0, "a dot is a dot without regex");
        let regex = Query {
            regex: true,
            ..literal.clone()
        };
        assert_eq!(count_in(&rows, &regex), 4);
        let regex_case = Query {
            case: true,
            ..regex.clone()
        };
        assert_eq!(count_in(&rows, &regex_case), 3);
        let regex_word = Query {
            word: true,
            ..regex
        };
        assert_eq!(count_in(&rows, &regex_word), 3);
    }

    #[test]
    fn whole_word_needs_a_boundary_only_where_the_query_ends_in_a_word_character() {
        let rows = vec![prose(0, "a.rs b.rsx .rs")];
        let word = |text: &str| Query {
            word: true,
            ..query(text)
        };
        assert_eq!(count_in(&rows, &word("a.rs")), 1);
        assert_eq!(count_in(&rows, &word("b.rs")), 0, "`b.rsx` is another word");
        assert_eq!(
            count_in(&rows, &word(".rs")),
            2,
            "the dot needs no boundary before it, the s still needs one after"
        );
    }

    #[test]
    fn a_regex_match_is_never_empty_and_stays_on_character_boundaries() {
        let regex = Query {
            regex: true,
            ..query("\u{e9}*")
        };
        let hay = "x \u{e9}\u{e9} y";
        let found = regex.compile().unwrap().matches(hay);
        assert_eq!(found.len(), 1, "empty matches are skipped: {found:?}");
        assert_eq!(&hay[found[0].clone()], "\u{e9}\u{e9}");
        assert!(query("").compile().unwrap().matches(hay).is_empty());
    }

    #[test]
    fn a_regex_that_does_not_parse_or_is_too_large_says_why() {
        let bad = Query {
            regex: true,
            ..query("(open")
        };
        let why = bad.compile().unwrap_err();
        assert!(why.contains("unclosed group"), "{why}");
        assert!(!why.contains('\n'), "one line, fit for the strip: {why}");

        let huge = Query {
            regex: true,
            ..query("\\pL{1000}\\pL{1000}")
        };
        let why = huge.compile().unwrap_err();
        assert!(why.contains("too large"), "{why}");

        assert!(
            query("(open").compile().is_ok(),
            "plain text is never a pattern"
        );

        // The pattern must parse on its own: the group around it must not hide a stray paren.
        let escape = Query {
            regex: true,
            ..query("a)|(b")
        };
        let why = escape.compile().unwrap_err();
        assert!(why.contains("unopened group"), "{why}");
        assert!(!why.contains('\n'), "one line: {why}");
    }

    #[test]
    fn a_recount_keeps_the_current_hit_once_one_is_gone_to() {
        let regex = Query {
            regex: true,
            ..query("x+")
        };
        let rows = vec![prose(2, "x xx"), prose(4, "xxx")];
        let mut f = Found::new(find_all(&rows, &regex.compile().unwrap()));
        f.go_to(2);
        let more = vec![prose(2, "x xx x"), prose(4, "xxx"), prose(6, "x")];
        let again = f.recounted(find_all(&more, &regex.compile().unwrap()));
        let current = again.current_hit().unwrap();
        assert_eq!((current.key, current.range.start), (Key(4), 0));
        assert_eq!(again.current, Some(3));
    }

    #[test]
    fn hits_come_in_reading_order_through_closed_folds_and_name_their_turn() {
        let rows = vec![
            user(0, "look at supervisor.rs"),
            fold(
                2,
                vec![
                    prose(2, "note: supervisor.rs waits"),
                    Row::Tools {
                        key: Key(4),
                        calls: vec![
                            call(
                                "a",
                                "cat src/supervisor.rs",
                                Some("fn main() {}\nsupervisor.rs:3\n"),
                            ),
                            call("b", "ls", None),
                        ],
                    },
                ],
            ),
            prose(8, "done, see supervisor.rs"),
            user(1 << 40, "again?"),
            prose(10 | 1 << 40, "no supervisor.rs this time"),
        ];
        let hits = search(&rows, "SUPERVISOR.RS");
        let at: Vec<_> = hits
            .iter()
            .map(|h| (h.key.0 & 0xff, h.part, h.source, h.turn))
            .collect();
        assert_eq!(
            at,
            vec![
                (0, Part::Whole, Source::Prompt, 1),
                (2, Part::Whole, Source::Reply, 1),
                (4, Part::Label(0), Source::Command, 1),
                (4, Part::Output(0), Source::Output, 1),
                (8, Part::Whole, Source::Reply, 1),
                (10, Part::Whole, Source::Reply, 2),
            ]
        );
        assert!(hits.windows(2).all(|w| w[0].order() < w[1].order()));
        // The text each range points into.
        let first_output = &hits[3];
        let row = &rows[1];
        let Row::Worked { rows: inner, .. } = row else {
            unreachable!()
        };
        let text = text_of(&inner[1], first_output.part).unwrap();
        assert_eq!(&text[first_output.range.clone()], "supervisor.rs");
        assert!(search(&rows, "").is_empty());
    }

    #[test]
    fn the_count_reads_one_of_n_and_says_when_nothing_matches() {
        assert_eq!(count_label("x", 7, Some(0)).as_deref(), Some("1 of 7"));
        assert_eq!(count_label("x", 7, Some(2)).as_deref(), Some("3 of 7"));
        assert_eq!(count_label("x", 0, None).as_deref(), Some("No results"));
        assert_eq!(count_label("", 0, None), None);
    }

    #[test]
    fn hits_stop_at_ten_thousand_and_the_count_says_so() {
        let rows = vec![
            prose(0, &"x ".repeat(HIT_CAP - 1)),
            prose(2, &"x ".repeat(HIT_CAP)),
            prose(4, "x"),
        ];
        let hits = search(&rows, "x");
        assert_eq!(hits.len(), HIT_CAP);
        assert!(
            hits.windows(2).all(|w| w[0].order() < w[1].order()),
            "the first ten thousand, in reading order"
        );
        assert_eq!(hits.last().unwrap().key, Key(2));

        assert_eq!(
            count_label("x", HIT_CAP, Some(0)).as_deref(),
            Some("1 of 10,000+")
        );
        assert_eq!(
            count_label("x", HIT_CAP - 1, Some(0)).as_deref(),
            Some("1 of 9999")
        );
    }

    fn found(n: usize) -> Found {
        let rows: Vec<Row> = (0..n).map(|i| prose(2 * i as u64, "x")).collect();
        Found::new(search(&rows, "x"))
    }

    #[test]
    fn next_and_previous_wrap_at_the_ends() {
        let mut f = found(3);
        assert_eq!(f.current, Some(0));
        f.step(false);
        assert_eq!(
            f.current,
            Some(2),
            "previous from the first wraps to the last"
        );
        f.step(true);
        assert_eq!(f.current, Some(0), "next from the last wraps to the first");
        f.step(true);
        f.step(true);
        assert_eq!(f.current, Some(2));
        assert!(Found::default().clone().step(true).is_none());
    }

    #[test]
    fn a_recount_keeps_the_current_match_where_it_is() {
        let rows = vec![prose(2, "x x"), prose(4, "x")];
        let mut f = Found::new(search(&rows, "x"));
        f.go_to(2);
        // A reply grew above the current match, and one was added after it.
        let more = vec![prose(2, "x x x"), prose(4, "x"), prose(6, "x")];
        let again = f.recounted(search(&more, "x"));
        assert_eq!(again.hits.len(), 5);
        let current = again.current_hit().unwrap();
        assert_eq!((current.key, current.range.start), (Key(4), 0));
        assert_eq!(again.current, Some(3));
    }

    #[test]
    fn a_recount_whose_current_match_is_gone_takes_the_next_one() {
        let rows = vec![prose(2, "x"), prose(4, "x"), prose(6, "x")];
        let mut f = Found::new(search(&rows, "x"));
        f.go_to(1);
        let after = vec![prose(2, "x"), prose(4, "-"), prose(6, "x")];
        let again = f.recounted(search(&after, "x"));
        assert_eq!(again.current_hit().map(|h| h.key), Some(Key(6)));
        // Nothing left, then something: the first hit.
        let none = f.recounted(Vec::new());
        assert_eq!(none.current, None);
        assert_eq!(none.recounted(search(&rows, "x")).current, Some(0));
    }

    #[test]
    fn a_hit_in_a_closed_fold_asks_for_the_fold() {
        let row = prose(2, "x");
        let hit = &search(std::slice::from_ref(&row), "x")[0];
        let fold_key = Key::fold(Key(2));
        let need = reveal(hit, &row, Some(fold_key));
        assert_eq!(need.fold, Some(fold_key));
        assert!(!need.row && need.output.is_none());
        assert_eq!(reveal(hit, &row, None), Reveal::default());
    }

    #[test]
    fn a_hit_in_reasoning_asks_for_the_row() {
        let row = Row::Reasoning {
            key: Key(2),
            text: "think about x".to_owned(),
        };
        let hit = &search(std::slice::from_ref(&row), "x")[0];
        assert!(reveal(hit, &row, None).row);
    }

    #[test]
    fn a_hit_past_the_cut_of_a_long_message_asks_to_show_more() {
        let long = format!("{} needle", "word ".repeat(100));
        let row = user(0, &long);
        let hit = &search(std::slice::from_ref(&row), "needle")[0];
        assert!(reveal(hit, &row, None).row);
        let early = user(0, &format!("needle {}", "word ".repeat(100)));
        let hit = &search(std::slice::from_ref(&early), "needle")[0];
        assert!(!reveal(hit, &early, None).row, "the first lines show");
        let short = user(0, "needle");
        let hit = &search(std::slice::from_ref(&short), "needle")[0];
        assert!(!reveal(hit, &short, None).row);
    }

    #[test]
    fn a_hit_in_an_output_asks_for_it_and_for_its_cut_part_when_that_is_where_it_is() {
        let mut out = String::new();
        for i in 0..30 {
            out.push_str(&format!("line {i}\n"));
        }
        out.push_str("the needle is here\n");
        let row = Row::Tools {
            key: Key(2),
            calls: vec![call("c1", "cargo test", Some(&out))],
        };
        let hits = search(std::slice::from_ref(&row), "needle");
        let need = reveal(&hits[0], &row, None);
        assert_eq!(need.output, Some(("c1".to_owned(), true)));
        let early = Row::Tools {
            key: Key(2),
            calls: vec![call("c1", "cargo test", Some("the needle\nline 2\n"))],
        };
        let hits = search(std::slice::from_ref(&early), "needle");
        assert_eq!(
            reveal(&hits[0], &early, None).output,
            Some(("c1".to_owned(), false))
        );
    }

    #[test]
    fn closing_search_closes_what_search_opened_and_only_that() {
        let mut o: Opens<u32> = Opens::default();
        o.toggle(1);
        assert!(o.search_open(2));
        assert!(!o.search_open(1), "the owner's stays the owner's");
        assert!(!o.search_open(2), "already open");
        assert!(o.contains(&1) && o.contains(&2));
        let mut closed = o.end_search();
        closed.sort_unstable();
        assert_eq!(closed, vec![2]);
        assert!(o.contains(&1) && !o.contains(&2));
        assert!(!o.search_holds());
    }

    #[test]
    fn a_row_the_owner_touches_becomes_the_owners_even_when_search_opened_it() {
        let mut o: Opens<u32> = Opens::default();
        o.search_open(5);
        // The owner closes it: it stays closed, and search has nothing to close.
        o.toggle(5);
        assert!(!o.contains(&5));
        assert!(o.end_search().is_empty());
        // The owner opens what search then opens again over: also left alone.
        o.toggle(5);
        assert!(o.contains(&5));
        assert!(!o.search_open(5));
        assert!(o.end_search().is_empty());
        assert!(o.contains(&5));
    }

    #[test]
    fn a_row_open_for_another_reason_is_not_search_to_close() {
        // A reasoning row that streams is open because the owner (or the stream) holds it open;
        // search finding a match in it adds nothing, so closing search leaves it open.
        let mut o: Opens<u32> = Opens::default();
        o.owner_open(9);
        assert!(!o.search_open(9));
        assert!(o.end_search().is_empty());
        assert!(o.contains(&9));
    }

    #[test]
    fn only_a_call_with_its_own_step_line_can_open_its_output() {
        let mut edit = call("e1", "edit a.rs", Some("x"));
        edit.kind = ToolKind::Edit;
        let mut edit2 = call("e2", "edit b.rs", Some("y"));
        edit2.kind = ToolKind::Edit;
        let calls = vec![edit, edit2, call("c1", "ls", Some("z"))];
        assert!(!output_can_open(&calls, "e1"));
        assert!(output_can_open(&calls, "c1"));
    }
}
