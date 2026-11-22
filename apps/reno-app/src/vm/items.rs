//! The transcript's list items: a flat projection under the nested rows.
//!
//! The run screen keeps the nested [`Row`] model (a finished turn's interim rows sit inside its
//! "Worked for" row), because that is what the transcript's tests read. The list draws a flat
//! sequence instead: one item per top-level row and, while a fold is open, one item per row inside
//! it (the fold's head stays an item). [`project`] makes that sequence, and [`diff`] says what
//! the list must be told when the rows or the open folds change: items to measure again, and at
//! most one range to replace, so a streamed delta lays out only its own item and never replaces
//! the item under the scroll anchor.
//!
//! Pure: no window, no list, no clock.

use std::collections::HashMap;
use std::ops::Range;

use crate::transcript::{Key, Row};

/// One list item: where its row is, and what around it changes how it is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Item {
    pub(crate) key: Key,
    /// The top-level row this item is, or the fold it sits in.
    pub(crate) top: usize,
    /// Its index inside that fold; `None` for a top-level row (a fold's head included).
    pub(crate) inner: Option<usize>,
    pub(crate) decor: Decor,
}

/// What an item's drawing depends on besides its own row. A change here changes its height.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Decor {
    /// The last item of the list: it has the longer bottom space, and "Thinking" under it.
    pub(crate) last: bool,
    /// The prose just before a `Done`: it carries the quiet Copy.
    pub(crate) final_message: bool,
    /// For a top-level prose row, how many prose rows come before it: subagent chips hang on it.
    pub(crate) prose_at: Option<usize>,
}

/// The flat list over some rows.
#[derive(Clone, Debug, Default)]
pub(crate) struct Projection {
    pub(crate) items: Vec<Item>,
    by_key: HashMap<Key, usize>,
    /// The item of each top-level row.
    top_item: Vec<usize>,
    /// The item of each top-level prose row, by its place among them.
    prose_item: Vec<usize>,
    /// The thread's last `Done` row, the only one that says what the branch holds.
    last_done: Option<usize>,
}

impl Projection {
    /// The item holding the row `key`.
    pub(crate) fn item_of(&self, key: Key) -> Option<usize> {
        self.by_key.get(&key).copied()
    }

    /// The item of top-level row `row` (the list's length for a row past the end).
    pub(crate) fn item_of_top(&self, row: usize) -> usize {
        self.top_item.get(row).copied().unwrap_or(self.items.len())
    }

    /// The top-level row an item belongs to (the rows' count past the end).
    pub(crate) fn top_of(&self, item: usize, rows: usize) -> usize {
        self.items.get(item).map_or(rows, |i| i.top)
    }

