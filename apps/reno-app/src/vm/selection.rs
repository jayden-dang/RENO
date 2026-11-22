//! Selecting text across the transcript (TRANSCRIPT-2.1, 2.4 to 2.8, 2.13).
//!
//! A selection is two [`Pos`]itions in reading order, each naming an entry, one of the texts it
//! shows and a byte offset in that text's *source*: a painted position is never what is kept, so
//! a selection survives scrolling and a later comment can anchor to it. What a selection covers
//! is resolved from the rows (not from what is painted): [`anchors`] gives, for each entry, its
//! key and the byte range of its source text (none for text that has no source: a fold head, a
//! step line, a cut note), and [`copy_text`] gives what the owner saw there.
//!
//! Only shown text is a [`Text`]: a closed fold, collapsed reasoning, a collapsed message and a
//! cut output give the lines they show and no more; control text (buttons, "Show N more lines",
//! chips, "Thinking", hover times) is not one. Pure: no window, no layout.

use std::ops::Range;

use crate::transcript::{Key, Row};
use crate::vm::anchor::{self, Atom};
use crate::vm::changes as facts;
use crate::vm::convo;
use reno_markdown::BlockTree;

/// What kind of text of an entry a position is in. The order is reading order within an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Kind {
    /// A head line that has no source: "Worked for 3m 12s", "Reasoning".
    Head,
    /// A step line of a tool group ("Ran cargo test"): no source.
    Step,
    /// The entry's own text: a prompt, a reply, reasoning.
    Whole,
    /// What a step printed, as far as it is shown.
    Output,
    /// "Output was cut: 482 KB total": no source.
    Cut,
}

/// One of an entry's texts: the `n`th of its kind (a tool group has a step line, output and cut
/// note for each step, numbered by the step line).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Slot {
    pub(crate) n: usize,
    pub(crate) kind: Kind,
}

impl Slot {
    pub(crate) const fn new(n: usize, kind: Kind) -> Self {
        Self { n, kind }
    }

    pub(crate) const WHOLE: Self = Self::new(0, Kind::Whole);
    pub(crate) const HEAD: Self = Self::new(0, Kind::Head);
}

/// A place in the transcript: the entry (by its place in reading order), one of its texts, and a
/// byte offset in that text's source (zero in a text that has none).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Pos {
    pub(crate) ordinal: usize,
    pub(crate) slot: Slot,
    pub(crate) off: usize,
}

/// What is selected: where the drag started and where it is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) anchor: Pos,
    pub(crate) head: Pos,
}

impl Selection {
    pub(crate) fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// The start and the end in reading order.
    pub(crate) fn ordered(&self) -> (Pos, Pos) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// What of the text `slot` of entry `ordinal` is selected, as bytes of its source: `len` is
    /// the source's length, `None` for a text with no source, whose range is empty when any of
    /// it is selected.
    pub(crate) fn within(
        &self,
        ordinal: usize,
        slot: Slot,
        len: Option<usize>,
    ) -> Option<Range<usize>> {
        let (start, end) = self.ordered();
        if start == end || (ordinal, slot) < (start.ordinal, start.slot) {
            return None;
        }
        if (ordinal, slot) > (end.ordinal, end.slot) {
            return None;
        }
        let Some(len) = len else {
            return Some(0..0);
        };
        let from = if (ordinal, slot) == (start.ordinal, start.slot) {
            start.off
        } else {
            0
        };
        let to = if (ordinal, slot) == (end.ordinal, end.slot) {
            end.off
        } else {
            len
        };
        (from < to.min(len)).then(|| from..to.min(len))
    }
}

/// One text an entry shows, as atoms over its source.
#[derive(Clone, Debug)]
pub(crate) struct Text {
    pub(crate) slot: Slot,
    /// `false` for a text that has no source.
    pub(crate) sourced: bool,
    /// Length of the source the atoms cover.
    pub(crate) len: usize,
    pub(crate) atoms: Vec<Atom>,
}

