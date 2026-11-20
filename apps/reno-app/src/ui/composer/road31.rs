//! ROAD-31 draft: the composer's completions (`@` files, `/` commands, `$` skills), the message
//! queue and the place chooser. One behaviour layer drives all three variants: this file owns the
//! state, the keyboard and the effects; `complete`, `queue` and `place` draw them, each variant
//! in its own way. The words come from `vm::draft31`; the data from `Store` (the mocked
//! contracts, and the real `fs.search_files` for a live run's files). Nothing here runs unless
//! `--fake --draft31` installed the switch (`ui::draft31::current`), so the composer is exactly
//! as it was without it.
//!
//! Completions never take the keyboard: whether one is open is derived from the text and the
//! caret on every change, Up/Down/Enter/Tab/Esc are handled by the box itself, and a click on a
//! row inserts without moving focus.

mod complete;
mod place;
mod queue;

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{App, Bounds, Context, Pixels, Task, Window};
use gpui_tokio::Tokio;
use reno_proto::{FsSearchFilesParams, RunId};

use super::{Composer, Mode};
use crate::summary;
use crate::ui::draft31;
use crate::ui::text_area::Decor;
use crate::ui::theme::Theme;
use crate::vm::draft31::{
    Active, EntryKind, Listing, TokenKind, Trigger, Variant, command_listing, file_entries,
    find_active, skill_listing, tokens, window_start,
};

pub(crate) use place::{PlaceMenu, PlaceView};

/// How many rows each variant's list shows at once.
pub(super) fn rows_cap(variant: Variant) -> usize {
    match variant {
        Variant::C => 5,
        Variant::A | Variant::B => 8,
    }
}

/// The rows asked of a search.
const FILES_ASKED: u32 = 24;

pub(super) type Anchor = Rc<Cell<Option<Bounds<Pixels>>>>;

/// An open list of completions.
pub(super) struct Completion {
    pub(super) active: Active,
    pub(super) listing: Listing,
    pub(super) selected: usize,
    pub(super) top: usize,
}

/// Everything the draft keeps on a composer: view state only. The data it shows is the store's.
pub(super) struct State {
    pub(super) complete: Option<Completion>,
    /// Esc closed the list for the token that starts here; it stays closed until the token goes.
    pub(super) dismissed: Option<(usize, Trigger)>,
    file_ticket: u64,
    file_task: Option<Task<()>>,
    /// Where the box was last painted, for the lists that dock to it.
    pub(super) box_bounds: Anchor,
    pub(super) place: place::PlaceState,
    pub(super) queue: queue::QueueState,
    /// The place's folder, for the route preview and the send.
    pub(super) cwd: Option<std::path::PathBuf>,
    /// The variant the last render drew: a change closes what is open.
    shown: Option<Variant>,
}

impl State {
    pub(super) fn new(cx: &mut Context<Composer>) -> Self {
        Self {
            complete: None,
            dismissed: None,
            file_ticket: 0,
            file_task: None,
            box_bounds: Rc::default(),
            place: place::PlaceState::new(cx),
            queue: queue::QueueState::new(cx),
            cwd: None,
            shown: None,
        }
    }
}

impl Composer {
    /// The variant on show, when the draft is on.
    pub(super) fn road31(&self, cx: &App) -> Option<Variant> {
        draft31::current(cx)
    }

    /// The variant drawing where a new task runs, when the draft's place chooser is on here.
    pub(crate) fn draft_place(&self, cx: &App) -> Option<Variant> {
        self.road31_place_view(cx).and_then(|_| self.road31(cx))
    }

    /// Whether any of the draft's own menus is open (the app-wide overlay signal reads this).
    pub(super) fn road31_overlay_open(&self) -> bool {
        self.road31.place.menu.is_some() || self.road31.queue.open
    }

