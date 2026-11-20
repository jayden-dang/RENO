//! `@` `/` `$` completions in the composer: when a list is open, what it holds, and the keys
//! that drive it. The words and the grouping are `vm::completions`; the data is the store's (the
//! harness's commands and a new thread's project files are the mocked completions contract; a live
//! run's files are the real `fs.search_files`). The list itself is drawn by `completions/list.rs`.
//!
//! Completions never take the keyboard: whether one is open is derived from the text and the
//! caret on every change, Up/Down/Enter/Tab/Esc are handled by the box itself, and a click on a
//! row inserts without moving focus.

mod list;

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{App, Bounds, Context, Pixels, Task, Window};
use gpui_tokio::Tokio;
use reno_proto::{FsSearchFilesParams, RunId};

use super::{Composer, Mode};
use crate::summary;
use crate::ui::text_area::Decor;
use crate::ui::theme::Theme;
use crate::vm::completions::{
    Active, EntryKind, Listing, Trigger, command_listing, file_entries, find_active, mentions,
    skill_listing, window_start,
};

/// How many rows the list shows at once.
const ROWS: usize = 8;

/// The rows asked of a search.
const FILES_ASKED: u32 = 24;

/// Where something was last painted, for the popover that hangs from it.
pub(super) type Anchor = Rc<Cell<Option<Bounds<Pixels>>>>;

/// An open list of completions.
pub(super) struct Completion {
    pub(super) active: Active,
    pub(super) listing: Listing,
    pub(super) selected: usize,
    pub(super) top: usize,
}

/// What the composer keeps about completions: view state only. The data is the store's.
pub(super) struct State {
    pub(super) complete: Option<Completion>,
    /// Esc closed the list for the token that starts here; it stays closed until the token goes.
    pub(super) dismissed: Option<(usize, Trigger)>,
    file_ticket: u64,
    file_task: Option<Task<()>>,
    /// Where the box was last painted, for the list that docks to it.
    pub(super) box_bounds: Anchor,
}

impl State {
    pub(super) fn new() -> Self {
        Self {
            complete: None,
            dismissed: None,
            file_ticket: 0,
            file_task: None,
            box_bounds: Rc::default(),
        }
    }
}

impl Composer {
    /// The labels of the open list's rows (tests).
    #[cfg(test)]
    pub(crate) fn completion_labels(&self) -> Option<Vec<String>> {
        self.completions
            .complete
            .as_ref()
            .map(|c| c.listing.entries.iter().map(|e| e.label.clone()).collect())
    }

    /// The groups of the open list's rows, in order, without repeats (tests).
    #[cfg(test)]
    pub(crate) fn completion_groups(&self) -> Vec<String> {
        let mut groups: Vec<String> = Vec::new();
        for entry in self
            .completions
            .complete
            .iter()
            .flat_map(|c| &c.listing.entries)
        {
            if groups.last() != Some(&entry.group) {
                groups.push(entry.group.clone());
            }
        }
        groups
    }

    /// The failure line of the open list (tests).
    #[cfg(test)]
    pub(crate) fn completion_failure_line(&self) -> Option<String> {
        self.completions
            .complete
            .as_ref()
            .and_then(|c| c.listing.failure.clone())
    }

    /// Enter on the open list (tests).
    #[cfg(test)]
    pub(crate) fn completion_accept_for_test(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.completion_accept(None, window, cx)
    }

    /// Esc on the open list (tests).
    #[cfg(test)]
    pub(crate) fn completion_dismiss_for_test(&mut self, cx: &mut Context<Self>) -> bool {
        self.completion_dismiss(cx)
    }

    /// The harness the next message goes to: the run's, else the one picked, else Claude Code.
    fn completion_harness(&self, cx: &App) -> (String, String) {
        let id = match &self.mode {
            Mode::Steer(run) => self
                .store
                .read(cx)
                .state()
                .run(run)
                .map(|r| r.executor.as_str().to_owned()),
            _ => self.pick.as_ref().map(|p| p.executor.as_str().to_owned()),
        }
        .unwrap_or_else(|| "claude-code".to_owned());
        let name = summary::executor_name(&id);
        (id, name)
    }

    // ---- what is open follows the text -------------------------------------------------------