    /// The item of the `n`th top-level prose row.
    pub(crate) fn prose_item(&self, n: usize) -> Option<usize> {
        self.prose_item.get(n).copied()
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    /// The item of the thread's last `Done` row.
    pub(crate) fn last_done_item(&self) -> Option<usize> {
        self.last_done.map(|top| self.item_of_top(top))
    }

    /// Whether `item` is the thread's last `Done` row.
    pub(crate) fn is_last_done(&self, item: usize) -> bool {
        self.last_done_item() == Some(item)
    }

    /// The row an item shows.
    pub(crate) fn row<'a>(&self, rows: &'a [Row], item: usize) -> Option<&'a Row> {
        row_of(rows, 0, self.items.get(item)?)
    }

    /// Makes the items of `rows[from..]` again (those of the rows before `from` stay) and says
    /// which items they replace. Every top-level row has an item, and the rows of each fold
    /// `is_open` says is open follow the fold's head.
    pub(crate) fn reproject(
        &mut self,
        rows: &[Row],
        from: usize,
        is_open: impl Fn(Key) -> bool,
    ) -> Vec<Item> {
        let from = from.min(self.top_item.len()).min(rows.len());
        let at = self.top_item.get(from).copied().unwrap_or(self.items.len());
        let replaced = self.items.split_off(at);
        for gone in &replaced {
            self.by_key.remove(&gone.key);
        }
        self.top_item.truncate(from);
        let prose_before = self.prose_item.iter().take_while(|&&i| i < at).count();
        self.prose_item.truncate(prose_before);
        if let Some(before) = self.items.last_mut() {
            before.decor.last = false;
        }
        for (top, row) in rows.iter().enumerate().skip(from) {
            let prose_at = matches!(row, Row::Prose { .. }).then_some(self.prose_item.len());
            let final_message = matches!(row, Row::Prose { .. })
                && matches!(rows.get(top + 1), Some(Row::Done { .. }));
            let here = self.items.len();
            self.top_item.push(here);
            if prose_at.is_some() {
                self.prose_item.push(here);
            }
            self.by_key.insert(row.key(), here);
            self.items.push(Item {
                key: row.key(),
                top,
                inner: None,
                decor: Decor {
                    last: false,
                    final_message,
                    prose_at,
                },
            });
            if let Row::Worked {
                key, rows: inner, ..
            } = row
                && is_open(*key)
            {
                for (i, child) in inner.iter().enumerate() {
                    self.by_key.insert(child.key(), self.items.len());
                    self.items.push(Item {
                        key: child.key(),
                        top,
                        inner: Some(i),
                        decor: Decor::default(),
                    });
                }
            }
        }
        if let Some(last) = self.items.last_mut() {
            last.decor.last = true;
        }
        self.last_done = rows.iter().rposition(|r| matches!(r, Row::Done { .. }));
        replaced
    }
}

/// The row an item shows, in `rows`, which start at top-level row `base`.
pub(crate) fn row_of<'a>(rows: &'a [Row], base: usize, item: &Item) -> Option<&'a Row> {
    let top = rows.get(item.top.checked_sub(base)?)?;
    match (item.inner, top) {
        (None, row) => Some(row),
        (Some(at), Row::Worked { rows, .. }) => rows.get(at),
        (Some(_), _) => None,
    }
}

/// The list over `rows`.
#[cfg(test)]
pub(crate) fn project(rows: &[Row], is_open: impl Fn(Key) -> bool) -> Projection {
    let mut p = Projection::default();
    p.reproject(rows, 0, is_open);
    p
}

/// Whether two rows draw the same item. A fold's head draws only its own line, so its inner rows
/// do not count, except how many there are.
fn same_item_content(old: &Row, new: &Row) -> bool {
    match (old, new) {
        (
            Row::Worked {
                key: ka,
                secs: sa,
                rows: ra,
            },
            Row::Worked {
                key: kb,
                secs: sb,
                rows: rb,
            },
        ) => ka == kb && sa == sb && ra.len() == rb.len(),
        _ => old == new,
    }
}

/// What the list must be told.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Items `.0` (old numbering) are replaced by `.1` new ones: at most one range.
    pub(crate) splice: Option<(Range<usize>, usize)>,
    /// Items, by their index after the splice, that stay but must be measured again.
    pub(crate) remeasure: Vec<usize>,
}

impl Plan {
    pub(crate) fn is_empty(&self) -> bool {
        self.splice.is_none() && self.remeasure.is_empty()
    }
}

/// A run of items and the rows they point into, which start at top-level row `base`.
#[derive(Clone, Copy)]
pub(crate) struct Side<'a> {
    pub(crate) rows: &'a [Row],
    pub(crate) base: usize,
    pub(crate) items: &'a [Item],
}