    /// The harness the next message goes to: the run's, else the one picked, else Claude Code.
    fn road31_harness(&self, cx: &App) -> (String, String) {
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
    pub(super) fn road31_refresh(&mut self, cx: &mut Context<Self>) {
        if self.road31(cx).is_none() {
            return;
        }
        let active = (self.mode != Mode::Reason
            && self.model.marked().is_none()
            && self.model.selection().is_empty())
        .then(|| find_active(self.model.text(), self.model.caret()))
        .flatten();
        let Some(active) = active else {
            self.road31.complete = None;
            self.road31.dismissed = None;
            self.road31.file_task = None;
            return;
        };
        if self.road31.dismissed == Some((active.start, active.trigger)) {
            self.road31.complete = None;
            return;
        }
        let before = self.road31.complete.take();
        let same = before
            .as_ref()
            .is_some_and(|c| c.active.trigger == active.trigger && c.active.start == active.start);
        let (selected, top) = match (&before, same) {
            (Some(c), true) if c.active.query == active.query => (c.selected, c.top),
            _ => (0, 0),
        };
        let listing = match active.trigger {
            Trigger::Slash | Trigger::Dollar => self.road31_command_listing(&active, cx),
            Trigger::At => self.road31_file_listing(&active, before.as_ref().filter(|_| same), cx),
        };
        self.road31.complete = listing.map(|listing| Completion {
            active,
            listing,
            selected,
            top,
        });
    }

    fn road31_command_listing(&self, active: &Active, cx: &App) -> Option<Listing> {
        let (id, name) = self.road31_harness(cx);
        let harness = self.store.read(cx).harness_commands(&id);
        let listing = if active.trigger == Trigger::Slash {
            command_listing(&active.query, &name, harness.as_ref())
        } else {
            skill_listing(&active.query, &name, harness.as_ref())
        };
        // A `$` with no contract has nothing to say, and says nothing.
        (!listing.entries.is_empty() || listing.failure.is_some()).then_some(listing)
    }

    /// `@`: a new thread asks the mocked project search; a live run asks the real
    /// `fs.search_files`, and keeps the last answer on show until the next one comes.
    fn road31_file_listing(
        &mut self,
        active: &Active,
        before: Option<&Completion>,
        cx: &mut Context<Self>,
    ) -> Option<Listing> {
        match self.mode.clone() {
            Mode::Steer(run) => {
                self.road31_search_run(run, &active.query, cx);
                let mut listing = before.map(|c| c.listing.clone()).unwrap_or_default();
                listing.pending = true;
                Some(listing)
            }
            Mode::Home => {
                let chosen = self.road31_chosen(cx)?;
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

    fn road31_search_run(&mut self, run: RunId, query: &str, cx: &mut Context<Self>) {
        self.road31.file_ticket += 1;
        let ticket = self.road31.file_ticket;
        let Some(client) = self.store.read(cx).client() else {
            self.road31.file_task = None;
            return;
        };
        let params = FsSearchFilesParams {
            run_id: run,
            query: query.to_owned(),
            limit: Some(FILES_ASKED),
        };
        let search = Tokio::spawn(cx, async move { client.fs_search_files(params).await });
        self.road31.file_task = Some(cx.spawn(async move |this, cx| {
            let found = match search.await {
                Ok(Ok(found)) => Ok(found.files.into_iter().map(|f| f.path).collect()),
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            this.update(cx, |this, cx| this.road31_files_found(ticket, found, cx))
                .ok();
        }));
    }

    /// A real search answered. A stale answer (the query moved on) is dropped.
    fn road31_files_found(
        &mut self,
        ticket: u64,
        found: Result<Vec<String>, String>,
        cx: &mut Context<Self>,
    ) {
        if ticket != self.road31.file_ticket {
            return;
        }
        let Some(complete) = self.road31.complete.as_mut() else {
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
    pub(super) fn road31_retry(&mut self, cx: &mut Context<Self>) {
        self.road31_refresh(cx);
        cx.notify();
    }

    // ---- the keyboard ------------------------------------------------------------------------

    /// Up and Down move over the rows while a list is open and has any. Returns whether it did.
    pub(super) fn road31_nav(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(variant) = self.road31(cx) else {
            return false;
        };
        let Some(complete) = self.road31.complete.as_mut() else {
            return false;
        };
        let n = complete.listing.entries.len();
        if n == 0 || self.model.marked().is_some() {
            return false;
        }
        complete.selected = (complete.selected as isize + delta).rem_euclid(n as isize) as usize;
        complete.top = window_start(complete.selected, complete.top, rows_cap(variant));
        cx.notify();
        true
    }

    /// Esc closes the list and keeps the text. Returns whether a list was open.
    pub(super) fn road31_dismiss(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(complete) = self.road31.complete.take() else {
            return false;
        };
        self.road31.dismissed = Some((complete.active.start, complete.active.trigger));
        self.road31.file_task = None;
        cx.notify();
        true
    }

    /// Enter and Tab (and a click): put the row in the text. Returns whether one went in.
    pub(super) fn road31_accept(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.model.marked().is_some() {
            return false;
        }
        let Some(complete) = &self.road31.complete else {
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
        self.road31.complete = None;
        let text = self.model.text();
        // A space already follows the token: do not add another.
        let mut insert = entry.insert.clone();
        if text[active.end..].starts_with(char::is_whitespace) {
            insert = insert.trim_end().to_owned();
        }
        let app_command = entry.kind == EntryKind::App;
        self.road31_replace(
            active.start..active.end,
            if app_command { "" } else { &insert },
        );
        self.text_changed(window, cx);
        if app_command {
            self.road31_run_app_command(entry.label.trim_start_matches('/'), window, cx);
        }
        true
    }

    fn road31_replace(&mut self, range: Range<usize>, with: &str) {
        self.model.move_to(range.start, false);
        self.model.move_to(range.end, true);
        self.model.insert(with);
    }

    /// The app's own commands: real, handled here, never sent to the harness.
    fn road31_run_app_command(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
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
            other => crate::ui::toast::push(
                cx,
                crate::ui::toast::Tone::NotYet,
                "Command",
                format!("/{other} is not wired in this draft"),
            ),
        }
    }

    /// Tab on the command sheet: the next tab, which turns the trigger character. Returns
    /// whether it did.
    pub(super) fn road31_next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.road31(cx) != Some(Variant::B) {
            return false;
        }
        let Some(complete) = &self.road31.complete else {
            return false;
        };
        let active = complete.active.clone();
        let text = self.model.text().to_owned();
        let order = Trigger::ALL;
        let at = order.iter().position(|t| *t == active.trigger).unwrap_or(0);
        let next = (1..=order.len())
            .map(|step| order[(at + step) % order.len()])
            .find(|t| crate::vm::draft31::trigger_fits(&text, active.start, *t));
        let Some(next) = next.filter(|t| *t != active.trigger) else {
            return false;
        };
        self.road31_set_trigger(&active, next, window, cx);
        true
    }

    /// Turns the trigger character of the token (a click on a tab).
    pub(super) fn road31_set_trigger(
        &mut self,
        active: &Active,
        to: Trigger,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.road31_replace(active.start..active.start + 1, &to.glyph().to_string());
        let end = (active.end).min(self.model.text().len());
        self.model.move_to(end, false);
        self.road31.dismissed = None;
        self.text_changed(window, cx);
    }

    // ---- what the text looks like ------------------------------------------------------------

    /// The coloured words and the ghost completion the text area paints.
    pub(crate) fn decor(&self, theme: &Theme, cx: &App) -> Decor {
        let Some(variant) = draft31::current(cx) else {
            return Decor::default();
        };
        if self.mode == Mode::Reason {
            return Decor::default();
        }
        let text = self.model.text();
        let typing = self
            .road31
            .complete
            .as_ref()
            .map(|c| c.active.start..c.active.end);
        let kind_ink = |kind: TokenKind| match kind {
            TokenKind::File => theme.accent,
            TokenKind::Command => theme.warning,
            TokenKind::Skill => theme.success,
        };
        let mut spans = Vec::new();
        for (range, kind) in tokens(text, typing.as_ref()) {
            match variant {
                // Sentence: tinted by kind.
                Variant::C => {
                    let ink = kind_ink(kind);
                    spans.push((range, ink, Some(ink.opacity(0.14))));
                }
                // Popovers and sheet: a neutral chip, the trigger character in the accent.
                Variant::A | Variant::B => {
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
            }
        }
        if let Some(typing) = typing {
            let kind = match text[typing.start..].chars().next() {
                Some('/') => TokenKind::Command,
                Some('$') => TokenKind::Skill,
                _ => TokenKind::File,
            };
            let ink = if variant == Variant::C {
                kind_ink(kind)
            } else {
                theme.accent
            };
            let end = if variant == Variant::C {
                typing.end
            } else {
                typing.start + 1
            };
            spans.push((typing.start..end, ink, None));
        }
        let ghost = (variant == Variant::C)
            .then(|| self.road31_ghost())
            .flatten()
            .map(|ghost| (ghost, theme.faint));
        Decor { spans, ghost }
    }

    /// The top match after the caret, when the caret is at the end of the text and the match
    /// continues what is typed (Tab accepts it).
    fn road31_ghost(&self) -> Option<String> {
        let complete = self.road31.complete.as_ref()?;
        let text = self.model.text();
        if complete.active.end != text.len() || self.model.caret() != text.len() {
            return None;
        }
        let entry = complete.listing.entries.get(complete.selected)?;
        let typed = &text[complete.active.start..];
        let insert = entry.insert.as_str();
        let head = insert.get(..typed.len())?;
        (head.eq_ignore_ascii_case(typed) && insert.len() > typed.len())
            .then(|| insert[typed.len()..].to_owned())
    }

    // ---- hooks -------------------------------------------------------------------------------

    /// `RENO_DRAFT31_OPEN`: opens a picker, the queue list, or runs a clone, for a screenshot.
    pub(crate) fn draft31_open(&mut self, what: &str, window: &mut Window, cx: &mut Context<Self>) {
        match what {
            "project" => self.road31_open_menu(PlaceMenu::Project, window, cx),
            "branch" => self.road31_open_menu(PlaceMenu::Branch, window, cx),
            "checkout" => self.road31_open_menu(PlaceMenu::Checkout, window, cx),
            "sheet" => self.road31_open_menu(PlaceMenu::Sheet, window, cx),
            "queue" => self.road31_open_queue(cx),
            "clone-ok" => {
                self.road31_demo_clone("https://github.com/jaydendang/atlas-docs", window, cx)
            }
            "clone-fail" => self.road31_demo_clone("https://example.com/nope.git", window, cx),
            _ => {}
        }
    }

    /// The place the box's message goes to, when the chooser is on (Home only).
    pub(crate) fn road31_chosen(&self, cx: &App) -> Option<PlaceView> {
        self.road31_place_view(cx)
    }

    /// The draft's anchors hang their menus from the left, like the chips they sit on.
    pub(super) fn road31_hangs_left(&self, at: &Anchor) -> bool {
        let place = &self.road31.place;
        Rc::ptr_eq(at, &self.road31.box_bounds)
            || Rc::ptr_eq(at, &self.road31.queue.chip)
            || Rc::ptr_eq(at, &place.crumb)
            || place.triggers.iter().any(|t| Rc::ptr_eq(at, t))
    }

    /// A render closes what the previous variant opened.
    pub(super) fn road31_note_variant(&mut self, variant: Option<Variant>) {
        if self.road31.shown != variant {
            self.road31.shown = variant;
            self.road31.place.menu = None;
            self.road31.queue.open = false;
            self.road31.complete = None;
        }
    }
}