    /// Called after every change of the text or the caret.
    pub(super) fn completion_refresh(&mut self, cx: &mut Context<Self>) {
        let active = (self.mode != Mode::Reason
            && self.model.marked().is_none()
            && self.model.selection().is_empty())
        .then(|| find_active(self.model.text(), self.model.caret()))
        .flatten();
        let Some(active) = active else {
            self.completions.complete = None;
            self.completions.dismissed = None;
            self.completions.file_task = None;
            return;
        };
        if self.completions.dismissed == Some((active.start, active.trigger)) {
            self.completions.complete = None;
            return;
        }
        let before = self.completions.complete.take();
        let same = before
            .as_ref()
            .is_some_and(|c| c.active.trigger == active.trigger && c.active.start == active.start);
        let (selected, top) = match (&before, same) {
            (Some(c), true) if c.active.query == active.query => (c.selected, c.top),
            _ => (0, 0),
        };
        let listing = match active.trigger {
            Trigger::Slash | Trigger::Dollar => self.completion_command_listing(&active, cx),
            Trigger::At => {
                self.completion_file_listing(&active, before.as_ref().filter(|_| same), cx)
            }
        };
        self.completions.complete = listing.map(|listing| Completion {
            active,
            listing,
            selected,
            top,
        });
    }

    fn completion_command_listing(&self, active: &Active, cx: &App) -> Option<Listing> {
        let (id, name) = self.completion_harness(cx);
        let harness = self.store.read(cx).harness_commands(&id);
        let listing = if active.trigger == Trigger::Slash {
            command_listing(
                &active.query,
                &name,
                harness.as_ref(),
                self.live_run(cx).is_some(),
            )
        } else {
            skill_listing(&active.query, &name, harness.as_ref())
        };
        // A `$` with no contract has nothing to say, and says nothing.
        (!listing.entries.is_empty() || listing.failure.is_some()).then_some(listing)
    }

    /// `@`: a new thread asks the mocked project search; a live run asks the real
    /// `fs.search_files`, and keeps the last answer on show until the next one comes.
    fn completion_file_listing(
        &mut self,
        active: &Active,
        before: Option<&Completion>,
        cx: &mut Context<Self>,
    ) -> Option<Listing> {
        match self.mode.clone() {
            Mode::Steer(run) => {
                self.completion_search_run(run, &active.query, cx);
                let mut listing = before.map(|c| c.listing.clone()).unwrap_or_default();
                listing.pending = true;
                Some(listing)
            }
            Mode::Home => {
                let chosen = self.place_view(cx)?;
                let found = self.store.read(cx).new_thread_files(
                    &chosen.project.name,
                    &active.query,
                    FILES_ASKED as usize,
                )?;
                Some(Listing {
                    entries: file_entries(found, &format!("in {}", chosen.project.name)),
                    ..Listing::default()
                })
            }
            Mode::Reason => None,
        }
    }

