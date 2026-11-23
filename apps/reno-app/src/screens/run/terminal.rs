//! The Terminal tab of the side panel: which terminals the run screen holds, when each is
//! attached, how they are saved and restored, what is said above one (D7) and what happens when
//! one is closed, reopened or refused.
//!
//! A tab is a local slot (`Surface::Terminal(slot)`), not the daemon's terminal id: the tab must
//! exist before `term.open` answers, survive a refusal, and outlive its shell for a Reopen. The
//! slot's terminal id is what is saved (`kind: terminal, key: <id>`); a restore gives each saved
//! id a fresh slot. `ui::terminal_view` is the terminal itself.

use std::collections::HashMap;

use gpui::{
    AnyElement, Context, Entity, FontWeight, IntoElement, SharedString, Subscription, Window, div,
    prelude::*, px,
};
use gpui_tokio::Tokio;
use reno_proto::TermId;

use super::RunScreen;
use crate::panel::Descriptors;
use crate::panel::surface::{Descriptor, Kind, Surface};
use crate::panel::view::Prefs;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, Tone};
use crate::ui::terminal_view::{Phase, Start, TerminalView, ViewEvent};
use crate::ui::theme::Theme;
use crate::ui::toast;
use crate::vm::terminal::{self as vm, BannerFacts};

/// What a close or a reopen waits for the person to confirm: a program is running in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Then {
    /// Close the tab and end the shell.
    Close,
    /// Replace the tab with a fresh shell in the same place, and end this one.
    Reopen,
}

/// One terminal tab of the screen.
pub(super) struct TermTab {
    /// How it starts when its view is made.
    start: Start,
    /// The shell the daemon holds for it, once there is one.
    pub(super) id: Option<TermId>,
    view: Option<Entity<TerminalView>>,
    _events: Option<Subscription>,
    /// A close or reopen is waiting for the person.
    pub(super) confirm: Option<Then>,
}

impl TermTab {
    fn open() -> Self {
        Self {
            start: Start::Open,
            id: None,
            view: None,
            _events: None,
            confirm: None,
        }
    }

    #[cfg(test)]
    pub(super) fn open_for_tests() -> Self {
        Self::open()
    }

    fn resume(id: TermId) -> Self {
        Self {
            start: Start::Resume(id.clone()),
            id: Some(id),
            view: None,
            _events: None,
            confirm: None,
        }
    }
}

/// The terminals of one run screen, by slot.
#[derive(Default)]
pub(super) struct Terms {
    next: u64,
    pub(super) tabs: HashMap<u64, TermTab>,
    /// The slot that was in front on the last sync.
    shown: Option<u64>,
}

impl Terms {
    fn fresh(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// Reads the saved tabs: each terminal id gets a slot, and the descriptors that come back
    /// name slots, which is what `PanelState` restores. An id that does not parse is dropped.
    pub(super) fn restore(saved: Descriptors) -> (Self, Descriptors) {
        let mut terms = Self::default();
        let slot_of = |d: Descriptor, terms: &mut Self| -> Option<Descriptor> {
            if d.kind != Kind::Terminal {
                return Some(d);
            }
            let id: TermId = d.key.parse().ok()?;
            let slot = terms
                .tabs
                .iter()
                .find(|(_, t)| t.id.as_ref() == Some(&id))
                .map(|(slot, _)| *slot)
                .unwrap_or_else(|| {
                    let slot = terms.fresh();
                    terms.tabs.insert(slot, TermTab::resume(id));
                    slot
                });
            Some(Descriptor {
                kind: Kind::Terminal,
                key: slot.to_string(),
            })
        };
        let tabs = saved
            .tabs
            .into_iter()
            .filter_map(|d| slot_of(d, &mut terms))
            .collect();
        let active = saved.active.and_then(|d| slot_of(d, &mut terms));
        // A closed terminal is a closed shell: there is nothing to bring back.
        let closed = saved
            .closed
            .into_iter()
            .filter(|d| d.kind != Kind::Terminal)
            .collect();
        (
            terms,
            Descriptors {
                tabs,
                active,
                closed,
                place: saved.place,
            },
        )
    }

    /// The saved form: each terminal's slot becomes its id. A tab whose shell is not open yet
    /// is not saved, and neither is a closed one.
    pub(super) fn to_saved(&self, panel: Descriptors) -> Descriptors {
        let by_id = |d: Descriptor| -> Option<Descriptor> {
            if d.kind != Kind::Terminal {
                return Some(d);
            }
            let slot: u64 = d.key.parse().ok()?;
            let id = self.tabs.get(&slot)?.id.as_ref()?;
            Some(Descriptor {
                kind: Kind::Terminal,
                key: id.to_string(),
            })
        };
        Descriptors {
            tabs: panel.tabs.into_iter().filter_map(by_id).collect(),
            active: panel.active.and_then(by_id),
            closed: panel
                .closed
                .into_iter()
                .filter(|d| d.kind != Kind::Terminal)
                .collect(),
            place: panel.place,
        }
    }
}

/// The slot behind a terminal tab.
fn slot_of(surface: &Surface) -> Option<u64> {
    match surface {
        Surface::Terminal(slot) => Some(*slot),
        _ => None,
    }
}

#[cfg(test)]
impl RunScreen {
    /// The view of the terminal tab that is in front.
    pub(super) fn terminal_in_front(&self) -> Option<Entity<TerminalView>> {
        self.terms
            .tabs
            .get(&slot_of(self.panel.active())?)?
            .view
            .clone()
    }