impl Text {
    /// A text that is its own source, from byte zero.
    fn plain(slot: Slot, text: &str) -> Self {
        Self {
            slot,
            sourced: true,
            len: text.len(),
            atoms: vec![Atom::plain(0..text.len(), text)],
        }
    }

    /// A text that has no source.
    fn sourceless(slot: Slot, text: &str) -> Self {
        Self {
            slot,
            sourced: false,
            len: 0,
            atoms: vec![Atom::plain(0..0, text)],
        }
    }

    fn copy_all(&self) -> String {
        anchor::copy(&self.atoms, 0..self.len.max(1))
    }
}

/// What of the transcript is open, and the parsed reply of a prose row: what decides which
/// texts a row shows.
pub(crate) struct Shown<'a> {
    pub(crate) open: &'a dyn Fn(Key) -> bool,
    pub(crate) output_open: &'a dyn Fn(&str) -> bool,
    pub(crate) output_full: &'a dyn Fn(&str) -> bool,
    pub(crate) tree: &'a dyn Fn(Key, &str) -> BlockTree,
}

/// How many bytes of a user message's source are shown: all of it, or what is left of it before
/// the ellipsis a collapsed one ends with.
pub(crate) fn kept_len(view: &convo::UserText) -> usize {
    if view.toggle == Some("Show more") {
        view.shown.trim_end_matches('\u{2026}').len()
    } else {
        view.shown.len()
    }
}

/// The texts `row` shows, in reading order.
pub(crate) fn texts_of(row: &Row, shown: &Shown<'_>) -> Vec<Text> {
    match row {
        Row::User { key, text, .. } => {
            let view = convo::user_text(text, (shown.open)(*key));
            let kept = kept_len(&view);
            vec![Text::plain(Slot::WHOLE, &view.shown[..kept])]
        }
        Row::Prose { key, text } => {
            let tree = (shown.tree)(*key, text);
            let atoms = anchor::atoms(text, &tree);
            vec![Text {
                slot: Slot::WHOLE,
                sourced: true,
                len: text.len(),
                atoms,
            }]
        }
        Row::Reasoning { key, text } => {
            let mut out = vec![Text::sourceless(Slot::HEAD, "Reasoning")];
            if (shown.open)(*key) {
                out.push(Text::plain(Slot::WHOLE, text));
            }
            out
        }
        Row::Worked { secs, .. } => {
            vec![Text::sourceless(Slot::HEAD, &convo::worked_label(*secs))]
        }
        Row::Tools { calls, .. } => {
            let mut out = Vec::new();
            for (n, line) in convo::step_lines(calls).iter().enumerate() {
                out.push(Text::sourceless(Slot::new(n, Kind::Step), &line.text));
                let call = line
                    .call
                    .as_ref()
                    .and_then(|id| calls.iter().find(|c| &c.id == id));
                let Some((call, output)) = call.and_then(|c| Some((c, c.output.as_ref()?))) else {
                    continue;
                };
                if !(shown.output_open)(&call.id) {
                    continue;
                }
                let body = if (shown.output_full)(&call.id) {
                    output.text.strip_suffix('\n').unwrap_or(&output.text)
                } else {
                    facts::fold(&output.text, facts::OUTPUT_LINES).shown
                };
                out.push(Text::plain(Slot::new(n, Kind::Output), body));
                if let Some(note) = facts::cut_note(output) {
                    out.push(Text::sourceless(Slot::new(n, Kind::Cut), &note));
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// An entry in reading order.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry<'a> {
    pub(crate) ordinal: usize,
    pub(crate) row: &'a Row,
    /// Whether it is on screen: not inside a closed fold.
    pub(crate) shown: bool,
}

/// Every entry of `rows` in reading order, those inside folds after the fold's head. The
/// ordinal does not change when a fold opens or closes.
pub(crate) fn reading<'a>(rows: &'a [Row], open: &dyn Fn(Key) -> bool) -> Vec<Entry<'a>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(Entry {
            ordinal: out.len(),
            row,
            shown: true,
        });
        if let Row::Worked { key, rows, .. } = row {
            let shown = open(*key);
            for inner in rows {
                out.push(Entry {
                    ordinal: out.len(),
                    row: inner,
                    shown,
                });
            }
        }
    }
    out
}