    fn completion_search_run(&mut self, run: RunId, query: &str, cx: &mut Context<Self>) {
        self.completions.file_ticket += 1;
        let ticket = self.completions.file_ticket;
        let Some(client) = self.store.read(cx).client() else {
            self.completions.file_task = None;
            return;
        };
        let params = FsSearchFilesParams {
            run_id: run,
            query: query.to_owned(),
            limit: Some(FILES_ASKED),
        };
        let search = Tokio::spawn(cx, async move { client.fs_search_files(params).await });
        self.completions.file_task = Some(cx.spawn(async move |this, cx| {
            let found = match search.await {
                Ok(Ok(found)) => Ok(found.files.into_iter().map(|f| f.path).collect()),
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            this.update(cx, |this, cx| {
                this.completion_files_found(ticket, found, cx)
            })
            .ok();
        }));
    }

    /// A real search answered. A stale answer (the query moved on) is dropped.
    fn completion_files_found(
        &mut self,
        ticket: u64,
        found: Result<Vec<String>, String>,
        cx: &mut Context<Self>,
    ) {
        if ticket != self.completions.file_ticket {
            return;
        }
        let Some(complete) = self.completions.complete.as_mut() else {
            return;
        };
        if complete.active.trigger != Trigger::At {
            return;
        }
        complete.listing = match found {
            Ok(paths) => Listing {
                entries: file_entries(
                    paths
                        .into_iter()
                        .map(|path| reno_client::completions::ProjectFile {
                            path,
                            head: Vec::new(),
                        }),
                    "in this run's worktree",
                ),
                ..Listing::default()
            },
            Err(_) => Listing {
                failure: Some("This run's files could not be searched".to_owned()),
                ..Listing::default()
            },
        };
        complete.selected = complete
            .selected
            .min(complete.listing.entries.len().saturating_sub(1));
        cx.notify();
    }

    /// Asks again after a failure ("Try again").
    pub(super) fn completion_retry(&mut self, cx: &mut Context<Self>) {
        self.completion_refresh(cx);
        cx.notify();
    }

    // ---- the keyboard ------------------------------------------------------------------------

    /// Up and Down move over the rows while a list is open and has any. Returns whether it did.
    pub(super) fn completion_nav(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(complete) = self.completions.complete.as_mut() else {
            return false;
        };
        let n = complete.listing.entries.len();
        if n == 0 || self.model.marked().is_some() {
            return false;
        }
        complete.selected = (complete.selected as isize + delta).rem_euclid(n as isize) as usize;
        complete.top = window_start(complete.selected, complete.top, ROWS);
        cx.notify();
        true
    }

    /// Esc closes the list and keeps the text. Returns whether a list was open.
    pub(super) fn completion_dismiss(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(complete) = self.completions.complete.take() else {
            return false;
        };
        self.completions.dismissed = Some((complete.active.start, complete.active.trigger));
        self.completions.file_task = None;
        cx.notify();
        true
    }

    /// Enter and Tab (and a click): put the row in the text. Returns whether one went in.
    pub(super) fn completion_accept(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.model.marked().is_some() {
            return false;
        }
        let Some(complete) = &self.completions.complete else {
            return false;
        };
        let Some(entry) = complete
            .listing
            .entries
            .get(index.unwrap_or(complete.selected))
            .cloned()
        else {
            return false;
        };
        let active = complete.active.clone();
        self.completions.complete = None;
        let text = self.model.text();
        // A space already follows the token: do not add another.
        let mut insert = entry.insert.clone();
        if text[active.end..].starts_with(char::is_whitespace) {
            insert = insert.trim_end().to_owned();
        }
        let app_command = entry.kind == EntryKind::App;
        self.completion_replace(
            active.start..active.end,
            if app_command { "" } else { &insert },
        );
        self.text_changed(window, cx);
        if app_command {
            self.completion_run_app_command(entry.label.trim_start_matches('/'), window, cx);
        }
        true
    }

    fn completion_replace(&mut self, range: Range<usize>, with: &str) {
        self.model.move_to(range.start, false);
        self.model.move_to(range.end, true);
        self.model.insert(with);
    }

    /// The app's own commands: real, handled here, never sent to the harness.
    fn completion_run_app_command(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::actions::{OpenPalette, OpenSettings};
        match name {
            "model" => self.open_picker(window, cx),
            "settings" => window.dispatch_action(Box::new(OpenSettings), cx),
            "resume" => window.dispatch_action(Box::new(OpenPalette), cx),
            "stop" => {
                if let Mode::Steer(run) = self.mode.clone() {
                    self.store.update(cx, |store, cx| store.cancel_run(run, cx));
                }
            }
            _ => {}
        }
    }

    // ---- what the text looks like ------------------------------------------------------------

    /// The mentions the text area paints as chips: a neutral wash, the trigger character in the
    /// accent. The token being typed has only its trigger character in the accent.
    pub(crate) fn decor(&self, theme: &Theme) -> Decor {
        if self.mode == Mode::Reason {
            return Decor::default();
        }
        let typing = self
            .completions
            .complete
            .as_ref()
            .map(|c| c.active.start..c.active.end);
        let mut spans = Vec::new();
        for range in mentions(self.model.text(), typing.as_ref()) {
            spans.push((
                range.start..range.start + 1,
                theme.accent,
                Some(theme.row_selected()),
            ));
            spans.push((
                range.start + 1..range.end,
                theme.text,
                Some(theme.row_selected()),
            ));
        }
        if let Some(typing) = typing {
            spans.push((typing.start..typing.start + 1, theme.accent, None));
        }
        Decor { spans }
    }

    /// The menus that hang from the left of what they change: the box and the sentence's words.
    pub(super) fn completion_hangs_left(&self, at: &Anchor) -> bool {
        Rc::ptr_eq(at, &self.completions.box_bounds)
            || self.places.triggers.iter().any(|t| Rc::ptr_eq(at, t))
    }
}