/// Compares the old items with the new ones by item key, from index `at` of the list: the items
/// that lead and trail with the same keys stay (measured again when their row or what they
/// depend on changed), and the stretch between is replaced in one range. The numbers in the
/// plan are the list's, so they start at `at`.
pub(crate) fn diff(old: Side<'_>, new: Side<'_>, at: usize) -> Plan {
    let (o, n) = (old.items, new.items);
    let changed =
        |a: &Item, b: &Item| match (row_of(old.rows, old.base, a), row_of(new.rows, new.base, b)) {
            (Some(x), Some(y)) => a.decor != b.decor || !same_item_content(x, y),
            _ => true,
        };
    let head = o.iter().zip(n).take_while(|(a, b)| a.key == b.key).count();
    let tail = o[head..]
        .iter()
        .rev()
        .zip(n[head..].iter().rev())
        .take_while(|(a, b)| a.key == b.key)
        .count();
    let mut plan = Plan::default();
    for i in 0..head {
        if changed(&o[i], &n[i]) {
            plan.remeasure.push(at + i);
        }
    }
    for k in 0..tail {
        let (a, b) = (o.len() - tail + k, n.len() - tail + k);
        if changed(&o[a], &n[b]) {
            plan.remeasure.push(at + b);
        }
    }
    let (old_mid, new_len) = (head..o.len() - tail, n.len() - tail - head);
    if !old_mid.is_empty() || new_len > 0 {
        plan.splice = Some((at + old_mid.start..at + old_mid.end, new_len));
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{Key, Row};
    use reno_proto::StopReason;
    use std::collections::HashSet;

    fn prose(n: u64, text: &str) -> Row {
        Row::Prose {
            key: Key(n),
            text: text.to_owned(),
        }
    }

    fn user(n: u64) -> Row {
        Row::User {
            key: Key(n),
            text: "go".to_owned(),
            steer: false,
            at_ms: None,
        }
    }

    fn done(n: u64) -> Row {
        Row::Done {
            key: Key(n),
            stop: StopReason::Completed,
            result: None,
            diagnostic: None,
            reason: None,
        }
    }

    fn worked(n: u64, inner: Vec<Row>) -> Row {
        Row::Worked {
            key: Key::fold(Key(n)),
            secs: Some(5),
            rows: inner,
        }
    }

    fn plan(old: &[Row], po: &Projection, new: &[Row], pn: &Projection) -> Plan {
        diff(
            Side {
                rows: old,
                base: 0,
                items: &po.items,
            },
            Side {
                rows: new,
                base: 0,
                items: &pn.items,
            },
            0,
        )
    }

    fn none(_: Key) -> bool {
        false
    }

    fn keys(p: &Projection) -> Vec<u64> {
        p.items.iter().map(|i| i.key.0).collect()
    }

    #[test]
    fn a_closed_fold_is_one_item_and_an_open_one_adds_its_rows_after_its_head() {
        let rows = vec![
            user(0),
            worked(2, vec![prose(2, "a"), prose(4, "b"), prose(6, "c")]),
            prose(8, "end"),
            done(9),
        ];
        let closed = project(&rows, none);
        assert_eq!(closed.len(), 4);
        let fold = Key::fold(Key(2));
        let open = project(&rows, |k| k == fold);
        assert_eq!(keys(&open), vec![0, fold.0, 2, 4, 6, 8, 9]);
        // Inside the fold: the same top-level row, and the place in it.
        assert_eq!(open.items[2].top, 1);
        assert_eq!(open.items[2].inner, Some(0));
        assert_eq!(open.items[1].inner, None);
        // Keys and top-level rows map to items; the rows after the fold moved down.
        assert_eq!(open.item_of(Key(4)), Some(3));
        assert_eq!(open.item_of(Key(8)), Some(5));
        assert_eq!(open.item_of_top(2), 5);
        assert_eq!(open.item_of_top(9), open.len());
        assert_eq!(
            open.top_of(4, rows.len()),
            1,
            "an inner item is its fold's row"
        );
        assert_eq!(open.top_of(5, rows.len()), 2);
        assert_eq!(open.top_of(99, rows.len()), rows.len());
        assert_eq!(open.row(&rows, 3), Some(&prose(4, "b")));
    }

    #[test]
    fn the_final_message_the_last_done_and_the_last_item_are_marked() {
        let rows = vec![
            user(0),
            prose(2, "x"),
            done(3),
            user(1 << 40),
            prose(5 | 1 << 40, "y"),
            done(6 | 1 << 40),
        ];
        let p = project(&rows, none);
        let d = |i: usize| p.items[i].decor;
        assert!(d(1).final_message && d(4).final_message);
        assert!(!d(0).final_message && !d(2).final_message);
        assert!(p.is_last_done(5) && !p.is_last_done(2));
        assert!(d(5).last && !d(4).last);
        // Prose rows count, in order, for the chips that hang on them.
        assert_eq!(d(1).prose_at, Some(0));
        assert_eq!(d(4).prose_at, Some(1));
        assert_eq!(p.prose_item(1), Some(4));
        assert_eq!(p.prose_item(2), None);
    }

    #[test]
    fn the_final_message_mark_agrees_with_the_rule_in_convo() {
        let rows = vec![
            user(0),
            prose(2, "a"),
            prose(4, "b"),
            done(5),
            user(1 << 40),
            prose(7 | 1 << 40, "c"),
            done(8 | 1 << 40),
            prose(9 | 1 << 40, "tail"),
        ];
        let p = project(&rows, none);
        let marked: Vec<usize> = (0..rows.len())
            .filter(|&i| p.items[i].decor.final_message)
            .collect();
        assert_eq!(marked, crate::vm::convo::final_messages(&rows));
    }

    #[test]
    fn prose_inside_a_fold_is_not_counted_for_chips() {
        let rows = vec![
            prose(2, "top"),
            worked(4, vec![prose(4, "in")]),
            prose(6, "after"),
            done(7),
        ];
        let fold = Key::fold(Key(4));
        let p = project(&rows, |k| k == fold);
        let at: Vec<_> = p.items.iter().map(|i| i.decor.prose_at).collect();
        assert_eq!(at, vec![Some(0), None, None, Some(1), None]);
    }

    #[test]
    fn an_unchanged_list_needs_nothing() {
        let rows = vec![user(0), prose(2, "a"), done(3)];
        let p = project(&rows, none);
        assert_eq!(plan(&rows, &p, &rows, &p), Plan::default());
    }

    #[test]
    fn a_grown_reply_measures_only_its_own_item() {
        let old = vec![user(0), prose(2, "a"), prose(4, "b")];
        let new = vec![user(0), prose(2, "a"), prose(4, "b and more")];
        let (po, pn) = (project(&old, none), project(&new, none));
        let plan = plan(&old, &po, &new, &pn);
        assert_eq!(plan.splice, None);
        assert_eq!(plan.remeasure, vec![2]);
    }

    #[test]
    fn a_new_row_is_an_insert_and_the_old_last_item_is_measured_again() {
        let old = vec![user(0), prose(2, "a")];
        let new = vec![user(0), prose(2, "a"), prose(4, "b")];
        let (po, pn) = (project(&old, none), project(&new, none));
        let plan = plan(&old, &po, &new, &pn);
        assert_eq!(plan.splice, Some((2..2, 1)), "an insert replaces nothing");
        // It was the last item, with the longer bottom space; now it is not.
        assert_eq!(plan.remeasure, vec![1]);
    }

    #[test]
    fn a_finished_turn_folds_in_one_range_and_keeps_the_rows_after_it() {
        let old = vec![
            user(0),
            prose(2, "a"),
            prose(4, "b"),
            prose(6, "final"),
            done(7),
        ];
        let new = vec![
            user(0),
            worked(2, vec![prose(2, "a"), prose(4, "b")]),
            prose(6, "final"),
            done(7),
        ];
        let (po, pn) = (project(&old, none), project(&new, none));
        let plan = plan(&old, &po, &new, &pn);
        assert_eq!(plan.splice, Some((1..3, 1)));
        // The Done is the same item, one place higher. The final message is measured again: it is
        // now the first prose among the top-level rows, which is where chips hang.
        assert_eq!(plan.remeasure, vec![2], "{plan:?}");
    }

    #[test]
    fn opening_a_fold_inserts_its_rows_after_the_head_and_leaves_the_head_alone() {
        let rows = vec![
            user(0),
            worked(2, vec![prose(2, "a"), prose(4, "b")]),
            prose(6, "end"),
            done(7),
        ];
        let closed = project(&rows, none);
        let fold = Key::fold(Key(2));
        let open = project(&rows, |k| k == fold);
        let opening = plan(&rows, &closed, &rows, &open);
        assert_eq!(opening.splice, Some((2..2, 2)));
        assert!(opening.remeasure.is_empty(), "{opening:?}");
        let closing = plan(&rows, &open, &rows, &closed);
        assert_eq!(closing.splice, Some((2..4, 0)));
        assert!(closing.remeasure.is_empty(), "{closing:?}");
    }

    #[test]
    fn a_fold_head_does_not_change_when_its_rows_grow_unseen() {
        let a = vec![worked(2, vec![prose(2, "a")]), prose(6, "end"), done(7)];
        let mut b = a.clone();
        if let Row::Worked { rows, .. } = &mut b[0] {
            rows[0] = prose(2, "a, longer");
        }
        let (pa, pb) = (project(&a, none), project(&b, none));
        assert_eq!(plan(&a, &pa, &b, &pb), Plan::default());
        // Open, the same change is the item's own.
        let fold = Key::fold(Key(2));
        let open: HashSet<Key> = [fold].into_iter().collect();
        let (oa, ob) = (
            project(&a, |k| open.contains(&k)),
            project(&b, |k| open.contains(&k)),
        );
        assert_eq!(plan(&a, &oa, &b, &ob).remeasure, vec![1]);
    }

    #[test]
    fn a_row_taken_out_is_a_removal() {
        let old = vec![user(0), prose(2, "a"), prose(4, "b"), done(5)];
        let new = vec![user(0), prose(4, "b"), done(5)];
        let (po, pn) = (project(&old, none), project(&new, none));
        let plan = plan(&old, &po, &new, &pn);
        assert_eq!(plan.splice, Some((1..2, 0)));
    }

    #[test]
    fn reprojecting_the_tail_keeps_the_items_before_it_and_numbers_the_rest_after_them() {
        let mut rows = vec![
            user(0),
            prose(2, "old run"),
            done(3),
            user(1 << 40),
            prose(5 | 1 << 40, "a"),
        ];
        let mut p = project(&rows, none);
        assert_eq!(p.len(), 5);
        let replaced = p.reproject(&rows, 3, none);
        assert_eq!(
            replaced.len(),
            2,
            "only the last run's items were made again"
        );
        assert_eq!(p.len(), 5);
        // The tail grows: its new item is numbered after the older run's.
        rows.push(done(6 | 1 << 40));
        let replaced = p.reproject(&rows, 3, none);
        assert_eq!(replaced.len(), 2);
        assert_eq!(p.len(), 6);
        assert_eq!(p.item_of(Key(6 | 1 << 40)), Some(5));
        assert_eq!(p.item_of(Key(2)), Some(1));
        assert!(p.items[5].decor.last && !p.items[4].decor.last);
        // The older run's Done is no longer the thread's last.
        assert!(p.is_last_done(5) && !p.is_last_done(2));
        assert_eq!(p.prose_item(0), Some(1));
        assert_eq!(p.prose_item(1), Some(4));
        assert_eq!(p.prose_item(2), None);
    }

    #[test]
    fn a_tail_plan_numbers_its_ranges_from_where_the_tail_starts() {
        let old = vec![user(0), prose(2, "a")];
        let new = vec![user(0), prose(2, "a"), prose(4, "b")];
        let (po, pn) = (project(&old, none), project(&new, none));
        let plan = diff(
            Side {
                rows: &old[1..],
                base: 1,
                items: &po.items[1..],
            },
            Side {
                rows: &new,
                base: 0,
                items: &pn.items[1..],
            },
            1,
        );
        assert_eq!(plan.splice, Some((2..2, 1)));
        assert_eq!(plan.remeasure, vec![1]);
    }
}