/// What a selection covers of one entry (TRANSCRIPT-2.13).
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Anchor {
    pub(crate) key: Key,
    pub(crate) slot: Slot,
    /// Bytes of the entry's source text, on character boundaries; none for a text with no source.
    pub(crate) range: Option<Range<usize>>,
}

fn covered<'a>(
    sel: &'a Selection,
    entries: &'a [Entry<'a>],
    shown: &'a Shown<'a>,
) -> impl Iterator<Item = (Entry<'a>, Text, Range<usize>)> + 'a {
    let (start, end) = sel.ordered();
    let on = !sel.is_empty();
    entries
        .iter()
        .filter(move |e| on && e.shown && (start.ordinal..=end.ordinal).contains(&e.ordinal))
        .flat_map(move |e| {
            texts_of(e.row, shown)
                .into_iter()
                .filter_map(move |t| {
                    let range = sel.within(e.ordinal, t.slot, t.sourced.then_some(t.len))?;
                    Some((*e, t, range))
                })
                .collect::<Vec<_>>()
        })
}

/// The key and source range of each text the selection covers, in reading order.
#[allow(dead_code)] // The comments item anchors to these (TRANSCRIPT-2.13); the tests read them.
pub(crate) fn anchors(sel: &Selection, entries: &[Entry<'_>], shown: &Shown<'_>) -> Vec<Anchor> {
    covered(sel, entries, shown)
        .map(|(e, t, range)| Anchor {
            key: e.row.key(),
            slot: t.slot,
            range: t.sourced.then_some(range),
        })
        .collect()
}

/// What ⌘C puts on the clipboard: the text of each covered entry as the owner saw it, one blank
/// line between entries and a line break between an entry's own texts.
pub(crate) fn copy_text(sel: &Selection, entries: &[Entry<'_>], shown: &Shown<'_>) -> String {
    let mut out: Vec<(usize, String)> = Vec::new();
    for (e, t, range) in covered(sel, entries, shown) {
        let part = if t.sourced {
            anchor::copy(&t.atoms, range)
        } else {
            t.copy_all()
        };
        if part.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some((ordinal, text)) if *ordinal == e.ordinal => {
                text.push('\n');
                text.push_str(&part);
            }
            _ => out.push((e.ordinal, part)),
        }
    }
    out.into_iter()
        .map(|(_, t)| t)
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{Outcome, ToolEntry, ToolKind};
    use reno_proto::ToolOutput;
    use std::collections::HashSet;

    fn user(n: u64, text: &str) -> Row {
        Row::User {
            key: Key(n),
            text: text.to_owned(),
            steer: false,
            at_ms: None,
        }
    }

    fn prose(n: u64, text: &str) -> Row {
        Row::Prose {
            key: Key(n),
            text: text.to_owned(),
        }
    }

    fn tools(n: u64, label: &str, out: &str, truncated: bool) -> Row {
        Row::Tools {
            key: Key(n),
            calls: vec![ToolEntry {
                id: format!("c{n}"),
                kind: ToolKind::Command,
                label: label.to_owned(),
                outcome: Outcome::Ok(None),
                output: Some(ToolOutput {
                    text: out.to_owned(),
                    bytes: out.len() as u64,
                    truncated,
                }),
            }],
        }
    }

    fn worked(n: u64, inner: Vec<Row>) -> Row {
        Row::Worked {
            key: Key::fold(Key(n)),
            secs: Some(192),
            rows: inner,
        }
    }

    struct World {
        open: HashSet<Key>,
        outputs: HashSet<String>,
    }

    impl World {
        fn new() -> Self {
            Self {
                open: HashSet::new(),
                outputs: HashSet::new(),
            }
        }

        fn run<T>(&self, rows: &[Row], f: impl FnOnce(&[Entry<'_>], &Shown<'_>) -> T) -> T {
            let open = |k: Key| self.open.contains(&k);
            let entries = reading(rows, &open);
            let tree = |_: Key, text: &str| reno_markdown::parse_full(text);
            let out_open = |id: &str| self.outputs.contains(id);
            let never = |_: &str| false;
            let shown = Shown {
                open: &open,
                output_open: &out_open,
                output_full: &never,
                tree: &tree,
            };
            f(&entries, &shown)
        }

        fn copy(&self, rows: &[Row], sel: Selection) -> String {
            self.run(rows, |e, s| copy_text(&sel, e, s))
        }

        fn anchors(&self, rows: &[Row], sel: Selection) -> Vec<Anchor> {
            self.run(rows, |e, s| anchors(&sel, e, s))
        }
    }

    fn at(ordinal: usize, off: usize) -> Pos {
        Pos {
            ordinal,
            slot: Slot::WHOLE,
            off,
        }
    }

    fn drag(a: Pos, b: Pos) -> Selection {
        Selection { anchor: a, head: b }
    }

    #[test]
    fn a_drag_from_the_middle_of_the_prompt_to_the_middle_of_the_reply_copies_in_order() {
        let rows = [
            user(0, "fix the handshake please"),
            prose(2, "It waits on **the supervisor** to start."),
        ];
        let w = World::new();
        let got = w.copy(&rows, drag(at(0, 4), at(1, 20)));
        assert_eq!(got, "the handshake please\n\nIt waits on the su");
    }

    #[test]
    fn dragging_backwards_selects_the_same_text() {
        let rows = [user(0, "one two"), prose(2, "three four")];
        let w = World::new();
        let fwd = w.copy(&rows, drag(at(0, 4), at(1, 5)));
        let back = w.copy(&rows, drag(at(1, 5), at(0, 4)));
        assert_eq!(fwd, "two\n\nthree");
        assert_eq!(back, fwd);
    }

    #[test]
    fn anchors_are_entry_keys_with_utf8_byte_ranges_and_none_for_text_without_source() {
        let rows = [
            user(0, "héllo wörld"),
            worked(2, vec![prose(2, "hidden")]),
            prose(8, "tail text"),
        ];
        let w = World::new();
        let a = w.anchors(&rows, drag(at(0, "héllo ".len()), at(3, 4)));
        assert_eq!(
            a,
            vec![
                Anchor {
                    key: Key(0),
                    slot: Slot::WHOLE,
                    range: Some(7..13)
                },
                Anchor {
                    key: Key::fold(Key(2)),
                    slot: Slot::HEAD,
                    range: None
                },
                Anchor {
                    key: Key(8),
                    slot: Slot::WHOLE,
                    range: Some(0..4)
                },
            ]
        );
        // Every range ends on a character boundary of its source.
        assert!("héllo wörld".is_char_boundary(7) && "héllo wörld".is_char_boundary(13));
    }

    #[test]
    fn a_closed_fold_gives_its_head_line_only_and_an_open_one_gives_its_rows() {
        let rows = [
            user(0, "go"),
            worked(2, vec![prose(2, "inner one"), prose(4, "inner two")]),
            prose(8, "end"),
        ];
        let mut w = World::new();
        let sel = drag(at(0, 0), at(4, 3));
        assert_eq!(w.copy(&rows, sel), "go\n\nWorked for 3m 12s\n\nend");
        w.open.insert(Key::fold(Key(2)));
        assert_eq!(
            w.copy(&rows, sel),
            "go\n\nWorked for 3m 12s\n\ninner one\n\ninner two\n\nend"
        );
    }

    #[test]
    fn collapsed_reasoning_gives_its_label_and_a_collapsed_message_its_shown_words() {
        let long = "word ".repeat(200);
        let rows = [
            user(0, &long),
            Row::Reasoning {
                key: Key(2),
                text: "secret thoughts".into(),
            },
        ];
        let mut w = World::new();
        let all = drag(at(0, 0), at(1, 0));
        let got = w.copy(&rows, all);
        assert!(got.starts_with("word word"), "{got}");
        assert!(!got.contains("secret"), "{got}");
        assert!(got.ends_with("Reasoning"), "{got}");
        assert!(got.len() < long.len() / 2, "the cut part is not copied");
        w.open.insert(Key(2));
        let got = w.copy(
            &rows,
            drag(
                at(0, 0),
                Pos {
                    ordinal: 1,
                    slot: Slot::WHOLE,
                    off: 6,
                },
            ),
        );
        assert!(got.ends_with("Reasoning\nsecret"), "{got}");
    }

    #[test]
    fn a_tool_group_gives_its_step_line_and_only_the_output_that_is_open() {
        let rows = [
            prose(0, "before"),
            tools(2, "cargo test", "line one\nline two", true),
            prose(8, "after"),
        ];
        let mut w = World::new();
        let sel = drag(at(0, 0), at(2, 5));
        assert_eq!(w.copy(&rows, sel), "before\n\nRan cargo test\n\nafter");
        w.outputs.insert("c2".into());
        let got = w.copy(&rows, sel);
        assert_eq!(
            got,
            "before\n\nRan cargo test\nline one\nline two\nOutput was cut: 17 B total\n\nafter"
        );
        assert!(!got.contains("Show"), "control text is never copied");
    }

    #[test]
    fn math_a_diagram_an_image_and_a_file_label_copy_as_their_equivalents() {
        let reply = "See $x^2$ and\n\n$$a+b$$\n\n```mermaid\nflowchart LR\n A-->B\n```\n\n![alt text](docs/a.png)\n\nin [sup](src/s.rs:4).";
        let rows = [prose(0, reply)];
        let w = World::new();
        let got = w.copy(&rows, drag(at(0, 0), at(0, reply.len())));
        assert_eq!(
            got,
            "See $x^2$ and\n\n$$a+b$$\n\n```mermaid\nflowchart LR\n A-->B\n```\n\n[alt text]\n\nin src/s.rs:4."
        );
    }

    #[test]
    fn a_click_selects_nothing_and_an_entry_without_texts_is_skipped() {
        let rows = [
            user(0, "a"),
            Row::Asked {
                key: Key(2),
                line: "asked".into(),
            },
            prose(4, "b"),
        ];
        let w = World::new();
        assert_eq!(w.copy(&rows, drag(at(0, 1), at(0, 1))), "");
        assert_eq!(w.copy(&rows, drag(at(0, 0), at(2, 1))), "a\n\nb");
        assert!(w.anchors(&rows, drag(at(0, 1), at(0, 1))).is_empty());
    }

    #[test]
    fn a_selection_inside_one_text_is_the_range_between_its_ends() {
        let s = drag(at(3, 9), at(3, 2));
        assert_eq!(s.within(3, Slot::WHOLE, Some(20)), Some(2..9));
        assert_eq!(s.within(2, Slot::WHOLE, Some(20)), None);
        assert_eq!(s.within(3, Slot::new(1, Kind::Step), None), None);
        let across = drag(at(1, 5), at(4, 1));
        assert_eq!(across.within(2, Slot::WHOLE, Some(7)), Some(0..7));
        assert_eq!(across.within(1, Slot::WHOLE, Some(7)), Some(5..7));
        assert_eq!(across.within(4, Slot::WHOLE, Some(7)), Some(0..1));
        assert_eq!(across.within(2, Slot::HEAD, None), Some(0..0));
    }
}
