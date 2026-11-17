//! The Run screen: one run's transcript, its details and a steer input.
//!
//! The transcript is a virtualised GPUI `list()`, bottom-aligned, that follows the tail while the
//! run streams. Its rows come from the pure `transcript::build_rows`; this file only draws them
//! and keeps a few per-row caches: the streaming markdown parse of each prose row and which tool
//! groups are open. A store notification rebuilds the rows, finds the first one that differs and
//! tells the list to re-measure from there, so a streamed delta costs one row, not the list.

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, Context, Entity, FocusHandle, FollowMode, FontWeight, IntoElement, ListAlignment,
    ListState, Render, SharedString, Subscription, Window, div, list, prelude::*, px,
};
use reno_proto::{
    Approval, ApprovalDecision, ApprovalId, ExecutorId, Remember, RememberScope, RunId, RunSummary,
    StopReason,
};

use super::ScreenEvent;
use super::executors::health_lines;
use crate::store::{RunRouting, Store};
use crate::summary;
use crate::transcript::{self, Built, Facts, Key, Outcome, Resolution, Row, ToolEntry};
use crate::ui::composer::{Composer, ReasonEvent};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::markdown_view::{self, Streaming};
use crate::ui::theme::{self, Theme};
use crate::ui::toast;
use crate::vm::executors as executor_facts;
use crate::vm::routing::{self, RoutingView};
use crate::vm::rules;
use crate::vm::runs as run_facts;

/// Width of the transcript column and of the steer input.
const COLUMN: f32 = 760.0;
/// Width of the details pane.
const DETAILS: f32 = 330.0;
/// Rows the list keeps measured beyond the viewport.
const OVERDRAW: f32 = 600.0;

pub(crate) struct RunScreen {
    store: Entity<Store>,
    run: RunId,
    list: ListState,
    rows: Vec<Row>,
    facts: Facts,
    /// One streaming parse per prose row, keyed by the row's first event.
    markdown: HashMap<Key, Streaming>,
    /// Tool groups and reasoning rows the user opened.
    open: HashSet<Key>,
    details_open: bool,
    steer: Entity<Composer>,
    /// The reason field under each held action of this run: typing in it and pressing Deny (or
    /// Enter) denies with those words (ui-brief 2026-11-17, from direction B).
    reasons: HashMap<ApprovalId, (Entity<Composer>, Subscription)>,
    /// The held action whose Approve ▾ menu is open.
    menu_open: Option<ApprovalId>,
    _store: Subscription,
}

impl gpui::EventEmitter<ScreenEvent> for RunScreen {}

impl RunScreen {
    pub(crate) fn new(store: Entity<Store>, run: RunId, cx: &mut Context<Self>) -> Self {
        store.update(cx, |store, cx| store.open_run(run.clone(), cx));
        let steer = cx.new(|cx| Composer::steer(store.clone(), run.clone(), cx));
        let list = ListState::new(0, ListAlignment::Bottom, px(OVERDRAW));
        list.set_follow_mode(FollowMode::Tail);
        let mut screen = Self {
            _store: cx.observe(&store, |this, _, cx| {
                this.refresh(cx);
                cx.notify();
            }),
            store,
            run,
            list,
            rows: Vec::new(),
            facts: Facts::default(),
            markdown: HashMap::new(),
            open: HashSet::new(),
            details_open: true,
            steer,
            reasons: HashMap::new(),
            menu_open: None,
        };
        screen.refresh(cx);
        screen
    }

    pub(crate) fn run_id(&self) -> &RunId {
        &self.run
    }

    #[cfg(test)]
    pub(crate) fn steer_input(&self) -> &Entity<Composer> {
        &self.steer
    }

