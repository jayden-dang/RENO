//! ROAD-32, the sidebar draft: everything the three variants share and none of them draws. Which
//! variant is showing, what a row's menu offers (and what the real daemon hides), what the details
//! card says about a thread, and the words the sidebar uses for what is out of sight.

use reno_proto::ThreadInfo;

use super::projects::{Dot, ThreadRow};
use crate::summary;

/// The three directions under review.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Variant {
    /// Menu: everything is in one context menu; the list stays as quiet as today.
    #[default]
    A,
    /// Hover actions: one hover and one click for pin and archive; rows say branch and harness.
    B,
    /// Details card: rest on a row to see everything about the thread before you touch it.
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
            Self::A => "Menu",
            Self::B => "Hover actions",
            Self::C => "Details card",
        }
    }

    /// "A \u{2014} Menu".
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

    /// `RENO_DRAFT32=a|b|c`, either case.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "a" => Some(Self::A),
            "b" => Some(Self::B),
            "c" => Some(Self::C),
            _ => None,
        }
    }

    /// Pinned threads are listed apart from their projects (A: a "Pinned" section, C: a strip);
    /// B keeps them first in their project.
    pub(crate) fn pinned_apart(self) -> bool {
        self != Self::B
    }
}

/// What one menu entry does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Item {
    Rename,
    Pin,
    Unpin,
    Archive,
    /// Opens the Copy sub-list.
    Copy,
    CopyPath,
    CopyThreadId,
    CopySession,
    /// Asks, then closes the thread (the real `thread.close`).
    Close,
}

/// One line of the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Item {
        item: Item,
        label: &'static str,
        /// A key that does the same on the thread, drawn right-aligned.
        hint: Option<&'static str>,
        enabled: bool,
        /// Why it is off, said in words beside it.
        note: Option<&'static str>,
        danger: bool,
    },
    Divider,
}

/// What the menu needs to know about the thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Facts {
    /// The thread is known to the daemon (a row with no thread has nothing to act on).
    pub(crate) known: bool,
    /// The mocked controls exist (`Capabilities::thread_edit`).
    pub(crate) edit: bool,
    pub(crate) pinned: bool,
    pub(crate) has_path: bool,
    pub(crate) has_session: bool,
}

fn item(item: Item, label: &'static str) -> Entry {
    Entry::Item {
        item,
        label,
        hint: None,
        enabled: true,
        note: None,
        danger: false,
    }
}

fn hinted(entry: Entry, key: &'static str) -> Entry {
    match entry {
        Entry::Item {
            item,
            label,
            enabled,
            note,
            danger,
            ..
        } => Entry::Item {
            item,
            label,
            hint: Some(key),
            enabled,
            note,
            danger,
        },
        divider => divider,
    }
}

/// The row menu. A offers everything; B's "\u{2026}" holds what its two hover icons do not.
/// Without `edit` (the real daemon) Rename, Pin and Archive are not there at all.
pub(crate) fn menu(variant: Variant, facts: Facts) -> Vec<Entry> {
    if !facts.known {
        return Vec::new();
    }
    let mut out = Vec::new();
    if facts.edit {
        out.push(item(Item::Rename, "Rename"));
        if variant == Variant::A {
            out.push(hinted(
                item(
                    if facts.pinned { Item::Unpin } else { Item::Pin },
                    if facts.pinned { "Unpin" } else { "Pin" },
                ),
                "\u{2318}\u{21e7}P",
            ));
            out.push(hinted(item(Item::Archive, "Archive"), "\u{2318}\u{21e7}A"));
        }
    }
    out.push(item(Item::Copy, "Copy"));
    out.push(Entry::Divider);
    out.push(close_entry());
    out
}

/// "Close thread\u{2026}": it always asks first (a live run is said so there).
pub(crate) fn close_entry() -> Entry {
    Entry::Item {
        item: Item::Close,
        label: "Close thread\u{2026}",
        hint: None,
        enabled: true,
        note: None,
        danger: true,
    }
}

/// The Copy sub-list: a value the thread does not have yet is off, with the reason.
pub(crate) fn copy_entries(facts: Facts) -> Vec<Entry> {
    let mut path = item(Item::CopyPath, "Path");
    let mut session = item(Item::CopySession, "Harness session ID");
    for (entry, has, why) in [
        (&mut path, facts.has_path, "no worktree"),
        (&mut session, facts.has_session, "not started"),
    ] {
        if let Entry::Item { enabled, note, .. } = entry {
            *enabled = has;
            *note = (!has).then_some(why);
        }
    }
    vec![path, item(Item::CopyThreadId, "Thread ID"), session]
}

/// The status line of the details card.
pub(crate) fn status_word(row: &ThreadRow, info: Option<&ThreadInfo>) -> &'static str {
    if info.is_some_and(|i| i.status == reno_proto::ThreadStatus::Closed) {
        return "Closed";
    }
    match row.dot {
        Dot::Running => "Running",
        Dot::Waiting => row.badge.map_or("Waiting", |b| b.label()),
        Dot::New | Dot::Seen => "Finished",
        Dot::Failed => "Failed",
        Dot::Stopped => "Stopped",
    }
}

