//! Executors, a page of Settings (ui-brief 2026-11-19, settings): each agent CLI Reno drives, one
//! two-line row apiece in a card per group of what a person can do about it. The title says how
//! many can take runs; a row that blocks has one control that fixes it: Sign in…, Update,
//! Install / Install…, or Retry. Live through `executors.watch`. The ACP registry is the same
//! entity's other page, "Add from registry".
//!
//! Reno runs installs and updates as jobs (`executor.job.*`): the line shows them queued, running
//! and failed, and the view-model turns the daemon's job into words. An interactive step (a
//! sign-in) goes to Terminal through the injected [`crate::terminal`] opener. A daemon without
//! `executor.maintain` keeps today's Check again and managed Install and nothing else.

use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, Context, Div, Entity, EventEmitter, FontWeight, Hsla, IntoElement, Render,
    SharedString, Stateful, Subscription, Window, div, hsla, prelude::*, px,
};
use reno_proto::{ExecutorId, JobKind};

mod registry;

use self::registry::RegistryUi;
use super::ScreenEvent;
use crate::store::Store;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::page;
use crate::ui::theme::{self, Theme};
use crate::ui::toast;
use crate::vm::executors::{
    self, AutoView, Dot, ExecutorRow, ExecutorsView, Fact, Group, InstallOffer, JobState, Level,
    Need, RecipeView,
};

/// Which of the two Settings pages this entity draws.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Part {
    /// The executors Reno drives.
    #[default]
    Roster,
    /// Agents to add from the ACP registry.
    Registry,
}

pub(crate) struct ExecutorsScreen {
    store: Entity<Store>,
    _store: Subscription,
    /// Lines opened in place.
    expanded: BTreeSet<String>,
    /// First installs whose review panel is open.
    reviewing: BTreeSet<String>,
    /// Whether the Test executors group is open.
    test_open: bool,
    /// Executors with a command running in Terminal that Reno looks at again afterwards.
    terminal: BTreeSet<String>,
    /// Whether the once-a-second redraw for running jobs' clocks is going.
    ticking: bool,
    /// The ACP registry group: filter, search, open lines, forms and the keys editors.
    registry: RegistryUi,
    part: Part,
    scroll: gpui::ScrollHandle,
}

impl EventEmitter<ScreenEvent> for ExecutorsScreen {}

fn level_color(theme: &Theme, level: Level) -> gpui::Hsla {
    match level {
        Level::Good => theme.success,
        Level::Warn => theme.warning,
        Level::Bad => theme.danger,
        Level::Quiet => theme.faint,
    }
}

/// The info colour: "0.171.0 available", and a running job.
fn info_color(theme: &Theme) -> Hsla {
    if theme.appearance.is_dark() {
        hsla(0.58, 0.85, 0.68, 1.0)
    } else {
        hsla(0.59, 0.80, 0.40, 1.0)
    }
}

/// One fact of an executor's health: a dot, what it is, and the detail beside it.
fn fact_line(theme: &Theme, fact: &Fact) -> Div {
    let color = level_color(theme, fact.level);
    div()
        .flex()
        .items_start()
        .gap(px(8.0))
        .child(div().pt(px(6.0)).child(kit::dot(color, 7.0)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_wrap()
                .gap_x(px(7.0))
                .line_height(px(19.0))
                .child(
                    div()
                        .text_color(match fact.level {
                            Level::Good | Level::Quiet => theme.text,
                            Level::Warn | Level::Bad => color,
                        })
                        .child(fact.text),
                )
                .when_some(fact.detail.clone(), |d, detail| {
                    d.child(
                        div()
                            .min_w_0()
                            .text_size(px(12.5))
                            .text_color(theme.muted)
                            .child(SharedString::from(detail)),
                    )
                }),
        )
}

/// The three health lines of an executor: installed, version, signed in. Also shown inline on a
/// run that failed because of its executor.
pub(crate) fn health_lines(theme: &Theme, row: &ExecutorRow) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(fact_line(theme, &row.installed))
        .children(row.version.as_ref().map(|f| fact_line(theme, f)))
        .children(row.signed_in.as_ref().map(|f| fact_line(theme, f)))
}

fn need_tone(theme: &Theme, need: Option<Need>) -> Hsla {
    match need {
        Some(Need::SignIn | Need::Update) => theme.warning,
        Some(Need::Install | Need::Setup) => theme.danger,
        None => theme.faint,
    }
}

fn mono(text: impl Into<SharedString>) -> Div {
    div()
        .font_family(MONO)
        .text_size(px(12.0))
        .child(text.into())
}