    pub(crate) fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        use gpui::Focusable as _;
        self.steer.read(cx).focus_handle(cx)
    }

    #[cfg(test)]
    pub(crate) fn reason_input(&self, approval: &ApprovalId) -> Option<&Entity<Composer>> {
        self.reasons.get(approval).map(|(input, _)| input)
    }

    #[cfg(test)]
    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    #[cfg(test)]
    pub(crate) fn facts(&self) -> &Facts {
        &self.facts
    }

    #[cfg(test)]
    pub(crate) fn is_following_tail(&self) -> bool {
        self.list.is_following_tail()
    }

    pub(crate) fn toggle_details(&mut self, cx: &mut Context<Self>) {
        self.details_open = !self.details_open;
        cx.notify();
    }

    fn toggle(&mut self, key: Key, cx: &mut Context<Self>) {
        if !self.open.remove(&key) {
            self.open.insert(key);
        }
        // The row's height changed: measure it again.
        if let Some(ix) = self.rows.iter().position(|r| r.key() == key) {
            self.list.splice(ix..ix + 1, 1);
        }
        cx.notify();
    }

    /// Rebuilds the rows from the store and tells the list what changed.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        // The routing decision changes while a run starts (a fallback): read it again when the
        // run's executor or status does. Asking is a no-op while the answer held is current.
        let run = self.run.clone();
        self.store
            .update(cx, |store, cx| store.ensure_routing(&run, cx));
        let routing = match self.store.read(cx).routing(&self.run) {
            Some(RunRouting::Ready(Some(decision))) => Some(routing::view(decision)),
            _ => None,
        };
        let built = {
            let state = self.store.read(cx).state();
            let events = state
                .transcript(&self.run)
                .map_or(&[][..], |t| t.events.as_slice());
            let pending: Vec<Approval> = state.approvals_for(&self.run).cloned().collect();
            // The whole prompt when the daemon sends it; otherwise (an older daemon) the title,
            // which is its first line, and which in the "bot . repo: what" shape shows its "what".
            let prompt = state.run(&self.run).map(|r| {
                r.prompt
                    .as_deref()
                    .unwrap_or_else(|| summary::parse_title(&r.title).what)
            });
            let mut built = transcript::build_rows(prompt, events, &pending);
            built.rows = transcript::with_routing(built.rows, routing);
            (built, pending)
        };
        let (built, pending) = built;
        self.keep_reasons(&pending, cx);
        self.apply(built);
    }

    /// One reason field per held action of this run; a field goes when its action is answered.
    fn keep_reasons(&mut self, pending: &[Approval], cx: &mut Context<Self>) {
        self.reasons
            .retain(|id, _| pending.iter().any(|approval| &approval.id == id));
        if self
            .menu_open
            .as_ref()
            .is_some_and(|id| !pending.iter().any(|approval| &approval.id == id))
        {
            self.menu_open = None;
        }
        for approval in pending {
            if self.reasons.contains_key(&approval.id) {
                continue;
            }
            let store = self.store.clone();
            let input = cx.new(|cx| Composer::reason(store, cx));
            let id = approval.id.clone();
            let events = cx.subscribe(&input, move |this, _, event: &ReasonEvent, cx| {
                if let ReasonEvent::Submitted(message) = event {
                    this.deny(id.clone(), message.clone(), cx);
                }
            });
            self.reasons.insert(approval.id.clone(), (input, events));
        }
    }

    /// Deny `approval`, with the reason the agent is told.
    fn deny(&mut self, approval: ApprovalId, message: Option<String>, cx: &mut Context<Self>) {
        let reasons = self
            .store
            .read(cx)
            .state()
            .supports(reno_proto::FEATURE_APPROVAL_MESSAGE);
        let message = message.filter(|_| reasons);
        self.store.update(cx, |store, cx| {
            store.decide_with_message(approval, ApprovalDecision::Deny, message, cx);
        });
    }

    /// Approve `approval`, keeping the answer as far as `scope` reaches (`None`: once).
    fn approve(
        &mut self,
        approval: ApprovalId,
        scope: Option<RememberScope>,
        cx: &mut Context<Self>,
    ) {
        self.menu_open = None;
        let remember = scope.map(|scope| Remember {
            scope,
            expires_in_secs: None,
        });
        self.store.update(cx, |store, cx| {
            store.decide_remembering(approval, ApprovalDecision::ApproveOnce, None, remember, cx);
        });
    }

    fn apply(&mut self, built: Built) {
        let Built { rows, facts } = built;
        self.facts = facts;
        for row in &rows {
            if let Row::Prose { key, text } = row {
                self.markdown.entry(*key).or_default().set_text(text);
            }
        }
        let same = self
            .rows
            .iter()
            .zip(&rows)
            .take_while(|(old, new)| old == new)
            .count();
        if same < self.rows.len() || same < rows.len() {
            self.list.splice(same..self.rows.len(), rows.len() - same);
        }
        self.rows = rows;
    }

    fn summary<'a>(&self, cx: &'a gpui::App) -> Option<&'a RunSummary> {
        self.store.read(cx).state().run(&self.run)
    }

    // --- header ---------------------------------------------------------------------------

    fn header(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let run = self.summary(cx).cloned();
        let (executor, what, status) = match &run {
            Some(r) => (
                r.executor.as_str().to_owned(),
                summary::parse_title(&r.title).what.to_owned(),
                Some((
                    kit::run_color(theme, r.status, summary::reported_problem(r)),
                    summary::run_status_label(r),
                )),
            ),
            None => (String::new(), format!("Run {}", self.run), None),
        };
        let run_id = self.run.clone();
        let project = {
            let store = self.store.read(cx);
            crate::vm::projects::key_of(run.as_ref(), |dir| store.root_of(dir))
        };

        div()
            .flex_none()
            .h(px(52.0))
            .px(px(22.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .border_b_1()
            .border_color(theme.hairline)
            .child(
                div()
                    .id("crumb-project")
                    .debug_selector(|| "crumb-project".to_owned())
                    .text_color(theme.muted)
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.text))
                    .child(project.name())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(ScreenEvent::OpenProject(project.clone()));
                    })),
            )
            .child(div().text_color(theme.faint).child("/"))
            .when(!executor.is_empty(), |h| {
                h.child(kit::exec_glyph(theme, &executor, 16.0))
            })
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(what),
            )
            .when_some(status, |h, status| {
                h.child(
                    kit::status_pill_worded(status.0, status.1)
                        .id(SharedString::from(format!("status-{run_id}"))),
                )
            })
            .child(div().flex_1())
            .child(
                kit::icon_button(theme, "details-toggle", Icon::Sidebar, 30.0)
                    .debug_selector(|| "details-toggle".to_owned())
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_details(cx))),
            )
            .child(
                kit::icon_button(theme, "search-run", Icon::Search, 30.0)
                    .on_click(|_, _, cx| toast::not_yet(cx, "Search in a run")),
            )
    }

    // --- rows -----------------------------------------------------------------------------

    fn render_row(&self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme::current(cx);
        let Some(row) = self.rows.get(ix) else {
            return div().into_any_element();
        };
        let body = match row {
            Row::User { text, steer, .. } => user_bubble(&theme, text, *steer),
            Row::Prose { key, .. } => match self.markdown.get(key) {
                Some(stream) => div()
                    .text_size(px(15.0))
                    .line_height(px(24.0))
                    .child(markdown_view::blocks(stream.tree(), &theme, window))
                    .into_any_element(),
                None => div().into_any_element(),
            },
            Row::Reasoning { key, text } => self.reasoning(&theme, *key, text, cx),
            Row::Tools { key, calls } => self.tools(&theme, *key, calls, cx),
            Row::Question {
                questions,
                resolution,
                ..
            } => question_card(&theme, questions, resolution),
            Row::Approval { key, approval } => self.approval_card(&theme, *key, approval, cx),
            Row::Asked { line, .. } => event_line(&theme, Icon::Shield, theme.muted, line),
            Row::Routing { key, view } => self.routing_card(&theme, *key, view, cx),
            Row::Done {
                stop,
                result,
                diagnostic,
                reason,
                ..
            } => done_line(
                &theme,
                *stop,
                result.as_deref(),
                diagnostic.as_deref(),
                reason.as_ref(),
            ),
        };
        div()
            .w_full()
            .when(ix == 0, |d| d.pt(px(22.0)))
            .pb(px(if ix + 1 == self.rows.len() {
                26.0
            } else {
                16.0
            }))
            .child(body)
            .into_any_element()
    }

    /// "Routed to Codex: Claude Code was signed out", with the "why" collapsed.
    fn routing_card(
        &self,
        theme: &Theme,
        key: Key,
        view: &RoutingView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.open.contains(&key);
        let chosen = view.chosen.clone();
        let head = div()
            .id("routing-why")
            .debug_selector(|| "routing-why".to_owned())
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(13.5))
            .when(view.has_detail(), |d| {
                d.cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(key, cx)))
            })
            .child(icon(Icon::Route, 14.0, theme.faint))
            .when_some(chosen.clone(), |d, who| {
                d.child(kit::exec_glyph(theme, &who.id, 14.0))
            })
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(view.headline.clone())),
            )
            .when_some(
                chosen.and_then(|c| c.secondary().map(str::to_owned)),
                |d, id| {
                    d.child(
                        div()
                            .flex_none()
                            .text_color(theme.faint)
                            .child(SharedString::from(id)),
                    )
                },
            )
            .when_some(view.because.clone(), |d, because| {
                d.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.muted)
                        .child(SharedString::from(format!("\u{b7} {because}"))),
                )
            })
            .child(div().flex_1())
            .when(view.has_detail(), |d| {
                d.child(icon(
                    if open { Icon::ChevDown } else { Icon::Fwd },
                    14.0,
                    theme.muted,
                ))
            });

        let label = |text: &'static str| {
            div()
                .flex_none()
                .w(px(86.0))
                .text_color(theme.faint)
                .child(text)
        };
        let named = |name: String, id: Option<String>, then: String| {
            div()
                .flex()
                .flex_wrap()
                .gap(px(6.0))
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(SharedString::from(name)),
                )
                .when_some(id, |d, id| {
                    d.child(div().text_color(theme.faint).child(SharedString::from(id)))
                })
                .child(SharedString::from(then))
        };
        let detail = div()
            .pt(px(8.0))
            .pl(px(22.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .text_size(px(13.0))
            .text_color(theme.muted)
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(label("Rule"))
                    .child(SharedString::from(view.rule.clone())),
            )
            .when(view.order.len() > 1, |d| {
                let order = view
                    .order
                    .iter()
                    .map(|n| n.name.clone())
                    .collect::<Vec<_>>()
                    .join(" \u{2192} ");
                d.child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(label("Order"))
                        .child(SharedString::from(order)),
                )
            })
            .children(view.rejected.iter().enumerate().map(|(i, r)| {
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(label(if i == 0 { "Passed over" } else { "" }))
                    .child(named(
                        r.who.name.clone(),
                        r.who.secondary().map(str::to_owned),
                        r.reason.clone(),
                    ))
            }))
            .children(view.walk.iter().enumerate().map(|(i, step)| {
                let to = step
                    .to
                    .as_ref()
                    .map_or_else(|| "nothing left".to_owned(), |t| t.name.clone());
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(label(if i == 0 { "Fallback" } else { "" }))
                    .child(named(
                        format!("{} \u{2192} {to}", step.from.name),
                        None,
                        step.reason.clone(),
                    ))
            }));

        div()
            .id("routing-card")
            .debug_selector(|| "routing-card".to_owned())
            .w_full()
            .px(px(14.0))
            .py(px(10.0))
            .rounded(px(12.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.hairline)
            .child(head)
            .when(open, |d| d.child(detail))
            .into_any_element()
    }

    fn reasoning(&self, theme: &Theme, key: Key, text: &str, cx: &mut Context<Self>) -> AnyElement {
        let open = self.open.contains(&key);
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                disclosure(
                    theme,
                    SharedString::from(format!("reasoning-{}", key.0)),
                    open,
                    "Reasoning".to_owned(),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(key, cx))),
            )
            .when(open, |d| {
                d.child(
                    div()
                        .pl(px(24.0))
                        .text_size(px(13.5))
                        .line_height(px(21.0))
                        .text_color(theme.muted)
                        .child(SharedString::from(text.to_owned())),
                )
            })
            .into_any_element()
    }

    fn tools(
        &self,
        theme: &Theme,
        key: Key,
        calls: &[ToolEntry],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.open.contains(&key);
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                disclosure(
                    theme,
                    SharedString::from(format!("tools-{}", key.0)),
                    open,
                    transcript::group_title(calls),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(key, cx))),
            )
            .when(open, |d| {
                d.child(
                    div()
                        .ml(px(24.0))
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(theme.hairline)
                        .bg(theme.surface)
                        .flex()
                        .flex_col()
                        .children(calls.iter().map(|call| tool_line(theme, call))),
                )
            })
            .into_any_element()
    }

    /// A held action, answered where the agent asked (ui-brief 2026-11-17): who and what, the
    /// exact command, why it waits, Approve once ▾ (how far the answer reaches), and a reason
    /// field with Deny under it.
    fn approval_card(
        &self,
        theme: &Theme,
        key: Key,
        approval: &Approval,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let state = store.state();
        let busy = store.is_deciding(&approval.id);
        let keeps_rules = state.supports(reno_proto::FEATURE_RULES);
        let summary = state.run(&approval.run_id);
        let bot = summary.and_then(|r| summary::run_bot(r));
        let who = bot.unwrap_or("The run").to_owned();
        let executor = summary.map_or("", |r| r.executor.as_str());
        let age = summary::age_label(store.now_ms().saturating_sub(approval.requested_at_ms));
        let command = summary::subject_line(&approval.subject);
        let word = summary::action_word(&approval.subject);
        let choices = rules::approve_choices(bot);
        let menu_open = self.menu_open.as_ref() == Some(&approval.id);

        let id = approval.id.clone();
        let approve = {
            let id = id.clone();
            cx.listener(move |this, _, _, cx| this.approve(id.clone(), None, cx))
        };
        let toggle_menu = {
            let id = id.clone();
            cx.listener(move |this, _, _, cx| {
                this.menu_open = if this.menu_open.as_ref() == Some(&id) {
                    None
                } else {
                    Some(id.clone())
                };
                cx.notify();
            })
        };
        let deny = {
            let id = id.clone();
            cx.listener(move |this, _, _, cx| {
                let message = this
                    .reasons
                    .get(&id)
                    .and_then(|(input, _)| input.read(cx).typed_reason());
                this.deny(id.clone(), message, cx);
            })
        };

        let approve_button = kit::pill_button(theme, "approve", "Approve once", Tone::Primary)
            .when(busy, |b| b.opacity(0.4))
            .when(!busy, |b| b.on_click(approve));
        let more_button = keeps_rules.then(|| {
            kit::icon_button(
                theme,
                "approve-more",
                if menu_open {
                    Icon::ChevUp
                } else {
                    Icon::ChevDown
                },
                28.0,
            )
            .debug_selector(|| "approve-more".to_owned())
            .when(!busy, |b| b.on_click(toggle_menu))
        });
        let deny_button = kit::pill_button(theme, "deny", "Deny", Tone::Danger)
            .when(busy, |b| b.opacity(0.4))
            .when(!busy, |b| b.on_click(deny));
        // What a kept answer holds, in words: exactly this action, for whom.
        let keeps = rules::hint(
            &approval.subject,
            if bot.is_some() {
                RememberScope::Bot
            } else {
                RememberScope::Run
            },
            bot,
        );
        let menu =
            (menu_open && keeps_rules).then(|| {
                div()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap(px(6.0))
                    .child(
                        div()
                            .w_full()
                            .text_size(px(12.0))
                            .text_color(theme.muted)
                            .child(keeps),
                    )
                    .children(choices.into_iter().enumerate().skip(1).map(
                        |(ix, (scope, label))| {
                            let id = id.clone();
                            kit::pill_button(
                                theme,
                                format!("approve-scope-{ix}"),
                                label,
                                Tone::Quiet,
                            )
                            .debug_selector(move || format!("approve-scope-{ix}"))
                            .when(!busy, |b| {
                                b.on_click(cx.listener(move |this, _, _, cx| {
                                    this.approve(id.clone(), scope, cx);
                                }))
                            })
                        },
                    ))
            });
        let reason = self
            .reasons
            .get(&approval.id)
            .map(|(input, _)| input.clone());

        div()
            .id(SharedString::from(format!("approval-card-{}", key.0)))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .p(px(16.0))
            .rounded(px(18.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.hairline)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(13.5))
                    .child(kit::exec_glyph(theme, executor, 14.0))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(who))
                    .child(div().text_color(theme.muted).child("wants to run"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(theme.warning)
                            .child(format!("held {age}")),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .px(px(14.0))
                    .py(px(9.0))
                    .rounded(px(10.0))
                    .bg(theme.shell)
                    .font_family(MONO)
                    .text_size(px(13.0))
                    .child(command),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(kit::tag(theme.warning, format!("{word} needs a human")))
                    .child(div().flex_1())
                    .child(approve_button)
                    .children(more_button),
            )
            .children(menu)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().flex_1().min_w_0().children(reason))
                    .child(deny_button),
            )
            .into_any_element()
    }

    // --- details --------------------------------------------------------------------------

    fn details(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let run = store.state().run(&self.run);
        let now = store.now_ms();
        // A run its executor could not take shows that executor's health, as the daemon has it
        // now, so the reason and the way out are on the same screen.
        let failed = executor_facts::failed_executor(store.state(), &self.run, now);

        let section = |title: &'static str| {
            div()
                .text_size(px(12.0))
                .text_color(theme.faint)
                .pb(px(4.0))
                .child(title)
        };
        let kv = |label: &'static str, value: AnyElement| {
            div()
                .flex()
                .items_start()
                .gap(px(12.0))
                .py(px(6.0))
                .child(
                    div()
                        .flex_none()
                        .w(px(86.0))
                        .text_size(px(13.5))
                        .text_color(theme.muted)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().text_size(px(13.5)).child(value))
        };
        let text = |s: String| div().child(s).into_any_element();

        let mut pane = div()
            .id("details")
            .w(px(DETAILS))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(theme.hairline)
            .px(px(26.0))
            .py(px(22.0))
            .flex()
            .flex_col()
            .child(section("RUN"));

        let Some(run) = run else {
            return pane.child(kv("State", text("starting".to_owned())));
        };
        let model = self.facts.model.clone().or_else(|| run.model.clone());
        let end = if run.status.is_terminal() {
            run.updated_at_ms
        } else {
            now
        };
        // The access mode is the bot's: a run without a bot has none to show, and a bot whose
        // definition is not read yet shows an ellipsis.
        let access = run.bot.as_ref().map(|_| {
            run_facts::access_label(store.state(), &self.run)
                .unwrap_or("\u{2026}")
                .to_owned()
        });

        let problem = summary::reported_problem(run);
        let state_color = kit::run_color(theme, run.status, problem);
        pane = pane
            .child(kv(
                "State",
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(kit::dot(state_color, 7.0))
                    .child(
                        div()
                            .text_color(state_color)
                            .child(summary::run_status_label(run)),
                    )
                    .into_any_element(),
            ))
            .when_some(
                run.report.as_ref().filter(|_| problem.is_some()),
                |p, report| {
                    p.child(kv(
                        "Report",
                        div()
                            .id("details-report")
                            .debug_selector(|| "details-report".to_owned())
                            .child(report.summary.clone())
                            .into_any_element(),
                    ))
                },
            )
            .when_some(summary::run_bot(run), |p, bot| {
                p.child(kv("Bot", text(bot.to_owned())))
            })
            .child(kv(
                "Executor",
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(kit::exec_glyph(theme, run.executor.as_str(), 14.0))
                    .child(summary::executor_name(run.executor.as_str()))
                    .when(
                        run.executor.as_str() != summary::executor_name(run.executor.as_str()),
                        |d| {
                            d.child(
                                div()
                                    .text_color(theme.faint)
                                    .child(SharedString::from(run.executor.as_str().to_owned())),
                            )
                        },
                    )
                    .into_any_element(),
            ))
            .when_some(access, |p, access| p.child(kv("Access", text(access))))
            .when_some(model, |p, model| p.child(kv("Model", text(model))))
            .when_some(summary::run_repo(run), |p, repo| {
                // The folder's name, with the whole path under it when the daemon gave one.
                p.child(kv(
                    "Repository",
                    div()
                        .id("details-repository")
                        .debug_selector(|| "details-repository".to_owned())
                        .child(repo.name)
                        .when_some(repo.path, |d, path| {
                            d.child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme.faint)
                                    .child(SharedString::from(path)),
                            )
                        })
                        .into_any_element(),
                ))
            })
            .when(run.attempt > 1, |p| {
                p.child(kv("Attempt", text(run.attempt.to_string())))
            })
            .child(kv(
                "Started",
                text(format!(
                    "{} \u{b7} {}",
                    summary::clock_utc(run.created_at_ms),
                    summary::duration_label(end.saturating_sub(run.created_at_ms))
                )),
            ));

        let usage = match self.facts.usage {
            Some(u) => div()
                .child(format!("{} tokens", group_digits(u.total())))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.faint)
                        .child(format!(
                            "{} in \u{b7} {} out",
                            group_digits(u.input),
                            group_digits(u.output)
                        )),
                )
                .into_any_element(),
            None => div()
                .text_color(theme.faint)
                .child("none yet")
                .into_any_element(),
        };
        if let Some(failed) = failed {
            pane = pane
                .child(div().h(px(14.0)))
                .child(section("EXECUTOR HEALTH"))
                .child(health_lines(theme, &failed.row))
                .child(self.executor_actions(&failed.row, theme, cx));
        }
        pane.child(div().h(px(14.0)))
            .child(section("USAGE"))
            .child(kv("Tokens", usage))
    }

    /// Open the Executors screen, or check this executor again.
    fn executor_actions(
        &self,
        row: &executor_facts::ExecutorRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let checking = row
            .id
            .parse::<ExecutorId>()
            .is_ok_and(|id| self.store.read(cx).is_checking(Some(&id)));
        let check = {
            let id = row.id.parse::<ExecutorId>().ok();
            cx.listener(move |this, _, _, cx| {
                if let Some(id) = id.clone() {
                    this.store
                        .update(cx, |store, cx| store.refresh_executors(Some(id), cx));
                }
            })
        };
        let open = cx.listener(|_, _, _, cx| cx.emit(ScreenEvent::OpenExecutors));
        let check_button = kit::pill_button(
            theme,
            "run-check-executor",
            if checking {
                "Checking\u{2026}"
            } else {
                "Check again"
            },
            Tone::Quiet,
        );
        let check_button = if checking {
            check_button.opacity(0.4).cursor_default()
        } else {
            check_button.on_click(check)
        };
        div()
            .pt(px(10.0))
            .flex()
            .flex_wrap()
            .gap(px(8.0))
            .child(
                kit::pill_button(theme, "run-open-executors", "Open Executors", Tone::Quiet)
                    .on_click(open),
            )
            .child(check_button)
            .into_any_element()
    }
}