/// What the details card says, label then value, in this order.
pub(crate) fn details(
    row: &ThreadRow,
    info: Option<&ThreadInfo>,
    now_ms: u64,
) -> Vec<(&'static str, String)> {
    let ago = |at: u64| match summary::age_label(now_ms.saturating_sub(at)).as_str() {
        "just now" => "just now".to_owned(),
        age => format!("{age} ago"),
    };
    vec![
        ("Project", row.project.clone()),
        (
            "Branch",
            info.and_then(|i| i.branch.clone())
                .unwrap_or_else(|| "no worktree".to_owned()),
        ),
        ("Harness", summary::executor_name(row.executor.as_str())),
        ("Status", status_word(row, info).to_owned()),
        ("Started", ago(row.created_ms)),
        ("Last activity", ago(row.updated_ms)),
    ]
}

/// The count that says what the archive holds, in words.
pub(crate) fn archived_label(count: usize) -> String {
    format!("Archived \u{b7} {count}")
}

/// "Show 6 more", or "Show less" once the folder lists them all.
pub(crate) fn more_label(hidden: usize, expanded: bool) -> Option<String> {
    if expanded {
        Some("Show less".to_owned())
    } else {
        (hidden > 0).then(|| format!("Show {hidden} more"))
    }
}

/// The second line of a B row: "reno/thr_plan1 \u{b7} Claude Code"; an archived row says its
/// project instead of its branch, since the list is not grouped; a row with a badge has room for
/// the branch only.
pub(crate) fn subline(
    row: &ThreadRow,
    info: Option<&ThreadInfo>,
    archived: bool,
    has_badge: bool,
) -> String {
    if archived {
        return format!(
            "{} \u{b7} {}",
            row.project,
            summary::executor_name(row.executor.as_str())
        );
    }
    let branch = info
        .and_then(|i| i.branch.as_deref())
        .unwrap_or("no worktree");
    // A badge takes the room the harness would have had.
    if has_badge {
        return branch.to_owned();
    }
    format!(
        "{branch} \u{b7} {}",
        summary::executor_name(row.executor.as_str())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            known: true,
            edit: true,
            pinned: false,
            has_path: true,
            has_session: true,
        }
    }

    fn labels(entries: &[Entry]) -> Vec<&'static str> {
        entries
            .iter()
            .filter_map(|e| match e {
                Entry::Item { label, .. } => Some(*label),
                Entry::Divider => None,
            })
            .collect()
    }

    #[test]
    fn the_variants_cycle_and_parse() {
        assert_eq!(Variant::A.next(), Variant::B);
        assert_eq!(Variant::C.next(), Variant::A);
        assert_eq!(Variant::A.prev(), Variant::C);
        assert_eq!(Variant::parse(" B "), Some(Variant::B));
        assert_eq!(Variant::parse("d"), None);
        assert_eq!(Variant::B.label(), "B \u{2014} Hover actions");
        assert!(
            Variant::A.pinned_apart() && Variant::C.pinned_apart() && !Variant::B.pinned_apart()
        );
    }

    #[test]
    fn menu_a_has_everything_and_the_close_is_last_and_dangerous() {
        let entries = menu(Variant::A, facts());
        assert_eq!(
            labels(&entries),
            ["Rename", "Pin", "Archive", "Copy", "Close thread\u{2026}"]
        );
        assert_eq!(entries[entries.len() - 2], Entry::Divider);
        let Some(Entry::Item {
            danger, enabled, ..
        }) = entries.last()
        else {
            panic!("no last item");
        };
        assert!(*danger && *enabled);
        let pinned = menu(
            Variant::A,
            Facts {
                pinned: true,
                ..facts()
            },
        );
        assert_eq!(labels(&pinned)[1], "Unpin");
        // Every hint is a key that works on the thread.
        let hints: Vec<_> = entries
            .iter()
            .filter_map(|e| match e {
                Entry::Item { hint, .. } => *hint,
                Entry::Divider => None,
            })
            .collect();
        assert_eq!(hints, ["\u{2318}\u{21e7}P", "\u{2318}\u{21e7}A"]);
    }

    #[test]
    fn menu_b_leaves_pin_and_archive_to_the_hover_icons() {
        assert_eq!(
            labels(&menu(Variant::B, facts())),
            ["Rename", "Copy", "Close thread\u{2026}"]
        );
    }

    #[test]
    fn the_real_daemon_hides_the_mocked_controls_and_keeps_the_real_ones() {
        let real = Facts {
            edit: false,
            ..facts()
        };
        for variant in [Variant::A, Variant::B, Variant::C] {
            assert_eq!(
                labels(&menu(variant, real)),
                ["Copy", "Close thread\u{2026}"],
                "{variant:?}"
            );
        }
        assert!(
            menu(
                Variant::A,
                Facts {
                    known: false,
                    ..facts()
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn copy_says_why_a_value_is_missing() {
        let all = copy_entries(facts());
        assert_eq!(labels(&all), ["Path", "Thread ID", "Harness session ID"]);
        assert!(
            all.iter()
                .all(|e| matches!(e, Entry::Item { enabled: true, .. }))
        );
        let none = copy_entries(Facts {
            has_path: false,
            has_session: false,
            ..facts()
        });
        let notes: Vec<_> = none
            .iter()
            .filter_map(|e| match e {
                Entry::Item { note, .. } => *note,
                Entry::Divider => None,
            })
            .collect();
        assert_eq!(notes, ["no worktree", "not started"]);
    }

    #[test]
    fn what_is_out_of_sight_is_said_in_words() {
        assert_eq!(archived_label(4), "Archived \u{b7} 4");
        assert_eq!(more_label(6, false).as_deref(), Some("Show 6 more"));
        assert_eq!(more_label(0, false), None);
        assert_eq!(more_label(0, true).as_deref(), Some("Show less"));
    }
}