impl ExecutorsScreen {
    pub(crate) fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
            expanded: BTreeSet::new(),
            reviewing: BTreeSet::new(),
            test_open: false,
            terminal: BTreeSet::new(),
            ticking: false,
            registry: RegistryUi::new(cx),
            part: Part::Roster,
            scroll: gpui::ScrollHandle::new(),
        }
    }

    /// Draws the roster or the registry page.
    pub(crate) fn set_part(&mut self, part: Part, cx: &mut Context<Self>) {
        if self.part != part {
            self.part = part;
            self.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
            cx.notify();
        }
    }

    pub(crate) fn view(&self, cx: &gpui::App) -> ExecutorsView {
        let store = self.store.read(cx);
        executors::build_with(
            store.state(),
            store.now_ms(),
            store.registry().listing.as_ref(),
        )
    }

    /// "Check again" for one executor, or for all of them.
    pub(crate) fn check(&mut self, id: Option<ExecutorId>, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, cx| store.refresh_executors(id, cx));
    }

    pub(crate) fn install(&mut self, id: ExecutorId, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, cx| store.install_executor(id, cx));
    }

    fn toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_owned());
        }
        cx.notify();
    }

    fn start(&mut self, id: &str, kind: JobKind, force: bool, cx: &mut Context<Self>) {
        let Ok(id) = id.parse::<ExecutorId>() else {
            return;
        };
        self.store.update(cx, |store, cx| {
            store.start_executor_job(id, kind, force, cx)
        });
    }

    fn cancel_job(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Ok(id) = id.parse::<ExecutorId>() {
            self.store
                .update(cx, |store, cx| store.cancel_executor_job(id, cx));
        }
    }

    fn set_auto(&mut self, id: &str, on: bool, verified_only: bool, cx: &mut Context<Self>) {
        if let Ok(id) = id.parse::<ExecutorId>() {
            self.store.update(cx, |store, cx| {
                store.set_executor_auto_update(id, on, verified_only, cx);
            });
        }
    }

    fn toggle_log(&mut self, id: &str, cx: &mut Context<Self>) {
        let Ok(id) = id.parse::<ExecutorId>() else {
            return;
        };
        let show = !self.store.read(cx).is_log_open(&id);
        if show {
            // The log is a block of the opened line.
            self.expanded.insert(id.to_string());
        }
        self.store
            .update(cx, |store, cx| store.show_job_log(id, show, cx));
    }

    /// Opens `command` in Terminal and waits for the person to come back.
    pub(crate) fn open_terminal(
        &mut self,
        id: &str,
        label: &str,
        command: &str,
        cx: &mut Context<Self>,
    ) {
        match crate::terminal::open(cx, label, command) {
            Ok(()) => {
                self.terminal.insert(id.to_owned());
            }
            Err(reason) => toast::push(cx, toast::Tone::Error, "Terminal", reason),
        }
        cx.notify();
    }

    /// Probes the executors that had a command in Terminal, and stops waiting for them.
    pub(crate) fn check_back(&mut self, cx: &mut Context<Self>) {
        // What the person ran in Terminal from a line, and what a sign-in sent them to.
        let mut ids: std::collections::BTreeSet<ExecutorId> = std::mem::take(&mut self.terminal)
            .iter()
            .filter_map(|id| id.parse().ok())
            .collect();
        ids.extend(
            self.store
                .update(cx, |store, _| store.take_signin_terminals()),
        );
        for id in ids {
            self.store
                .update(cx, |store, cx| store.recheck_after_sign_in(id, cx));
        }
        cx.notify();
    }

    /// The window came back to the front: look again at what the person did in Terminal.
    pub(crate) fn on_window_activated(&mut self, cx: &mut Context<Self>) {
        if !self.terminal.is_empty() || self.store.read(cx).waiting_on_signin_terminal() {
            self.check_back(cx);
        }
    }

    /// Redraws once a second while a job runs, so its clock counts. Only a daemon on the
    /// system clock has a clock to count (the fake's moves with its events).
    fn keep_ticking(&mut self, running: bool, cx: &mut Context<Self>) {
        if !running || self.ticking || !self.store.read(cx).clock_is_live() {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let go = this
                    .update(cx, |screen, cx| {
                        let running = screen.view(cx).jobs_running > 0;
                        if running {
                            cx.notify();
                        } else {
                            screen.ticking = false;
                        }
                        running
                    })
                    .unwrap_or(false);
                if !go {
                    break;
                }
            }
        })
        .detach();
    }

    // --- Elements ----------------------------------------------------------------------------

    fn check_button(&self, row: &ExecutorRow, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Ok(id) = row.id.parse::<ExecutorId>() else {
            return div().into_any_element();
        };
        let store = self.store.read(cx);
        let busy = store.is_checking(Some(&id)) || store.is_checking(None);
        let button = kit::line_button(
            theme,
            format!("check-{}", row.id),
            Some(Icon::PhRefresh),
            if busy {
                "Checking\u{2026}"
            } else {
                "Check again"
            },
            Tone::Quiet,
            !busy,
        );
        if busy {
            button
        } else {
            button.on_click(cx.listener(move |this, _, _, cx| this.check(Some(id.clone()), cx)))
        }
        .into_any_element()
    }

    /// The buttons at the right of a line.
    fn controls(&self, row: &ExecutorRow, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let id = row.id.clone();
        let starting = row
            .id
            .parse::<ExecutorId>()
            .is_ok_and(|e| self.store.read(cx).is_starting(&e));
        let line = div().flex_none().flex().items_center().gap(px(8.0));

        // A command is running in Terminal.
        if self.terminal.contains(&row.id) {
            let now = {
                kit::line_button(
                    theme,
                    format!("check-now-{id}"),
                    None,
                    "Check now",
                    Tone::Quiet,
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.check_back(cx)))
            };
            let stop = {
                let id = id.clone();
                kit::line_button(
                    theme,
                    format!("wait-cancel-{id}"),
                    None,
                    "Cancel",
                    Tone::Quiet,
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.terminal.remove(&id);
                    cx.notify();
                }))
            };
            return line.child(now).child(stop);
        }

        // A job in flight or failed owns the right side.
        if let Some(job) = &row.job {
            let log_open = row
                .id
                .parse::<ExecutorId>()
                .is_ok_and(|e| self.store.read(cx).is_log_open(&e));
            let show_log = {
                let id = id.clone();
                kit::line_button(
                    theme,
                    format!("log-{id}"),
                    Some(Icon::PhTerminal),
                    if log_open { "Hide log" } else { "Show log" },
                    Tone::Quiet,
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_log(&id, cx)))
            };
            let cancel = {
                let id = id.clone();
                kit::line_button(
                    theme,
                    format!("cancel-{id}"),
                    None,
                    "Cancel",
                    Tone::Quiet,
                    !starting,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.cancel_job(&id, cx)))
            };
            return match &job.state {
                JobState::Queued { .. } => {
                    let now = {
                        let id = id.clone();
                        kit::line_button(
                            theme,
                            format!("force-{id}"),
                            None,
                            "Update now anyway",
                            Tone::Quiet,
                            !starting,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.start_forced(&id, cx);
                        }))
                    };
                    line.child(now).child(cancel)
                }
                JobState::Running { .. } => line.child(show_log).child(cancel),
                JobState::Failed { stopped, .. } => {
                    let terminal =
                        job.terminal_command
                            .clone()
                            .filter(|_| !stopped)
                            .map(|command| {
                                let id = id.clone();
                                let label = format!("Run in Terminal: {}", row.name);
                                kit::line_button(
                                    theme,
                                    format!("terminal-{id}"),
                                    Some(Icon::PhTerminal),
                                    "Run in Terminal",
                                    Tone::Quiet,
                                    true,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.open_terminal(&id, &label, &command, cx);
                                    },
                                ))
                            });
                    let kind = job.kind;
                    let registry_id = row.registry.as_ref().map(|r| r.id.clone());
                    let retry = {
                        let id = id.clone();
                        kit::line_button(
                            theme,
                            format!("retry-{id}"),
                            Some(Icon::PhRefresh),
                            "Retry",
                            Tone::Primary,
                            !starting,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            match &registry_id {
                                Some(rid) => this.add_registry_agent(rid, cx),
                                None => this.start(&id, kind, false, cx),
                            }
                        }))
                    };
                    let dismiss = {
                        let id = id.clone();
                        kit::icon_button(theme, format!("dismiss-{id}"), Icon::PhX, 26.0)
                            .on_click(cx.listener(move |this, _, _, cx| this.dismiss(&id, cx)))
                    };
                    line.child(show_log)
                        .children(terminal)
                        .child(retry)
                        .child(dismiss)
                }
            };
        }

        // A daemon without `executor.maintain`: today's Check again, and a managed Install.
        if !row.maintain {
            let install = (row.install == Some(InstallOffer::Legacy)).then(|| {
                let installing = row
                    .id
                    .parse::<ExecutorId>()
                    .is_ok_and(|e| self.store.read(cx).is_installing(&e));
                let button = kit::line_button(
                    theme,
                    format!("install-{id}"),
                    Some(Icon::PhDownload),
                    if installing {
                        "Installing\u{2026}"
                    } else {
                        "Install"
                    },
                    Tone::Primary,
                    !installing,
                );
                let id = id.clone();
                if installing {
                    button
                } else {
                    button.on_click(cx.listener(move |this, _, _, cx| {
                        if let Ok(id) = id.parse::<ExecutorId>() {
                            this.install(id, cx);
                        }
                    }))
                }
            });
            return line
                .children(install)
                .child(self.check_button(row, theme, cx));
        }

        let mut line = line;
        if let Some(offer) = row.install {
            let (label, glyph) = match offer {
                InstallOffer::Review => ("Install\u{2026}", Icon::PhDownload),
                _ => ("Install", Icon::PhDownload),
            };
            let button = {
                let id = id.clone();
                kit::line_button(
                    theme,
                    format!("install-{id}"),
                    Some(glyph),
                    label,
                    Tone::Primary,
                    !starting,
                )
                .on_click(cx.listener(move |this, _, _, cx| match offer {
                    InstallOffer::Review => {
                        this.expanded.insert(id.clone());
                        if !this.reviewing.remove(&id) {
                            this.reviewing.insert(id.clone());
                        }
                        cx.notify();
                    }
                    _ => this.start(&id, JobKind::Install, false, cx),
                }))
            };
            line = line.child(button);
        }
        if let Some(command) = &row.sign_in {
            let command = command.clone();
            let label = format!("Sign in to {}", row.name);
            let id = id.clone();
            line = line.child(
                kit::line_button(
                    theme,
                    format!("sign-in-{id}"),
                    Some(Icon::PhTerminal),
                    "Sign in\u{2026}",
                    Tone::Primary,
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_terminal(&id, &label, &command, cx);
                })),
            );
        }
        if row.update {
            let id = id.clone();
            let registry_id = row.registry.as_ref().map(|r| r.id.clone());
            line = line.child(
                kit::line_button(
                    theme,
                    format!("update-{id}"),
                    Some(Icon::PhDownload),
                    "Update",
                    if row.update_primary {
                        Tone::Primary
                    } else {
                        Tone::Quiet
                    },
                    !starting,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    match &registry_id {
                        // A registry agent updates by adding it again.
                        Some(rid) => this.add_registry_agent(rid, cx),
                        None => this.start(&id, JobKind::Update, false, cx),
                    }
                })),
            );
        }
        // A registry agent that needs a key has no command to run: its keys are one click away.
        if row.registry.is_some() && row.need == Some(Need::SignIn) && row.sign_in.is_none() {
            let id = id.clone();
            line = line.child(
                kit::line_button(
                    theme,
                    format!("add-key-{id}"),
                    None,
                    "Add key\u{2026}",
                    Tone::Primary,
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.expanded.insert(id.clone());
                    cx.notify();
                })),
            );
        }
        if let Some(switch) = &row.switch {
            let (id, on) = (id.clone(), switch.on);
            let verified = row.auto.as_ref().is_some_and(|a| a.verified_only);
            line = line.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.muted)
                            .child("Auto-update"),
                    )
                    .child(kit::switch(theme, format!("auto-{id}"), on).on_click(
                        cx.listener(move |this, _, _, cx| this.set_auto(&id, !on, verified, cx)),
                    )),
            );
        }
        if row.lock {
            line = line.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .text_size(px(12.0))
                    .text_color(theme.faint)
                    .child(icon(Icon::PhLock, 13.0, theme.faint))
                    .child("Updates with Reno"),
            );
        }
        if row.registry.is_some() && row.job.is_none() {
            line = self.remove_controls(row, line, theme, cx);
        }
        line
    }

    fn dismiss(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Ok(id) = id.parse::<ExecutorId>() {
            self.store
                .update(cx, |store, cx| store.dismiss_executor_job(id, cx));
        }
    }

    fn start_forced(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Ok(id) = id.parse::<ExecutorId>() {
            self.store
                .update(cx, |store, cx| store.start_update_now(id, cx));
        }
    }

    /// The middle of a line.
    fn middle(&self, row: &ExecutorRow, theme: &Theme) -> Div {
        let info = info_color(theme);
        let cell = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .overflow_hidden()
            .text_size(px(13.0));

        if self.terminal.contains(&row.id) {
            return cell.child(icon(Icon::PhTerminal, 14.0, theme.muted)).child(
                div()
                    .text_color(theme.muted)
                    .child("Running in Terminal \u{2014} Reno checks again when you're back"),
            );
        }

        if let Some(job) = &row.job {
            let words = job.words(&row.name);
            return match &job.state {
                JobState::Queued { .. } => cell
                    .child(icon(Icon::PhClock, 14.0, theme.muted))
                    .child(div().text_color(theme.muted).child(words)),
                JobState::Running {
                    clock,
                    last_line,
                    long,
                    quiet_minutes,
                    ..
                } => cell
                    .child(kit::spinner(
                        SharedString::from(format!("spin-{}", row.id)),
                        14.0,
                        info,
                    ))
                    .child(
                        div()
                            .flex_none()
                            .text_color(info)
                            .font_weight(FontWeight::MEDIUM)
                            .child(words),
                    )
                    .children(
                        clock
                            .clone()
                            .map(|c| mono(c).flex_none().text_color(theme.muted)),
                    )
                    .when_some(last_line.clone(), |d, line| {
                        d.child(mono(line).min_w_0().truncate().text_color(theme.faint))
                    })
                    .when(*long && quiet_minutes.is_none(), |d| {
                        d.child(
                            div()
                                .flex_none()
                                .text_size(px(12.0))
                                .text_color(theme.muted)
                                .child("still running \u{2014} installers can take a few minutes"),
                        )
                    })
                    .when_some(*quiet_minutes, |d, minutes| {
                        d.child(
                            div()
                                .flex_none()
                                .text_size(px(12.0))
                                .text_color(theme.warning)
                                .child(format!("no output for {minutes} min")),
                        )
                    }),
                JobState::Failed {
                    detail, stopped, ..
                } => cell
                    .child(icon(
                        if *stopped { Icon::PhClock } else { Icon::PhX },
                        14.0,
                        if *stopped { theme.muted } else { theme.danger },
                    ))
                    .child(
                        div()
                            .flex_none()
                            .text_color(if *stopped { theme.muted } else { theme.danger })
                            .font_weight(FontWeight::MEDIUM)
                            .child(words),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.5))
                            .text_color(if *stopped { theme.faint } else { theme.danger })
                            .child(SharedString::from(detail.clone())),
                    ),
            };
        }

        match row.group {
            Group::Ready | Group::Test => {
                let cell = cell.child(
                    mono(
                        row.version_text
                            .clone()
                            .unwrap_or_else(|| "version unknown".into()),
                    )
                    .flex_none()
                    .text_color(if row.version_text.is_some() {
                        theme.text
                    } else {
                        theme.faint
                    }),
                );
                let cell = match &row.available {
                    Some(latest) => cell.child(
                        div()
                            .flex_none()
                            .text_size(px(12.0))
                            .text_color(info)
                            .child(format!("{latest} available")),
                    ),
                    None => cell,
                };
                let cell = match &row.account {
                    Some(account) => cell.child(
                        div()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .text_color(theme.muted)
                            .child(icon(Icon::PhCheck, 13.0, theme.success))
                            .child(div().truncate().child(SharedString::from(account.clone()))),
                    ),
                    None => cell,
                };
                cell.children(self.registry_note(row, theme))
            }
            Group::NeedsYou | Group::Unknown => match &row.middle {
                Some(fact) => {
                    let tone = if row.group == Group::Unknown {
                        theme.faint
                    } else {
                        need_tone(theme, row.need)
                    };
                    cell.child(icon(
                        if row.group == Group::Unknown {
                            Icon::PhQuestion
                        } else {
                            Icon::PhWarning
                        },
                        14.0,
                        tone,
                    ))
                    .child(div().flex_none().text_color(tone).child(fact.text))
                    .when_some(fact.detail.clone(), |d, detail| {
                        d.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.5))
                                .text_color(theme.muted)
                                .child(SharedString::from(detail)),
                        )
                    })
                }
                None => cell,
            },
            Group::NotInstalled => {
                if self.reviewing.contains(&row.id) && row.recipe.is_some() {
                    cell.child(icon(Icon::PhEye, 14.0, theme.muted)).child(
                        div()
                            .text_color(theme.muted)
                            .child("Review what runs below"),
                    )
                } else {
                    cell.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.muted)
                            .child(SharedString::from(row.source.clone().unwrap_or_default())),
                    )
                }
            }
        }
    }

    fn review_panel(
        &self,
        row: &ExecutorRow,
        recipe: &RecipeView,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let n = recipe.reno_steps();
        let starting = row
            .id
            .parse::<ExecutorId>()
            .is_ok_and(|e| self.store.read(cx).is_starting(&e));
        let steps = recipe.steps.iter().enumerate().map(|(i, step)| {
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(
                    div()
                        .flex_none()
                        .w(px(18.0))
                        .text_size(px(11.5))
                        .text_color(theme.faint)
                        .child(format!("{}", i + 1)),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(150.0))
                        .text_size(px(12.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted)
                        .child(SharedString::from(step.label.clone())),
                )
                .child(
                    mono(step.command.clone())
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .px(px(8.0))
                        .py(px(3.0))
                        .rounded(px(5.0))
                        .bg(theme.raised)
                        .text_color(theme.text),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.5))
                        .text_color(theme.faint)
                        .child(if step.reno {
                            "Reno runs it"
                        } else {
                            "you, in Terminal, after"
                        }),
                )
        });
        let run = {
            let id = row.id.clone();
            kit::line_button(
                theme,
                format!("run-{id}"),
                Some(Icon::PhPlay),
                if n == 1 {
                    "Run 1 command".to_owned()
                } else {
                    format!("Run {n} commands")
                },
                Tone::Primary,
                !starting && n > 0,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.reviewing.remove(&id);
                this.start(&id, JobKind::Install, false, cx);
            }))
        };
        let cancel = {
            let id = row.id.clone();
            kit::line_button(
                theme,
                format!("review-cancel-{id}"),
                None,
                "Cancel",
                Tone::Quiet,
                true,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.reviewing.remove(&id);
                cx.notify();
            }))
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(12.0))
            .rounded(px(10.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.hairline)
            .child(
                div()
                    .text_size(px(13.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if n == 1 {
                        "Reno will run 1 command as you, in your login shell. No sudo.".to_owned()
                    } else {
                        format!("Reno will run {n} commands as you, in your login shell. No sudo.")
                    }),
            )
            .child(div().flex().flex_col().gap(px(5.0)).children(steps))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    // The daemon keeps the last 500 lines in memory (`executor.job.log`); there is
                    // no log file, so the panel promises none.
                    .child(format!(
                        "Source: {} \u{b7} Its output shows under Show log while it runs",
                        recipe.source
                    )),
            )
            .child(div().flex().gap(px(8.0)).child(run).child(cancel))
    }

    fn log_block(
        &self,
        row: &ExecutorRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        self.log_block_for(&row.id, theme, cx)
    }

    /// The output of an executor's job, when its log is shown.
    fn log_block_for(
        &self,
        executor: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        let id = executor.parse::<ExecutorId>().ok()?;
        let store = self.store.read(cx);
        if !store.is_log_open(&id) {
            return None;
        }
        let log = store.job_log(&id).cloned().unwrap_or_default();
        let lines: Vec<AnyElement> = if log.lines.is_empty() {
            vec![
                div()
                    .text_color(theme.faint)
                    .child("No output yet")
                    .into_any_element(),
            ]
        } else {
            log.lines
                .iter()
                .map(|l| {
                    div()
                        .child(SharedString::from(l.clone()))
                        .into_any_element()
                })
                .collect()
        };
        Some(
            div()
                .id(SharedString::from(format!("job-log-{executor}")))
                .debug_selector({
                    let id = executor.to_owned();
                    move || format!("job-log-{id}")
                })
                .max_h(px(140.0))
                .overflow_y_scroll()
                .p(px(10.0))
                .rounded(px(8.0))
                .bg(theme.raised)
                .font_family(MONO)
                .text_size(px(12.0))
                .line_height(px(17.0))
                .text_color(theme.muted)
                .when(log.truncated, |d| {
                    d.child(
                        div()
                            .text_color(theme.faint)
                            .child("\u{2026} older output was dropped"),
                    )
                })
                .children(lines),
        )
    }

    fn auto_block(
        &self,
        row: &ExecutorRow,
        auto: &AutoView,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = row.id.clone();
        let on = auto.on;
        let verified = auto.verified_only;
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .text_size(px(12.5))
                    .line_height(px(18.0))
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::MEDIUM)
                            .child("Auto-update"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(theme.muted)
                            .child(SharedString::from(auto.text.clone())),
                    ),
            )
            .when(auto.can_pin, |d| {
                let id = id.clone();
                d.child(
                    kit::checkbox(
                        theme,
                        format!("verified-{id}"),
                        verified,
                        "Only versions Reno has verified",
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_auto(&id, on, !verified, cx);
                    })),
                )
            })
            .when_some(auto.warning.clone(), |d, warning| {
                d.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.warning)
                        .child(SharedString::from(warning)),
                )
            })
    }

    /// What opens under a line.
    fn expanded(&mut self, row: &ExecutorRow, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .pl(px(62.0))
            .pr(px(14.0))
            .pb(px(14.0));
        if self.reviewing.contains(&row.id)
            && let Some(recipe) = &row.recipe
        {
            body = body.child(self.review_panel(row, recipe, theme, cx));
        }
        if let Some(log) = self.log_block(row, theme, cx) {
            body = body.child(log);
        }
        body = body.child(health_lines(theme, row));
        if row.group == Group::NotInstalled
            && !self.reviewing.contains(&row.id)
            && let Some(recipe) = &row.recipe
        {
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .text_size(px(12.5))
                    .child(
                        div()
                            .text_color(theme.muted)
                            .child(SharedString::from(format!("Install: {}", recipe.source))),
                    )
                    .children(recipe.steps.iter().map(|s| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(150.0))
                                    .text_color(theme.muted)
                                    .child(SharedString::from(s.label.clone())),
                            )
                            .child(
                                mono(s.command.clone())
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme.text),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.5))
                                    .text_color(theme.faint)
                                    .child(if s.reno {
                                        "Reno can run it"
                                    } else {
                                        "needs you"
                                    }),
                            )
                    })),
            );
        }
        if let Some(auto) = &row.auto {
            body = body.child(self.auto_block(row, auto, theme, cx));
        }
        body = body.children(self.registry_row_body(row, theme, cx));
        if row.maintain {
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(self.check_button(row, theme, cx))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.faint)
                            .child(SharedString::from(row.checked.clone())),
                    ),
            );
        }
        body
    }

    fn line(
        &mut self,
        row: &ExecutorRow,
        last: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.expanded.contains(&row.id)
            || (self.reviewing.contains(&row.id) && row.recipe.is_some());
        let running = row.job.as_ref().is_some_and(|j| j.running());
        let dot = match row.dot {
            Dot::Ready => kit::dot(theme.success, 8.0),
            Dot::Blocked => kit::dot(theme.danger, 8.0),
            Dot::Unknown => kit::dot(theme.faint, 8.0),
            Dot::NotInstalled => div()
                .flex_none()
                .size(px(8.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(theme.faint)
                .border_dashed(),
            Dot::Running => div()
                .flex_none()
                .size(px(8.0))
                .rounded(px(4.0))
                .border_2()
                .border_color(info_color(theme)),
        };
        let toggle = {
            let id = row.id.clone();
            div()
                .id(SharedString::from(format!("line-{}", row.id)))
                .debug_selector({
                    let id = row.id.clone();
                    move || format!("line-{id}")
                })
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .items_center()
                .gap(px(10.0))
                .cursor_pointer()
                .child(icon(
                    if open {
                        Icon::PhCaretDown
                    } else {
                        Icon::PhCaretRight
                    },
                    13.0,
                    theme.faint,
                ))
                .child(dot)
                .child(kit::logo(theme, &row.id, 20.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(13.5))
                                .font_weight(FontWeight::MEDIUM)
                                .child(SharedString::from(row.name.clone())),
                        )
                        .child(self.middle(row, theme).text_size(px(12.5))),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(&id, cx)))
        };
        let confirm = self.remove_confirm(row, theme, cx);
        div()
            .id(SharedString::from(format!("executor-{}", row.id)))
            .w_full()
            .flex()
            .flex_col()
            .when(!last, |d| d.border_b_1().border_color(theme.hairline))
            .when(open, |d| d.bg(theme.row_selected()))
            .when(running && !open, |d| d.bg(info_color(theme).opacity(0.06)))
            .child(
                div()
                    .min_h(px(56.0))
                    .py(px(10.0))
                    .px(px(14.0))
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .text_size(px(13.0))
                    .children(match confirm {
                        Some(dialog) => vec![dialog],
                        None => vec![
                            toggle.into_any_element(),
                            self.controls(row, theme, cx).into_any_element(),
                        ],
                    }),
            )
            .when(open, |d| d.child(self.expanded(row, theme, cx)))
            .into_any_element()
    }

    fn group(
        &mut self,
        group: Group,
        rows: &[&ExecutorRow],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = group == Group::Test && !self.test_open;
        let label = div()
            .id(SharedString::from(format!("group-{}", group.label())))
            .debug_selector({
                let label = group.label();
                move || format!("group-{label}")
            })
            .flex()
            .items_center()
            .gap(px(8.0))
            .pb(px(10.0))
            .when(group == Group::Test, |d| {
                d.cursor_pointer()
                    .child(icon(
                        if collapsed {
                            Icon::PhCaretRight
                        } else {
                            Icon::PhCaretDown
                        },
                        12.0,
                        theme.faint,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.test_open = !this.test_open;
                        cx.notify();
                    }))
            })
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(group.label()),
            )
            .child(page::count(theme, rows.len(), page::CountTone::Soft))
            .child(div().flex_1())
            .children(
                group
                    .note()
                    .map(|n| div().text_size(px(12.5)).text_color(theme.faint).child(n)),
            );
        let count = rows.len();
        let lines: Vec<AnyElement> = if collapsed {
            vec![]
        } else {
            rows.iter()
                .enumerate()
                .map(|(i, r)| self.line(r, i + 1 == count, theme, cx))
                .collect()
        };
        div()
            .flex()
            .flex_col()
            .child(label)
            .when(!lines.is_empty(), |d| {
                d.child(page::table_frame(theme, page::table()).children(lines))
            })
            .into_any_element()
    }
}