    /// The words of the line above the terminal in front.
    pub(super) fn terminal_banner_text(&self, cx: &gpui::App) -> Option<String> {
        let view = self.terminal_in_front()?;
        self.terminal_banner(&view, cx).map(|b| b.text())
    }

    /// What a refused tab says: its title and its words.
    pub(super) fn terminal_failure(&self, slot: u64, cx: &gpui::App) -> Option<(String, String)> {
        let view = self.terms.tabs.get(&slot)?.view.clone()?;
        match view.read(cx).phase() {
            Phase::Failed(fail) => Some((fail.title().to_owned(), fail.text())),
            _ => None,
        }
    }

    pub(super) fn terminal_ids(&self) -> Vec<(u64, Option<TermId>)> {
        let mut all: Vec<_> = self
            .terms
            .tabs
            .iter()
            .map(|(slot, tab)| (*slot, tab.id.clone()))
            .collect();
        all.sort_by_key(|(slot, _)| *slot);
        all
    }
}

impl RunScreen {
    /// A new terminal in front, with `input` typed into it once it is live (screenshots,
    /// reviews): `\\e` is Escape and `\\n` is Enter.
    pub(crate) fn open_demo_terminal(
        &mut self,
        input: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let surface = self.new_terminal_surface();
        self.open_surface(surface, window, cx);
        let Some(input) = input else {
            return;
        };
        let bytes = input
            .replace("\\e", "\x1b")
            .replace("\\n", "\r\n")
            .into_bytes();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1500))
                .await;
            this.update(cx, |screen, cx| {
                if let Some(view) = screen.visible_view() {
                    view.update(cx, |view, cx| view.type_bytes(&bytes, cx));
                }
            })
            .ok();
        })
        .detach();
    }

    fn visible_view(&mut self) -> Option<Entity<TerminalView>> {
        let slot = slot_of(self.panel.active())?;
        self.tabs_view(slot)
    }

    fn tabs_view(&self, slot: u64) -> Option<Entity<TerminalView>> {
        self.terms.tabs.get(&slot)?.view.clone()
    }

    // --- opening ------------------------------------------------------------------------------

    /// A new shell in a new tab.
    pub(super) fn new_terminal_surface(&mut self) -> Surface {
        let slot = self.terms.fresh();
        self.terms.tabs.insert(slot, TermTab::open());
        Surface::Terminal(slot)
    }

    /// The terminal that is in front and drawn, if one is.
    fn visible_terminal(&self, cx: &gpui::App) -> Option<u64> {
        let slot = slot_of(self.panel.active())?;
        (self.panel_shown && Prefs::get(cx).open != Some(false)).then_some(slot)
    }

    /// Makes the view of a terminal tab, once it is to be seen.
    fn terminal_view(&mut self, slot: u64, cx: &mut Context<Self>) -> Entity<TerminalView> {
        let tab = self.terms.tabs.entry(slot).or_insert_with(TermTab::open);
        if let Some(view) = &tab.view {
            return view.clone();
        }
        let (store, run, start) = (self.store.clone(), self.run.clone(), tab.start.clone());
        let view = cx.new(|cx| TerminalView::new(store, run, start, cx));
        let events = cx.subscribe(&view, move |this, _, event: &ViewEvent, cx| {
            this.on_terminal_event(slot, event, cx);
        });
        if let Some(tab) = self.terms.tabs.get_mut(&slot) {
            tab.view = Some(view.clone());
            tab._events = Some(events);
        }
        view
    }

    fn on_terminal_event(&mut self, slot: u64, event: &ViewEvent, cx: &mut Context<Self>) {
        if let ViewEvent::Opened(id) = event
            && let Some(tab) = self.terms.tabs.get_mut(&slot)
        {
            tab.id = Some(id.clone());
            self.persist_panel(cx);
        }
        cx.notify();
    }

    /// Attaches the terminal in front and detaches the others. Called wherever the panel's tabs
    /// change, and when the panel is drawn or hidden.
    pub(super) fn sync_terminals(&mut self, cx: &mut Context<Self>) {
        let front = self.visible_terminal(cx);
        if front != self.terms.shown {
            self.terms.shown = front;
        }
        let touched = self.panel_touched;
        if let Some(slot) = front {
            let view = self.terminal_view(slot, cx);
            let first = view.read(cx).phase() == &Phase::Opening && !view.read(cx).is_attached();
            view.update(cx, |view, cx| {
                // Switching to a terminal puts the keyboard in it; restoring a panel does not
                // take it from where it was.
                view.focus_pending = view.focus_pending || (touched && first);
                view.set_visible(true, cx);
            });
        }
        let others: Vec<Entity<TerminalView>> = self
            .terms
            .tabs
            .iter()
            .filter(|(slot, _)| Some(**slot) != front)
            .filter_map(|(_, tab)| tab.view.clone())
            .collect();
        for view in others {
            view.update(cx, |view, cx| view.set_visible(false, cx));
        }
    }

    // --- what the strip shows -----------------------------------------------------------------

    /// The label of each open terminal tab: "Terminal", then what a program titled it.
    pub(super) fn terminal_labels(&self, cx: &gpui::App) -> HashMap<u64, String> {
        self.terms
            .tabs
            .iter()
            .map(|(slot, tab)| {
                let title = tab.view.as_ref().and_then(|v| v.read(cx).title());
                (*slot, vm::tab_label(title))
            })
            .collect()
    }

    // --- closing and reopening ----------------------------------------------------------------

    /// ⌘W or the tab's close button. A shell with a program running in it asks first.
    pub(super) fn request_close(&mut self, slot: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.guard(slot, Then::Close, window, cx);
    }

    /// Runs `then` now, or after the person agrees when a program is running in the shell.
    fn guard(&mut self, slot: u64, then: Then, window: &mut Window, cx: &mut Context<Self>) {
        let live = self.terms.tabs.get(&slot).and_then(|tab| {
            let view = tab.view.as_ref()?;
            let id = tab.id.clone()?;
            matches!(view.read(cx).phase(), Phase::Live).then_some(id)
        });
        let client = self.store.read(cx).client();
        let (Some(id), Some(client)) = (live, client) else {
            return self.run_then(slot, then, window, cx);
        };
        let task = Tokio::spawn(cx, async move { client.term_status(id).await });
        cx.spawn_in(window, async move |this, cx| {
            let busy = matches!(task.await, Ok(Ok(status)) if status.busy);
            this.update_in(cx, |screen, window, cx| {
                if busy {
                    // Shown in front, so the question is seen where it is asked.
                    if let Some(tab) = screen.terms.tabs.get_mut(&slot) {
                        tab.confirm = Some(then);
                    }
                    screen.open_surface(Surface::Terminal(slot), window, cx);
                } else {
                    screen.run_then(slot, then, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn run_then(&mut self, slot: u64, then: Then, window: &mut Window, cx: &mut Context<Self>) {
        match then {
            Then::Close => self.drop_terminal(slot, cx),
            Then::Reopen => self.replace_terminal(slot, window, cx),
        }
    }

    /// Closes the tab, and ends the shell if there is one: closing the tab is the one thing that
    /// does (leaving the run screen, hiding the panel and quitting leave shells running).
    fn drop_terminal(&mut self, slot: u64, cx: &mut Context<Self>) {
        let surface = Surface::Terminal(slot);
        self.panel_touched = true;
        self.panel.close(&surface);
        if let Some(tab) = self.terms.tabs.remove(&slot)
            && let Some(id) = tab.id
            && let Some(client) = self.store.read(cx).client()
        {
            Tokio::spawn(cx, async move { client.term_close(id).await }).detach();
        }
        self.persist_panel(cx);
        self.ensure_patches(cx);
        cx.notify();
    }

    /// A fresh shell where this one was: in front, with the old tab closed and its shell ended.
    fn replace_terminal(&mut self, slot: u64, window: &mut Window, cx: &mut Context<Self>) {
        let fresh = self.new_terminal_surface();
        self.open_surface(fresh, window, cx);
        self.drop_terminal(slot, cx);
    }

    /// "Reopen" and "Reopen without limits": a new shell in the same place. A running program
    /// in the old one is asked about first.
    pub(super) fn request_reopen(
        &mut self,
        slot: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.guard(slot, Then::Reopen, window, cx);
    }

    fn cancel_confirm(&mut self, slot: u64, cx: &mut Context<Self>) {
        if let Some(tab) = self.terms.tabs.get_mut(&slot) {
            tab.confirm = None;
        }
        cx.notify();
    }

    /// The daemon holds as many terminals as it allows: close one that has ended, and try again.
    fn close_an_ended_terminal(&mut self, slot: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        let run = self.run.clone();
        let task = Tokio::spawn(cx, async move {
            let all = client.term_list(run).await?;
            let Some(ended) = all.into_iter().find(|t| t.exited) else {
                return Ok(false);
            };
            client.term_close(ended.id).await?;
            Ok::<bool, reno_client::ClientError>(true)
        });
        cx.spawn_in(window, async move |this, cx| {
            let closed = matches!(task.await, Ok(Ok(true)));
            this.update_in(cx, |screen, window, cx| {
                if closed {
                    screen.replace_terminal(slot, window, cx);
                } else {
                    toast::push(
                        cx,
                        toast::Tone::Info,
                        "Terminal",
                        "No ended terminal to close here. Close one from its tab.",
                    );
                }
            })
            .ok();
        })
        .detach();
    }

    /// The one line above a terminal (D7), from what the shell is and whether the run is live.
    fn terminal_banner(&self, view: &Entity<TerminalView>, cx: &gpui::App) -> Option<vm::Banner> {
        let info = view.read(cx).info()?;
        let run = self.store.read(cx).state().run(&self.run)?.status;
        vm::banner(&BannerFacts {
            sandboxed: info.sandboxed,
            in_place: info.in_place,
            cwd: &info.cwd,
            run,
        })
    }

    // --- the page -----------------------------------------------------------------------------

    /// The Terminal tab's page: the line above it (D7), a question when a close waits for an
    /// answer, and the grid, or the reason there is none.
    pub(super) fn terminal_body(
        &mut self,
        theme: &Theme,
        slot: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.store.read(cx).supports_terminals() {
            return note(
                theme,
                "terminal-unsupported",
                "Terminal needs a newer daemon",
                "This daemon does not serve terminals. Update it to open a shell.".to_owned(),
                None,
            );
        }
        let view = self.terminal_view(slot, cx);
        let confirm = self.terms.tabs.get(&slot).and_then(|t| t.confirm);
        let phase = view.read(cx).phase().clone();
        let reopen = |id: &'static str, label: &'static str, tone: Tone, cx: &mut Context<Self>| {
            kit::pill_button(theme, id, label, tone).on_click(
                cx.listener(move |this, _, window, cx| this.request_reopen(slot, window, cx)),
            )
        };
        match phase {
            Phase::Gone => {
                return note(
                    theme,
                    "terminal-gone",
                    "This shell ended",
                    "The daemon no longer holds it. A new one opens in the same place.".to_owned(),
                    Some(reopen("terminal-reopen", "Reopen", Tone::Primary, cx).into_any_element()),
                );
            }
            Phase::Failed(fail) => {
                let action = match fail {
                    vm::OpenFail::Limit => kit::pill_button(
                        theme,
                        "terminal-close-ended",
                        "Close an ended terminal",
                        Tone::Primary,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.close_an_ended_terminal(slot, window, cx);
                    }))
                    .into_any_element(),
                    // A workspace that is gone stays gone; the others may work next time.
                    vm::OpenFail::WorkspaceRemoved => div().into_any_element(),
                    _ => {
                        reopen("terminal-reopen", "Try again", Tone::Primary, cx).into_any_element()
                    }
                };
                return note(
                    theme,
                    "terminal-failed",
                    fail.title(),
                    fail.text(),
                    Some(action),
                );
            }
            Phase::Opening | Phase::Live | Phase::Ended { .. } => {}
        }

        let mut top: Vec<AnyElement> = Vec::new();
        if let Some(banner) = self.terminal_banner(&view, cx) {
            let mut row = bar(theme, "terminal-banner", banner.text());
            if banner.offers_reopen() {
                row = row.child(
                    reopen(
                        "terminal-reopen-unlimited",
                        "Reopen without limits",
                        Tone::Quiet,
                        cx,
                    )
                    .h(px(24.0)),
                );
            }
            top.push(row.into_any_element());
        }
        if let Phase::Ended { code, signal } = &phase {
            let what = match (code, signal) {
                (_, Some(signal)) => format!("This shell ended ({signal})."),
                (Some(code), None) => format!("This shell ended (exit {code})."),
                (None, None) => "This shell ended.".to_owned(),
            };
            top.push(
                bar(theme, "terminal-ended", what)
                    .child(reopen("terminal-reopen", "Reopen", Tone::Quiet, cx).h(px(24.0)))
                    .into_any_element(),
            );
        }
        if let Some(then) = confirm {
            let question = match then {
                Then::Close => "A program is still running here. Closing the terminal ends it.",
                Then::Reopen => "A program is still running here. Reopening ends it.",
            };
            top.push(
                bar(theme, "terminal-confirm", question.to_owned())
                    .child(
                        kit::pill_button(
                            theme,
                            "terminal-confirm-yes",
                            match then {
                                Then::Close => "Close terminal",
                                Then::Reopen => "Reopen",
                            },
                            Tone::Danger,
                        )
                        .h(px(24.0))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.run_then(slot, then, window, cx);
                            },
                        )),
                    )
                    .child(
                        kit::pill_button(theme, "terminal-confirm-keep", "Keep open", Tone::Quiet)
                            .h(px(24.0))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cancel_confirm(slot, cx);
                            })),
                    )
                    .into_any_element(),
            );
        }
        div()
            .id("terminal-tab")
            .debug_selector(|| "terminal-tab".to_owned())
            .size_full()
            .flex()
            .flex_col()
            .children(top)
            .child(div().flex_1().min_h_0().child(view))
            .into_any_element()
    }
}

/// A one-line strip above the grid.
fn bar(theme: &Theme, id: &'static str, text: String) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .flex_none()
        .min_h(px(32.0))
        .px(px(12.0))
        .py(px(4.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .border_b_1()
        .border_color(theme.hairline)
        .text_size(px(12.0))
        .text_color(theme.muted)
        .child(div().flex_1().min_w_0().child(SharedString::from(text)))
}

/// A page that says why there is no grid.
fn note(
    theme: &Theme,
    id: &'static str,
    title: &str,
    text: String,
    action: Option<AnyElement>,
) -> AnyElement {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .size_full()
        .flex()
        .justify_center()
        .child(
            div()
                .w_full()
                .max_w(px(460.0))
                .px(px(28.0))
                .pt(px(44.0))
                .flex()
                .flex_col()
                .items_start()
                .gap(px(8.0))
                .child(icon(Icon::Terminal, 20.0, theme.muted))
                .child(
                    div()
                        .text_size(px(15.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(SharedString::from(title.to_owned())),
                )
                .child(
                    div()
                        .text_size(px(13.5))
                        .text_color(theme.muted)
                        .child(SharedString::from(text)),
                )
                .children(action.map(|a| div().pt(px(6.0)).child(a))),
        )
        .into_any_element()
}