/// 12345 becomes "12,345".
fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A user message: right-aligned bubble.
fn user_bubble(theme: &Theme, text: &str, steer: bool) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .items_end()
        .gap(px(5.0))
        .when(steer, |d| {
            d.child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.faint)
                    .child("You \u{b7} steer"),
            )
        })
        .child(
            div()
                .max_w(px(560.0))
                .px(px(18.0))
                .py(px(12.0))
                .rounded(px(20.0))
                .bg(theme.accent.opacity(0.14))
                .text_size(px(15.0))
                .line_height(px(23.0))
                .child(SharedString::from(text.to_owned())),
        )
        .into_any_element()
}

/// The clickable head of a collapsible row: a chevron and a label.
fn disclosure(
    theme: &Theme,
    id: SharedString,
    open: bool,
    label: String,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(px(14.5))
        .text_color(theme.muted)
        .cursor_pointer()
        .hover({
            let text = theme.text;
            move |s| s.text_color(text)
        })
        .child(icon(
            if open { Icon::ChevDown } else { Icon::Fwd },
            14.0,
            theme.muted,
        ))
        .child(label)
}

fn tool_line(theme: &Theme, call: &ToolEntry) -> impl IntoElement {
    let (summary_text, color) = match &call.outcome {
        Outcome::Pending => ("running".to_owned(), theme.faint),
        Outcome::Ok(s) => (s.clone().unwrap_or_else(|| "ok".to_owned()), theme.muted),
        Outcome::Failed(s) => (
            s.clone().unwrap_or_else(|| "failed".to_owned()),
            theme.danger,
        ),
    };
    div()
        .flex()
        .items_start()
        .justify_between()
        .gap(px(16.0))
        .px(px(14.0))
        .py(px(8.0))
        .child(
            div()
                .min_w_0()
                .font_family(MONO)
                .text_size(px(12.5))
                .child(SharedString::from(call.label.clone())),
        )
        .child(
            div()
                .flex_none()
                .max_w(px(280.0))
                .truncate()
                .text_size(px(12.5))
                .text_color(color)
                .child(summary_text),
        )
}

