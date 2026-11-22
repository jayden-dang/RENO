//! The run side panel as a pure model: which tabs are open, which one shows, what a close falls
//! back to, and what can be reopened. No window and no GPUI, so every rule is a unit test; the
//! strip and the surfaces only draw and drive this.

pub(crate) mod layout;
pub(crate) mod persist;
pub(crate) mod saved;
pub(crate) mod surface;
pub(crate) mod view;

use serde::{Deserialize, Serialize};
use surface::{Descriptor, Surface};

/// How many closed tabs ⌘⇧T can bring back.
pub(crate) const CLOSED_CAP: usize = 10;

/// What a panel belongs to. A run id today; a thread id once threads land (D10).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct PanelKey(pub(crate) String);

impl PanelKey {
    #[cfg(test)]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where a thread was left: the entry at the top of the view (its row key) and how far into it,
/// in whole points, so that [`Descriptors`] stays `Eq`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Place {
    pub(crate) key: u64,
    pub(crate) offset_pt: i32,
}

/// What the panel needs to know about a run to choose its first tabs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RunFacts {
    /// The run has a workspace to review.
    pub(crate) has_workspace: bool,
    /// The run changed something.
    pub(crate) has_changes: bool,
}

/// The tabs of one panel.
///
/// Invariants: `Session` is always `tabs[0]`; tabs are unique; `active` is a tab or `Home`
/// (which shows with no tab, so it need not be one); `mru` holds open tabs other than `active`,
/// most recent first; `closed` holds at most [`CLOSED_CAP`] tabs, most recent last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PanelState {
    tabs: Vec<Surface>,
    active: Surface,
    mru: Vec<Surface>,
    closed: Vec<Surface>,
    /// The transcript's reading place, kept with the tabs so that both are saved and carried
    /// over together.
    place: Option<Place>,
}

impl Default for PanelState {
    fn default() -> Self {
        Self {
            tabs: vec![Surface::Session],
            active: Surface::Session,
            mru: Vec::new(),
            closed: Vec::new(),
            place: None,
        }
    }
}

/// A panel as saved (D8): descriptors and the active one. Small, versioned by its container.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Descriptors {
    pub(crate) tabs: Vec<Descriptor>,
    pub(crate) active: Option<Descriptor>,
    #[serde(default)]
    pub(crate) closed: Vec<Descriptor>,
    /// Absent in a file from before places were saved, and not written when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) place: Option<Place>,
}

impl Descriptors {
    /// Whether any tab was saved: a thread left with only a place has no panel to restore.
    pub(crate) fn has_panel(&self) -> bool {
        !self.tabs.is_empty() || self.active.is_some() || !self.closed.is_empty()
    }
}

impl PanelState {
    /// The tabs a run starts with: Session, plus Review when it has a workspace. Review shows
    /// when the run changed something, otherwise Session does.
    pub(crate) fn reset_for(facts: RunFacts) -> Self {
        let mut state = Self::default();
        if facts.has_workspace {
            state.tabs.push(Surface::Review);
            if facts.has_changes {
                state.active = Surface::Review;
                state.mru.push(Surface::Session);
            }
        }
        state
    }

    pub(crate) fn tabs(&self) -> &[Surface] {
        &self.tabs
    }

    pub(crate) fn active(&self) -> &Surface {
        &self.active
    }

    pub(crate) fn place(&self) -> Option<Place> {
        self.place
    }

    pub(crate) fn set_place(&mut self, place: Option<Place>) {
        self.place = place;
    }

    #[cfg(test)]
    pub(crate) fn is_open(&self, surface: &Surface) -> bool {
        self.tabs.contains(surface)
    }

    /// Show `surface`, adding a tab if it has none. A kind opens once; a file is the same tab
    /// for the same path.
    pub(crate) fn open(&mut self, surface: Surface) {
        // Open now, so no longer "recently closed".
        self.closed.retain(|s| *s != surface);
        if !self.tabs.contains(&surface) {
            self.tabs.push(surface.clone());
        }
        self.activate(&surface);
    }