impl Render for ExecutorsScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let view = self.view(cx);
        self.keep_ticking(view.jobs_running > 0, cx);
        let retry: page::Action = {
            let store = self.store.clone();
            Rc::new(move |_, cx| store.update(cx, |s, _| s.retry_now()))
        };
        let rows = view.rows.rows();
        self.prune_registry(&rows.iter().collect::<Vec<_>>());

        let checking_all = self.store.read(cx).is_checking(None);
        let check_all = kit::pill_button_enabled(
            &theme,
            "check-all",
            if checking_all {
                "Checking\u{2026}"
            } else {
                "Check all"
            },
            Tone::Quiet,
            !(checking_all || rows.is_empty()),
        );
        let check_all = if checking_all || rows.is_empty() {
            check_all
        } else {
            check_all.on_click(cx.listener(|this, _, _, cx| this.check(None, cx)))
        };
        let status = if view.jobs_running > 0 {
            Some(if view.jobs_running == 1 {
                "1 job running".to_owned()
            } else {
                format!("{} jobs running", view.jobs_running)
            })
        } else {
            view.checked.clone()
        };
        let update_all = (view.maintain && view.updates_waiting.len() > 1).then(|| {
            let ids = view.updates_waiting.clone();
            let registry_ids: BTreeSet<String> = view
                .rows
                .rows()
                .iter()
                .filter(|r| r.registry.is_some())
                .map(|r| r.id.clone())
                .collect();
            kit::pill_button(
                &theme,
                "update-all",
                format!("Update {}", ids.len()),
                Tone::Primary,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                for id in &ids {
                    // A registry agent updates by adding it again.
                    match id
                        .strip_prefix("acp.")
                        .filter(|_| registry_ids.contains(id))
                    {
                        Some(rid) => this.add_registry_agent(rid, cx),
                        None => this.start(id, JobKind::Update, false, cx),
                    }
                }
            }))
        });
        let trailing = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .children(status.map(|s| {
                div()
                    .text_size(px(12.5))
                    .text_color(theme.faint)
                    .child(SharedString::from(s))
            }))
            .children(update_all)
            .child(check_all);

        let blocked = view.needing > 0;
        let page_body = match self.part {
            Part::Roster => {
                let headline = div()
                    .flex()
                    .items_start()
                    .gap(px(16.0))
                    .child(
                        page::title(&theme, view.lede.clone(), view.detail.clone(), blocked)
                            .flex_1()
                            .min_w_0(),
                    )
                    .child(trailing.pt(px(2.0)));
                let groups: Vec<AnyElement> = Group::ALL
                    .iter()
                    .filter_map(|g| {
                        let in_group = view.group(*g);
                        (!in_group.is_empty()).then_some((*g, in_group))
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|(g, in_group)| self.group(g, &in_group, &theme, cx))
                    .collect();
                page::content(page::SETTINGS)
                    .gap(px(30.0))
                    .when_some(view.banner.as_ref(), |c, b| {
                        c.child(page::banner(&theme, b, Some(retry.clone())))
                    })
                    .child(headline)
                    .when_some(
                        page::note(
                            &theme,
                            &view.rows,
                            "The daemon lists no executors. Start it with an agent CLI installed.",
                        ),
                        ParentElement::child,
                    )
                    .children(groups)
            }
            Part::Registry => {
                let registry_group = self
                    .registry_view(cx)
                    .map(|registry| self.registry_group(&registry, &theme, cx));
                let missing = registry_group.is_none();
                page::content(page::SETTINGS)
                    .gap(px(30.0))
                    .when_some(view.banner.as_ref(), |c, b| {
                        c.child(page::banner(&theme, b, Some(retry.clone())))
                    })
                    .child(page::title(
                        &theme,
                        "Add from the ACP registry",
                        if missing {
                            "This daemon has no registry. Update Reno to add agents from it."
                        } else {
                            "Agents Reno can add and drive, checked before it offers Add."
                        },
                        false,
                    ))
                    .children(registry_group)
            }
        };

        div().id("executors").size_full().flex().flex_col().child(
            page::settings_column("executors-scroll")
                .track_scroll(&self.scroll)
                .child(page_body),
        )
    }
}