/// The tone of the card's leading "?": warning while the question waits, then the tone of how
/// it ended.
fn question_icon_tone(theme: &Theme, resolution: &Resolution) -> gpui::Hsla {
    match resolution {
        Resolution::Pending => theme.warning,
        Resolution::Declined | Resolution::Unanswered => theme.faint,
        Resolution::Answered(_) => theme.success,
    }
}

/// The question card. Pending, it is the live ask; resolved, it is the record of how it ended:
/// a status tag, the options dimmed (the chosen one lit) and no promise of a later phase.
fn question_card(
    theme: &Theme,
    questions: &[reno_proto::Question],
    resolution: &Resolution,
) -> AnyElement {
    let tag = match resolution {
        Resolution::Pending => None,
        Resolution::Declined => Some((theme.faint, "Declined", "question-tag-declined")),
        Resolution::Unanswered => Some((theme.faint, "Not answered", "question-tag-unanswered")),
        Resolution::Answered(_) => Some((theme.success, "Answered", "question-tag-answered")),
    };
    let answer_of = |i: usize| match resolution {
        Resolution::Answered(answers) => answers.get(i).cloned().flatten(),
        _ => None,
    };
    let card = div()
        .w_full()
        .p(px(16.0))
        .rounded(px(18.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
        .flex()
        .flex_col()
        .gap(px(12.0))
        .children(questions.iter().enumerate().map(|(i, q)| {
            let answer = answer_of(i);
            let chosen = |o: &str| {
                answer
                    .as_deref()
                    .is_some_and(|a| a.split(", ").any(|part| part == o))
            };
            let free_text = answer
                .as_ref()
                .filter(|_| !q.options.iter().any(|o| chosen(o)));
            let header_tag = tag.filter(|_| i == 0).map(|(tone, label, selector)| {
                div()
                    .ml_auto()
                    .debug_selector(move || selector.to_owned())
                    .child(kit::tag(tone, label))
            });
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(icon(
                            Icon::Question,
                            15.0,
                            question_icon_tone(theme, resolution),
                        ))
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .child(q.prompt.clone()),
                        )
                        .children(header_tag),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(8.0))
                        .children(q.options.iter().map(|o| {
                            let chip = kit::chip(theme, o.clone());
                            match resolution {
                                Resolution::Pending => chip,
                                _ if chosen(o) => chip
                                    .border_color(theme.success.opacity(0.7))
                                    .bg(theme.success.opacity(0.14))
                                    .font_weight(FontWeight::MEDIUM)
                                    .debug_selector(|| "question-chosen-chip".to_owned()),
                                _ => chip
                                    .opacity(0.5)
                                    .text_color(theme.faint)
                                    .debug_selector(|| "question-dimmed-chip".to_owned()),
                            }
                        })),
                )
                .children(free_text.map(|text| {
                    div()
                        .text_size(px(13.5))
                        .text_color(theme.text)
                        .child(SharedString::from(format!("\u{201c}{text}\u{201d}")))
                }))
        }));
    match resolution {
        Resolution::Pending => card.child(
            div()
                .debug_selector(|| "question-pending-note".to_owned())
                .text_size(px(12.5))
                .text_color(theme.faint)
                .child("Answering from the app arrives in a later phase."),
        ),
        Resolution::Declined => card.child(
            div()
                .debug_selector(|| "question-declined-line".to_owned())
                .text_size(px(12.5))
                .text_color(theme.faint)
                .child("Nobody answered in time."),
        ),
        Resolution::Unanswered => card.child(
            div()
                .text_size(px(12.5))
                .text_color(theme.faint)
                .child("The run went on without an answer."),
        ),
        Resolution::Answered(_) => card,
    }
    .into_any_element()
}