    /// Show an open tab (or the home). Returns false when there is no such tab.
    pub(crate) fn activate(&mut self, surface: &Surface) -> bool {
        if *surface != Surface::Home && !self.tabs.contains(surface) {
            return false;
        }
        if self.active == *surface {
            return true;
        }
        let previous = std::mem::replace(&mut self.active, surface.clone());
        self.mru.retain(|s| s != surface && *s != previous);
        if self.tabs.contains(&previous) {
            self.mru.insert(0, previous);
        }
        true
    }

    /// Close a tab. Session cannot be closed. Closing the shown tab falls back to the most
    /// recently used one, then to the home. Returns false when nothing closed.
    pub(crate) fn close(&mut self, surface: &Surface) -> bool {
        if !surface.closable() {
            return false;
        }
        let Some(at) = self.tabs.iter().position(|s| s == surface) else {
            return false;
        };
        self.tabs.remove(at);
        self.mru.retain(|s| s != surface);
        if *surface != Surface::Home {
            self.closed.retain(|s| s != surface);
            self.closed.push(surface.clone());
            if self.closed.len() > CLOSED_CAP {
                self.closed.remove(0);
            }
        }
        if self.active == *surface {
            self.active = self.mru.first().cloned().unwrap_or(Surface::Home);
            let active = self.active.clone();
            self.mru.retain(|s| *s != active);
        }
        true
    }

    /// Move the tab at `from` to `to` (a drag). Session stays first, so neither index may be 0.
    /// Returns false for an out-of-range or pinned index.
    pub(crate) fn reorder(&mut self, from: usize, to: usize) -> bool {
        let len = self.tabs.len();
        if from == 0 || to == 0 || from >= len || to >= len {
            return false;
        }
        if from != to {
            let tab = self.tabs.remove(from);
            self.tabs.insert(to, tab);
        }
        true
    }

    /// ⌃Tab / ⌃⇧Tab: show the next or previous tab, wrapping. From the home (no tab) it goes
    /// to the first or the last.
    pub(crate) fn cycle(&mut self, forward: bool) {
        let len = self.tabs.len();
        if len == 0 {
            return;
        }
        let next = match (self.tabs.iter().position(|s| *s == self.active), forward) {
            (Some(at), true) => (at + 1) % len,
            (Some(at), false) => (at + len - 1) % len,
            (None, true) => 0,
            (None, false) => len - 1,
        };
        let target = self.tabs[next].clone();
        self.activate(&target);
    }

    /// ⌘⇧T: open the most recently closed tab that is not open now.
    pub(crate) fn reopen_closed(&mut self) -> Option<Surface> {
        while let Some(surface) = self.closed.pop() {
            if !self.tabs.contains(&surface) {
                self.open(surface.clone());
                return Some(surface);
            }
        }
        None
    }

    /// The last `n` closed tabs, most recent first.
    pub(crate) fn recently_closed(&self, n: usize) -> Vec<&Surface> {
        self.closed.iter().rev().take(n).collect()
    }

    /// The saved form.
    pub(crate) fn snapshot(&self) -> Descriptors {
        Descriptors {
            tabs: self.tabs.iter().map(Surface::descriptor).collect(),
            active: Some(self.active.descriptor()),
            closed: self.closed.iter().map(Surface::descriptor).collect(),
            place: self.place,
        }
    }

    /// Rebuild from the saved form. Descriptors this build cannot read are dropped; Session is
    /// put back if missing; an active tab that is gone becomes Session.
    pub(crate) fn restore(saved: &Descriptors) -> Self {
        let mut tabs = vec![Surface::Session];
        for surface in saved.tabs.iter().filter_map(Descriptor::surface) {
            if !tabs.contains(&surface) {
                tabs.push(surface);
            }
        }
        let active = saved
            .active
            .as_ref()
            .and_then(Descriptor::surface)
            .filter(|s| *s == Surface::Home || tabs.contains(s))
            .unwrap_or(Surface::Session);
        let mru = tabs.iter().filter(|s| **s != active).cloned().collect();
        let mut closed: Vec<Surface> = Vec::new();
        for surface in saved.closed.iter().filter_map(Descriptor::surface) {
            if surface != Surface::Home && surface != Surface::Session {
                closed.retain(|s| *s != surface);
                closed.push(surface);
            }
        }
        let excess = closed.len().saturating_sub(CLOSED_CAP);
        closed.drain(..excess);
        Self {
            tabs,
            active,
            mru,
            closed,
            place: saved.place,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::surface::Kind;
    use super::*;

    fn file(p: &str) -> Surface {
        Surface::File(p.into())
    }

    fn state_with(files: &[&str]) -> PanelState {
        let mut s = PanelState::reset_for(RunFacts::default());
        for f in files {
            s.open(file(f));
        }
        s
    }

    #[test]
    fn panel_key_serializes_as_a_plain_string() {
        let k = PanelKey("run_a1".into());
        assert_eq!(serde_json::to_string(&k).expect("json"), "\"run_a1\"");
        assert_eq!(k.as_str(), "run_a1");
    }

    #[test]
    fn reset_without_a_workspace_is_session_only() {
        let s = PanelState::reset_for(RunFacts::default());
        assert_eq!(s.tabs(), [Surface::Session]);
        assert_eq!(s.active(), &Surface::Session);
    }

    #[test]
    fn reset_with_a_workspace_adds_review_and_shows_session() {
        let s = PanelState::reset_for(RunFacts {
            has_workspace: true,
            has_changes: false,
        });
        assert_eq!(s.tabs(), [Surface::Session, Surface::Review]);
        assert_eq!(s.active(), &Surface::Session);
    }

    #[test]
    fn reset_with_changes_shows_review() {
        let s = PanelState::reset_for(RunFacts {
            has_workspace: true,
            has_changes: true,
        });
        assert_eq!(s.active(), &Surface::Review);
    }

    #[test]
    fn changes_without_a_workspace_do_not_open_review() {
        let s = PanelState::reset_for(RunFacts {
            has_workspace: false,
            has_changes: true,
        });
        assert_eq!(s.tabs(), [Surface::Session]);
        assert_eq!(s.active(), &Surface::Session);
    }

    #[test]
    fn open_dedupes_and_activates() {
        let mut s = state_with(&["a.rs"]);
        s.open(file("b.rs"));
        s.open(file("a.rs"));
        assert_eq!(s.tabs(), [Surface::Session, file("a.rs"), file("b.rs")]);
        assert_eq!(s.active(), &file("a.rs"));
        s.open(Surface::Home);
        s.open(Surface::Home);
        assert_eq!(s.tabs().iter().filter(|t| **t == Surface::Home).count(), 1);
    }

    #[test]
    fn activate_unknown_tab_is_refused() {
        let mut s = state_with(&[]);
        assert!(!s.activate(&Surface::Review));
        assert!(s.activate(&Surface::Home));
        assert_eq!(s.active(), &Surface::Home);
    }

    #[test]
    fn session_cannot_close() {
        let mut s = state_with(&["a.rs"]);
        assert!(!s.close(&Surface::Session));
        assert!(s.is_open(&Surface::Session));
        assert!(s.recently_closed(3).is_empty());
    }

    #[test]
    fn closing_a_background_tab_keeps_the_active_one() {
        let mut s = state_with(&["a.rs", "b.rs"]);
        assert!(s.close(&file("a.rs")));
        assert_eq!(s.active(), &file("b.rs"));
        assert!(!s.close(&file("a.rs")));
    }

    #[test]
    fn closing_the_active_tab_falls_back_to_mru() {
        let mut s = state_with(&["a.rs", "b.rs", "c.rs"]);
        // Order of use: Session, a, b, c. Visit a, then c.
        s.activate(&file("a.rs"));
        s.activate(&file("c.rs"));
        assert!(s.close(&file("c.rs")));
        assert_eq!(s.active(), &file("a.rs"));
        assert!(s.close(&file("a.rs")));
        assert_eq!(s.active(), &file("b.rs"));
    }

    #[test]
    fn mru_never_returns_a_closed_tab() {
        let mut s = state_with(&["a.rs", "b.rs"]);
        s.activate(&file("a.rs"));
        s.close(&file("b.rs"));
        s.close(&file("a.rs"));
        assert_eq!(s.active(), &Surface::Session);
    }

    #[test]
    fn closing_with_empty_mru_shows_home() {
        let mut s = PanelState::restore(&Descriptors {
            tabs: vec![file("a.rs").descriptor()],
            active: Some(file("a.rs").descriptor()),
            closed: vec![],
            place: None,
        });
        // Session is open but a restored panel's mru order is by tab order; drop it to model an
        // exhausted mru.
        s.mru.clear();
        s.close(&file("a.rs"));
        assert_eq!(s.active(), &Surface::Home);
        // The home is shown without being a tab.
        assert!(!s.is_open(&Surface::Home));
        assert_eq!(s.tabs(), [Surface::Session]);
    }

    #[test]
    fn closing_everything_closable_leaves_session_or_home_never_nothing() {
        let mut s = state_with(&["a.rs", "b.rs"]);
        s.close(&file("a.rs"));
        s.close(&file("b.rs"));
        assert_eq!(s.tabs(), [Surface::Session]);
        assert_eq!(s.active(), &Surface::Session);
    }

    #[test]
    fn closing_the_home_tab_works_and_is_not_remembered() {
        let mut s = state_with(&[]);
        s.open(Surface::Home);
        assert!(s.close(&Surface::Home));
        assert_eq!(s.active(), &Surface::Session);
        assert!(s.recently_closed(3).is_empty());
    }

    #[test]
    fn reorder_moves_and_respects_bounds() {
        let mut s = state_with(&["a.rs", "b.rs", "c.rs"]);
        assert!(s.reorder(1, 3));
        assert_eq!(
            s.tabs(),
            [Surface::Session, file("b.rs"), file("c.rs"), file("a.rs")]
        );
        assert!(s.reorder(3, 1));
        assert_eq!(s.tabs()[1], file("a.rs"));
        assert!(s.reorder(2, 2));
        assert!(!s.reorder(1, 4), "to out of range");
        assert!(!s.reorder(4, 1), "from out of range");
        assert!(!s.reorder(0, 2), "session is pinned");
        assert!(!s.reorder(2, 0), "nothing goes before session");
        assert_eq!(s.tabs()[0], Surface::Session);
        assert_eq!(s.tabs().len(), 4);
    }

    #[test]
    fn reorder_does_not_change_the_active_tab() {
        let mut s = state_with(&["a.rs", "b.rs"]);
        s.reorder(1, 2);
        assert_eq!(s.active(), &file("b.rs"));
    }

    #[test]
    fn cycle_wraps_both_ways() {
        let mut s = state_with(&["a.rs", "b.rs"]);
        s.activate(&Surface::Session);
        s.cycle(false);
        assert_eq!(s.active(), &file("b.rs"));
        s.cycle(true);
        assert_eq!(s.active(), &Surface::Session);
        s.cycle(true);
        assert_eq!(s.active(), &file("a.rs"));
    }

    #[test]
    fn cycle_from_home_goes_to_the_ends() {
        let mut s = state_with(&["a.rs"]);
        s.activate(&Surface::Home);
        s.cycle(true);
        assert_eq!(s.active(), &Surface::Session);
        s.activate(&Surface::Home);
        s.cycle(false);
        assert_eq!(s.active(), &file("a.rs"));
    }

    #[test]
    fn cycle_with_one_tab_stays() {
        let mut s = state_with(&[]);
        s.cycle(true);
        s.cycle(false);
        assert_eq!(s.active(), &Surface::Session);
    }

    #[test]
    fn reopen_brings_back_the_latest_closed_tab_active() {
        let mut s = state_with(&["a.rs", "b.rs"]);
        s.close(&file("a.rs"));
        s.close(&file("b.rs"));
        assert_eq!(s.recently_closed(3), [&file("b.rs"), &file("a.rs")]);
        assert_eq!(s.reopen_closed(), Some(file("b.rs")));
        assert_eq!(s.active(), &file("b.rs"));
        assert_eq!(s.reopen_closed(), Some(file("a.rs")));
        assert_eq!(s.reopen_closed(), None);
    }

    #[test]
    fn opening_a_closed_tab_takes_it_off_the_recently_closed_list() {
        let mut s = state_with(&["a.rs", "b.rs"]);
        s.close(&file("a.rs"));
        s.close(&file("b.rs"));
        s.open(file("a.rs"));
        assert_eq!(s.recently_closed(3), [&file("b.rs")]);
    }

    #[test]
    fn reopen_skips_tabs_already_open() {
        let mut s = state_with(&["a.rs"]);
        s.close(&file("a.rs"));
        s.open(file("a.rs"));
        assert_eq!(s.reopen_closed(), None);
        assert!(s.recently_closed(3).is_empty());
    }

    #[test]
    fn closing_the_same_tab_twice_lists_it_once() {
        let mut s = state_with(&["a.rs"]);
        s.close(&file("a.rs"));
        s.open(file("a.rs"));
        s.close(&file("a.rs"));
        assert_eq!(s.recently_closed(10), [&file("a.rs")]);
    }

    #[test]
    fn closed_stack_is_capped_dropping_the_oldest() {
        let mut s = state_with(&[]);
        for i in 0..CLOSED_CAP + 3 {
            let f = file(&format!("f{i}.rs"));
            s.open(f.clone());
            s.close(&f);
        }
        let all = s.recently_closed(100);
        assert_eq!(all.len(), CLOSED_CAP);
        assert_eq!(all[0], &file(&format!("f{}.rs", CLOSED_CAP + 2)));
        assert_eq!(all[CLOSED_CAP - 1], &file("f3.rs"));
    }

    #[test]
    fn recently_closed_takes_n() {
        let mut s = state_with(&["a.rs", "b.rs", "c.rs", "d.rs"]);
        for f in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            s.close(&file(f));
        }
        assert_eq!(
            s.recently_closed(3),
            [&file("d.rs"), &file("c.rs"), &file("b.rs")]
        );
        assert_eq!(s.recently_closed(0).len(), 0);
    }

    #[test]
    fn snapshot_restore_round_trips() {
        let mut s = state_with(&["a.rs", "b.rs"]);
        s.open(Surface::Review);
        s.close(&file("a.rs"));
        let json = serde_json::to_string(&s.snapshot()).expect("json");
        let back: Descriptors = serde_json::from_str(&json).expect("parse");
        let r = PanelState::restore(&back);
        assert_eq!(r.tabs(), s.tabs());
        assert_eq!(r.active(), s.active());
        assert_eq!(r.recently_closed(10), s.recently_closed(10));
    }

    #[test]
    fn restore_drops_unknown_kinds_and_repairs_session() {
        let saved: Descriptors = serde_json::from_str(
            r#"{"tabs":[{"kind":"browser","key":"u"},{"kind":"file","key":"a.rs"},
                {"kind":"file","key":"a.rs"}],
               "active":{"kind":"browser","key":"u"}}"#,
        )
        .expect("parse");
        let r = PanelState::restore(&saved);
        assert_eq!(r.tabs(), [Surface::Session, file("a.rs")]);
        assert_eq!(
            r.active(),
            &Surface::Session,
            "gone active falls to Session"
        );
        assert!(r.recently_closed(3).is_empty());
    }

    #[test]
    fn restore_keeps_a_home_active_and_ignores_an_unopened_active() {
        let home = Descriptors {
            tabs: vec![],
            active: Some(Surface::Home.descriptor()),
            closed: vec![],
            place: None,
        };
        assert_eq!(PanelState::restore(&home).active(), &Surface::Home);
        let stray = Descriptors {
            tabs: vec![],
            active: Some(file("x.rs").descriptor()),
            closed: vec![],
            place: None,
        };
        assert_eq!(PanelState::restore(&stray).active(), &Surface::Session);
    }

    #[test]
    fn restore_caps_and_cleans_the_closed_stack() {
        let closed = (0..CLOSED_CAP + 5)
            .map(|i| file(&format!("f{i}")).descriptor())
            .chain([
                Descriptor {
                    kind: Kind::Unknown,
                    key: String::new(),
                },
                Surface::Session.descriptor(),
            ])
            .collect();
        let r = PanelState::restore(&Descriptors {
            tabs: vec![],
            active: None,
            closed,
            place: None,
        });
        assert_eq!(r.recently_closed(100).len(), CLOSED_CAP);
        assert_eq!(
            r.recently_closed(1),
            [&file(&format!("f{}", CLOSED_CAP + 4))]
        );
    }

    #[test]
    fn restored_panel_closes_back_to_a_saved_tab() {
        let mut r = PanelState::restore(&Descriptors {
            tabs: vec![file("a.rs").descriptor()],
            active: Some(file("a.rs").descriptor()),
            closed: vec![],
            place: None,
        });
        r.close(&file("a.rs"));
        assert_eq!(r.active(), &Surface::Session);
    }
}