/// A one-line event: a muted icon and a sentence.
fn event_line(theme: &Theme, glyph: Icon, tone: gpui::Hsla, text: &str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(9.0))
        .text_size(px(13.5))
        .text_color(theme.muted)
        .child(icon(glyph, 15.0, tone))
        .child(SharedString::from(text.to_owned()))
        .into_any_element()
}

/// The closing status line.
fn done_line(
    theme: &Theme,
    stop: StopReason,
    result: Option<&str>,
    diagnostic: Option<&str>,
    reason: Option<&reno_proto::StopDetail>,
) -> AnyElement {
    let (glyph, tone, head) = match stop {
        StopReason::Completed => (Icon::Check, theme.success, "Finished"),
        StopReason::Interrupted => (Icon::Stop, theme.faint, "Stopped"),
        StopReason::Error => (Icon::X, theme.danger, "Failed"),
        StopReason::Refusal => (Icon::X, theme.warning, "The model declined"),
        StopReason::MaxTokens => (Icon::X, theme.warning, "Hit the output limit"),
    };
    // The typed reason says it in words (a budget stop reads as the limit, not as JSON); the
    // daemon's own diagnostic and the result are what is left.
    let said = reason.map(ToString::to_string);
    let detail = said.as_deref().or(diagnostic).or(result);
    div()
        .flex()
        .items_start()
        .gap(px(9.0))
        .text_size(px(14.0))
        .child(div().pt(px(2.0)).child(icon(glyph, 15.0, tone)))
        .child(
            div()
                .text_color(tone)
                .font_weight(FontWeight::MEDIUM)
                .child(head),
        )
        .when_some(detail, |d, text| {
            d.child(
                div()
                    .min_w_0()
                    .text_color(theme.muted)
                    .child(SharedString::from(format!("\u{b7} {text}"))),
            )
        })
        .into_any_element()
}

impl Render for RunScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let transcript = div()
            .id("transcript")
            .flex_1()
            .min_h_0()
            .w_full()
            .max_w(px(COLUMN))
            .child(
                list(
                    self.list.clone(),
                    cx.processor(|this, ix, window, cx| this.render_row(ix, window, cx)),
                )
                .size_full(),
            );
        let dock = div()
            .flex_none()
            .w_full()
            .max_w(px(COLUMN))
            .pb(px(20.0))
            .pt(px(6.0))
            .child(self.steer.clone());
        let main = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .px(px(24.0))
            .child(transcript)
            .child(dock);

        div()
            .id("run")
            .size_full()
            .flex()
            .flex_col()
            .child(self.header(&theme, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(main)
                    .when(self.details_open, |d| d.child(self.details(&theme, cx))),
            )
    }
}

#[cfg(test)]
mod question_card_tests;
