//! The ACP registry group of the Executors page, and what an added registry agent's line has
//! that another executor's does not: Remove (confirmed in place), the keys editor, Update.
//! Locked UI brief 2026-11-18 ("Executors · add agents from the ACP registry").
//!
//! The view-model (`vm::registry`) decides what is shown; this file draws it and owns the
//! screen's own state: the filter, the search, which lines are open, the forms and the env
//! editors. Env values live in their field only: never in the store, a toast, a log or a
//! selector.

use std::collections::{BTreeMap, BTreeSet};

use gpui::{
    AnyElement, Context, Div, Entity, FontWeight, IntoElement, ParentElement, SharedString,
    Stateful, Subscription, Task, div, prelude::*, px,
};

use super::{ExecutorsScreen, info_color, mono};
use crate::store::{SigninPhase, SigninView};
use crate::summary;
use crate::ui::field::{Field, Look};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, Tone};
use crate::ui::theme::Theme;
use crate::vm::executors::{ExecutorRow, RegistryNote};
use crate::vm::registry::{
    self, Control, FactRow, FactTone, Filter, ListState, Phase, RegistryLine, RegistryView,
    SignInChoice, StepCard,
};
use reno_proto::ExecutorId;

/// Where an agent's environment editor is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EnvState {
    Loading,
    Ready,
    Saving,
    Saved,
    /// Why it could not be read or saved, in words that never hold a value.
    Failed(String),
}

pub(super) struct EnvEditor {
    field: Entity<Field>,
    state: EnvState,
    task: Option<Task<()>>,
    _sub: Subscription,
}

/// The "Your own agent\u{2026}" form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OwnState {
    Idle,
    Adding,
    Failed(String),
}

/// Everything the registry part of the screen holds.
pub(super) struct RegistryUi {
    pub(super) filter: Filter,
    pub(super) search: Entity<Field>,
    /// `RENO_REGISTRY_DEMO=signin:<kind>`: how every opened line says its agent signs in. The
    /// index does not say yet, so this is the only way to look at steps 2 of each kind.
    pub(super) demo_sign_in: Option<registry::SignIn>,
    /// Registry lines that are open, by registry id.
    pub(super) open: BTreeSet<String>,
    /// Added agents whose Remove is being confirmed, by executor id.
    pub(super) removing: BTreeSet<String>,
    pub(super) own_open: bool,
    pub(super) own_name: Entity<Field>,
    pub(super) own_command: Entity<Field>,
    pub(super) own_env: Entity<Field>,
    pub(super) own: OwnState,
    own_task: Option<Task<()>>,
    /// The keys editor of each added agent that has opened one, by registry id.
    pub(super) env: BTreeMap<String, EnvEditor>,
    _subs: Vec<Subscription>,
}

impl RegistryUi {
    pub(super) fn new(cx: &mut Context<ExecutorsScreen>) -> Self {
        let search = cx.new(|cx| {
            Field::new(
                "registry-search",
                "Search the registry",
                "Search the registry",
                Look::SEARCH,
                cx,
            )
        });
        let own_name = cx.new(|cx| {
            Field::new(
                "registry-own-name",
                "Name of your agent",
                "My agent",
                Look::LINE,
                cx,
            )
        });
        let own_command = cx.new(|cx| {
            Field::new(
                "registry-own-command",
                "Command that starts your agent",
                "my-agent --acp",
                Look::COMMAND,
                cx,
            )
        });
        let own_env = cx.new(|cx| {
            Field::new(
                "registry-own-env",
                "Environment of your agent, KEY=VALUE lines",
                "KEY=VALUE, one per line",
                Look::ENV,
                cx,
            )
        });
        let subs = [&search, &own_name, &own_command, &own_env]
            .into_iter()
            .map(|f| cx.observe(f, |_, _, cx| cx.notify()))
            .collect();
        Self {
            filter: Filter::All,
            demo_sign_in: None,
            search,
            open: BTreeSet::new(),
            removing: BTreeSet::new(),
            own_open: false,
            own_name,
            own_command,
            own_env,
            own: OwnState::Idle,
            own_task: None,
            env: BTreeMap::new(),
            _subs: subs,
        }
    }
}

/// Only web links are opened from a registry entry.
fn is_web_link(link: &str) -> bool {
    link.starts_with("https://") || link.starts_with("http://")
}

impl ExecutorsScreen {
    pub(crate) fn registry_view(&self, cx: &gpui::App) -> Option<RegistryView> {
        let store = self.store.read(cx);
        let mut view = registry::build(
            store.state(),
            store.registry(),
            self.registry.filter,
            self.registry.search.read(cx).text(),
            store.now_ms(),
        )?;
        if let Some(sign_in) = &self.registry.demo_sign_in {
            for line in view
                .lines
                .iter_mut()
                .filter(|l| matches!(l.phase, Phase::Available))
            {
                line.sign_in = sign_in.clone();
                line.steps = registry::steps_of(&line.name, sign_in, false, false);
            }
        }
        Some(view)
    }

    /// Puts the registry group in a state for a screenshot: comma-separated steps like
    /// `open:goose`, `filter:cant-run`, `search:go`, `signin:terminal|browser|keys|nothing`,
    /// `add:goose`, `remove:acp.cline`, `expand:acp.cline`, `offline` and `own`.
    pub(crate) fn registry_demo(&mut self, script: &str, cx: &mut Context<Self>) {
        for step in script.split(',') {
            let (verb, arg) = step.split_once(':').unwrap_or((step, ""));
            match verb {
                "open" => {
                    self.registry.open.insert(arg.to_owned());
                }
                "expand" => {
                    self.expanded.insert(arg.to_owned());
                }
                "filter" => {
                    self.registry.filter = [
                        Filter::All,
                        Filter::RunsOnceAdded,
                        Filter::NeedsSignIn,
                        Filter::CantRun,
                    ]
                    .into_iter()
                    .find(|f| f.key() == arg)
                    .unwrap_or(Filter::All);
                }
                "search" => {
                    let text = arg.to_owned();
                    self.registry
                        .search
                        .update(cx, |f, cx| f.set_text(&text, cx));
                }
                "signin" => {
                    self.registry.demo_sign_in = Some(match arg {
                        "terminal" => registry::SignIn::Terminal("auggie login".to_owned()),
                        "browser" => registry::SignIn::Browser,
                        "keys" => registry::SignIn::Keys(vec![
                            "ANTHROPIC_API_KEY".to_owned(),
                            "OPENAI_API_KEY".to_owned(),
                        ]),
                        _ => registry::SignIn::Nothing,
                    });
                }
                // `browser:acp.cline` holds a browser sign-in on its code; `browser-done:...` ends
                // one signed in. Nothing is opened: the page is muted.
                "browser" | "browser-done" => {
                    let Ok(id) = arg.parse::<ExecutorId>() else {
                        continue;
                    };
                    crate::links::mute();
                    let mut events = vec![
                        reno_proto::SigninEvent::Page {
                            url: "https://example.com/device".to_owned(),
                        },
                        reno_proto::SigninEvent::Output {
                            line: "Your code: ABCD-1234".to_owned(),
                        },
                    ];
                    if verb == "browser-done" {
                        events.push(reno_proto::SigninEvent::Done {
                            signed_in: true,
                            message: None,
                        });
                    }
                    let name = summary::executor_name(id.as_str());
                    self.store.update(cx, |store, cx| {
                        store.demo_script_signin(&id, events);
                        store.start_signin(id, name, None, cx);
                    });
                }
                "add" => self.add_registry_agent(arg, cx),
                "remove" => self.ask_remove(arg, cx),
                "offline" => {
                    self.store.read(cx).demo_registry_offline();
                    self.refresh_registry(cx);
                }
                "own" => self.registry.own_open = true,
                "scroll" if arg == "end" => self.scroll.scroll_to_bottom(),
                "scroll" => {
                    if let Ok(y) = arg.parse::<f32>() {
                        self.scroll.set_offset(gpui::point(px(0.0), px(-y)));
                    }
                }
                _ => {}
            }
        }
        cx.notify();
    }

    /// The screen was opened: read the registry once.
    pub(crate) fn shown(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| store.ensure_registry(cx));
    }

    pub(crate) fn refresh_registry(&mut self, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, cx| store.load_registry(true, cx));
    }

    pub(crate) fn add_registry_agent(&mut self, id: &str, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, cx| store.registry_add(id, cx));
    }

    fn toggle_registry_line(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.registry.open.remove(id) {
            self.registry.open.insert(id.to_owned());
        }
        cx.notify();
    }

    fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.registry.filter = filter;
        cx.notify();
    }

    // --- Remove ------------------------------------------------------------------------------

    fn ask_remove(&mut self, executor: &str, cx: &mut Context<Self>) {
        self.registry.removing.insert(executor.to_owned());
        cx.notify();
    }

    fn cancel_remove(&mut self, executor: &str, registry_id: &str, cx: &mut Context<Self>) {
        self.registry.removing.remove(executor);
        self.store.update(cx, |store, cx| {
            store.dismiss_registry_refusal(registry_id, cx)
        });
        cx.notify();
    }

    /// Forgets confirmations of agents that are gone.
    pub(super) fn prune_registry(&mut self, rows: &[&ExecutorRow]) {
        self.registry
            .removing
            .retain(|id| rows.iter().any(|r| &r.id == id));
    }

    fn remove_agent(&mut self, registry_id: &str, cx: &mut Context<Self>) {
        // The confirm stays on the line (as "Removing...", or the daemon's refusal) until the
        // agent is gone.
        self.store
            .update(cx, |store, cx| store.registry_remove(registry_id, cx));
        cx.notify();
    }

    // --- The keys editor ----------------------------------------------------------------------

    /// Opens an added agent's keys editor, reading its environment once.
    pub(super) fn open_env(&mut self, id: &str, vars: &[String], cx: &mut Context<Self>) {
        if self.registry.env.contains_key(id) {
            return;
        }
        // What the agent names is the placeholder: one KEY= line per variable.
        let placeholder = if vars.is_empty() {
            "KEY=VALUE, one per line".to_owned()
        } else {
            vars.iter()
                .map(|v| format!("{v}="))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let field = cx.new(|cx| {
            Field::new(
                format!("env-{id}"),
                "Environment, KEY=VALUE lines",
                placeholder,
                Look::ENV,
                cx,
            )
        });
        let owner = id.to_owned();
        // Any change of the text (typed, pasted, or set) re-arms Save.
        let sub = cx.observe(&field, move |this, _, cx| {
            if let Some(editor) = this.registry.env.get_mut(&owner)
                && matches!(editor.state, EnvState::Saved | EnvState::Failed(_))
            {
                editor.state = EnvState::Ready;
            }
            cx.notify();
        });
        let request = id.to_owned();
        let task = self.store.read(cx).registry_task(
            move |client| async move { client.registry_env_get(&request).await },
            cx,
        );
        let owner = id.to_owned();
        let load = cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                let Some(editor) = this.registry.env.get_mut(&owner) else {
                    return;
                };
                match result {
                    Ok(env) => {
                        editor.field.update(cx, |f, cx| f.set_text(&env, cx));
                        editor.state = EnvState::Ready;
                    }
                    Err(_) => {
                        editor.state =
                            EnvState::Failed("Could not read this agent's keys.".to_owned());
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self.registry.env.insert(
            id.to_owned(),
            EnvEditor {
                field,
                state: EnvState::Loading,
                task: Some(load),
                _sub: sub,
            },
        );
        cx.notify();
    }

    fn save_env(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(editor) = self.registry.env.get_mut(id) else {
            return;
        };
        if matches!(editor.state, EnvState::Loading | EnvState::Saving) {
            return;
        }
        // The value goes to the daemon and nowhere else.
        let env = editor.field.read(cx).text().to_owned();
        editor.state = EnvState::Saving;
        let (request, owner) = (id.to_owned(), id.to_owned());
        let task = self.store.read(cx).registry_task(
            move |client| async move { client.registry_env_set(&request, &env).await },
            cx,
        );
        let save = cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if let Some(editor) = this.registry.env.get_mut(&owner) {
                    editor.state = match result {
                        Ok(()) => EnvState::Saved,
                        // The daemon's reason is not shown: it is about a line of the text.
                        Err(_) => {
                            EnvState::Failed("Could not save. Each line is KEY=VALUE.".to_owned())
                        }
                    };
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(editor) = self.registry.env.get_mut(id) {
            editor.task = Some(save);
        }
        cx.notify();
    }

    // --- Your own agent -----------------------------------------------------------------------

    fn add_own(&mut self, cx: &mut Context<Self>) {
        let name = self.registry.own_name.read(cx).text().to_owned();
        let command = self.registry.own_command.read(cx).text().to_owned();
        if !registry::own_agent_ready(&name, &command) || self.registry.own == OwnState::Adding {
            return;
        }
        let env = self.registry.own_env.read(cx).text().to_owned();
        self.registry.own = OwnState::Adding;
        let task = self.store.read(cx).registry_task(
            move |client| async move { client.registry_add_own(&name, &command, &env).await },
            cx,
        );
        self.registry.own_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        for field in [
                            &this.registry.own_name,
                            &this.registry.own_command,
                            &this.registry.own_env,
                        ] {
                            field.update(cx, |f, cx| f.set_text("", cx));
                        }
                        this.registry.own = OwnState::Idle;
                        this.registry.own_open = false;
                        this.store.update(cx, |s, cx| s.load_registry(false, cx));
                    }
                    Err(reason) => this.registry.own = OwnState::Failed(reason),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // --- Elements -------------------------------------------------------------------------------

    /// The group: label and finding, the tools, the lines and your own agent.
    pub(super) fn registry_group(
        &mut self,
        view: &RegistryView,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = div()
            .id("group-registry")
            .debug_selector(|| "group-registry".to_owned())
            .flex()
            .items_baseline()
            .gap(px(8.0))
            .py(px(6.0))
            .px(px(8.0))
            .text_size(px(12.0))
            .text_color(theme.muted)
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .child("From the ACP registry"),
            )
            .when(view.state == ListState::Ready, |d| {
                d.child(
                    div()
                        .text_color(theme.faint)
                        .child(format!("{}", view.count)),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.faint)
                        .child(SharedString::from(format!("\u{b7} {}", view.finding))),
                )
            });

        let body: Vec<AnyElement> = match &view.state {
            ListState::Loading => vec![
                div()
                    .id("registry-loading")
                    .debug_selector(|| "registry-loading".to_owned())
                    .h(px(40.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(13.0))
                    .text_color(theme.muted)
                    .child(kit::spinner("registry-spin", 14.0, info_color(theme)))
                    .child("Loading the registry\u{2026}")
                    .into_any_element(),
            ],
            ListState::Failed(reason) => vec![
                div()
                    .id("registry-failed")
                    .debug_selector(|| "registry-failed".to_owned())
                    .min_h(px(40.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(13.0))
                    .child(icon(Icon::PhWarning, 14.0, theme.danger))
                    .child(div().flex_1().min_w_0().text_color(theme.danger).child(
                        SharedString::from(format!("Could not read the registry: {reason}")),
                    ))
                    .child(
                        kit::line_button(
                            theme,
                            "registry-retry",
                            Some(Icon::PhRefresh),
                            "Retry",
                            Tone::Primary,
                            true,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_registry(cx))),
                    )
                    .into_any_element(),
            ],
            ListState::Ready => {
                let mut out = vec![self.registry_tools(view, theme, cx)];
                if view.nothing_matches {
                    out.push(
                        div()
                            .id("registry-nothing")
                            .debug_selector(|| "registry-nothing".to_owned())
                            .h(px(40.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .text_size(px(13.0))
                            .text_color(theme.muted)
                            .child("Nothing matches")
                            .into_any_element(),
                    );
                }
                for line in &view.lines {
                    out.push(self.registry_line(line, theme, cx));
                }
                out.push(self.own_agent_line(theme, cx));
                out
            }
        };
        div()
            .flex()
            .flex_col()
            .child(label)
            .children(body)
            .into_any_element()
    }

    /// Search, the segmented filter, "registry checked 3m ago" and Refresh.
    fn registry_tools(
        &mut self,
        view: &RegistryView,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut filter = kit::segmented(theme);
        for segment in &view.segments {
            let which = segment.filter;
            filter = filter.child(
                kit::segment(
                    theme,
                    format!("registry-filter-{}", which.key()),
                    segment.label(),
                    segment.pressed,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_filter(which, cx))),
            );
        }
        let refresh = {
            let busy = view.refreshing;
            let button = kit::line_button(
                theme,
                "registry-refresh",
                Some(Icon::PhRefresh),
                if busy {
                    "Refreshing\u{2026}"
                } else {
                    "Refresh"
                },
                Tone::Quiet,
                !busy,
            );
            if busy {
                button
            } else {
                button.on_click(cx.listener(|this, _, _, cx| this.refresh_registry(cx)))
            }
        };
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.0))
            .py(px(6.0))
            .px(px(8.0))
            .child(self.registry.search.clone())
            .child(filter)
            .child(div().flex_1())
            .children(view.checked.clone().map(|text| {
                div()
                    .id("registry-checked")
                    .debug_selector(|| "registry-checked".to_owned())
                    .text_size(px(12.5))
                    .text_color(if view.offline {
                        theme.warning
                    } else {
                        theme.faint
                    })
                    .child(SharedString::from(text))
            }))
            .child(refresh)
            .into_any_element()
    }

    /// The dot of a registry line: dashed while not added, hollow while a job runs.
    fn registry_dot(&self, line: &RegistryLine, theme: &Theme) -> Div {
        match &line.phase {
            Phase::BuiltIn { .. } => kit::dot(theme.success, 8.0),
            Phase::Adding(_) => div()
                .flex_none()
                .size(px(8.0))
                .rounded(px(4.0))
                .border_2()
                .border_color(info_color(theme)),
            Phase::Failed(_) => kit::dot(theme.danger, 8.0),
            _ => div()
                .flex_none()
                .size(px(8.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(theme.faint)
                .border_dashed(),
        }
    }

    /// The middle of a line that is closed: version and description, or the add's progress.
    fn registry_middle(&self, line: &RegistryLine, open: bool, theme: &Theme) -> Div {
        let info = info_color(theme);
        let cell = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .overflow_hidden()
            .text_size(px(13.0));
        match &line.phase {
            Phase::Adding(job) => {
                let (clock, last_line) = match &job.state {
                    crate::vm::executors::JobState::Running {
                        clock, last_line, ..
                    } => (clock.clone(), last_line.clone()),
                    _ => (None, None),
                };
                let words = line.progress_words().unwrap_or_default();
                cell.child(kit::spinner(
                    SharedString::from(format!("registry-spin-{}", line.id)),
                    14.0,
                    info,
                ))
                .child(
                    div()
                        .flex_none()
                        .text_color(info)
                        .font_weight(FontWeight::MEDIUM)
                        .child(SharedString::from(words)),
                )
                .children(clock.map(|c| mono(c).flex_none().text_color(theme.muted)))
                .when_some(last_line, |d, l| {
                    d.child(mono(l).min_w_0().truncate().text_color(theme.faint))
                })
                .child(
                    div()
                        .flex_none()
                        .text_size(px(12.0))
                        .text_color(theme.muted)
                        .child("this can take a few minutes"),
                )
            }
            Phase::Failed(job) => {
                let detail = match &job.state {
                    crate::vm::executors::JobState::Failed { detail, .. } => detail.clone(),
                    _ => String::new(),
                };
                cell.child(icon(Icon::PhX, 14.0, theme.danger))
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.danger)
                            .font_weight(FontWeight::MEDIUM)
                            .child(SharedString::from(
                                line.progress_words().unwrap_or_default(),
                            )),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.5))
                            .text_color(theme.danger)
                            .child(SharedString::from(detail)),
                    )
            }
            Phase::BuiltIn { as_name } => cell
                .child(
                    mono(line.version.clone())
                        .flex_none()
                        .text_color(theme.text),
                )
                .child(div().min_w_0().truncate().text_color(theme.muted).child(
                    SharedString::from(format!("Reno drives this agent itself, as {as_name}")),
                )),
            _ => {
                let cell = cell.child(
                    mono(line.version.clone())
                        .flex_none()
                        .text_color(theme.text),
                );
                // While a line is open its description is in the body, not on the line.
                if open {
                    cell
                } else {
                    cell.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.muted)
                            .child(SharedString::from(line.description.clone())),
                    )
                }
            }
        }
    }

    fn registry_controls(
        &self,
        line: &RegistryLine,
        open: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = line.id.clone();
        let executor = line.executor.clone();
        let starting = executor
            .parse::<reno_proto::ExecutorId>()
            .is_ok_and(|e| self.store.read(cx).is_starting(&e));
        let controls = div().flex_none().flex().items_center().gap(px(8.0));
        match &line.phase {
            Phase::BuiltIn { .. } => controls.child(kit::tag(theme.muted, "Built in")),
            Phase::CantRun(why) => {
                // The reason is the faint text; there is nothing to press.
                let _ = why;
                controls.child(
                    div()
                        .max_w(px(260.0))
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(theme.faint)
                        .child(SharedString::from(line.needs.clone())),
                )
            }
            Phase::Adding(_) => {
                let log_open = executor
                    .parse::<reno_proto::ExecutorId>()
                    .is_ok_and(|e| self.store.read(cx).is_log_open(&e));
                let show_log = {
                    let (id, executor) = (id.clone(), executor.clone());
                    kit::line_button(
                        theme,
                        format!("registry-log-{id}"),
                        Some(Icon::PhTerminal),
                        if log_open { "Hide log" } else { "Show log" },
                        Tone::Quiet,
                        true,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_registry_log(&id, &executor, cx);
                    }))
                };
                let cancel = {
                    let (id, executor) = (id.clone(), executor.clone());
                    kit::line_button(
                        theme,
                        format!("registry-cancel-{id}"),
                        None,
                        "Cancel",
                        Tone::Quiet,
                        !starting,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.cancel_job(&executor, cx)))
                };
                controls.child(show_log).child(cancel)
            }
            Phase::Failed(_) => {
                let log_open = executor
                    .parse::<reno_proto::ExecutorId>()
                    .is_ok_and(|e| self.store.read(cx).is_log_open(&e));
                let show_log = {
                    let (id, executor) = (id.clone(), executor.clone());
                    kit::line_button(
                        theme,
                        format!("registry-log-{id}"),
                        Some(Icon::PhTerminal),
                        if log_open { "Hide log" } else { "Show log" },
                        Tone::Quiet,
                        true,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_registry_log(&id, &executor, cx);
                    }))
                };
                let retry = {
                    let id = id.clone();
                    kit::line_button(
                        theme,
                        format!("registry-retry-{id}"),
                        Some(Icon::PhRefresh),
                        "Retry",
                        Tone::Primary,
                        !starting,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.add_registry_agent(&id, cx);
                    }))
                };
                let dismiss = {
                    let (id, executor) = (id.clone(), executor.clone());
                    kit::icon_button(theme, format!("registry-dismiss-{id}"), Icon::PhX, 26.0)
                        .on_click(cx.listener(move |this, _, _, cx| this.dismiss(&executor, cx)))
                };
                controls.child(show_log).child(retry).child(dismiss)
            }
            // While a line is open Add moves to step 1 inside it.
            Phase::Available if open => controls,
            Phase::Available => {
                let id2 = id.clone();
                controls.child(
                    kit::line_button(
                        theme,
                        format!("registry-add-{id}"),
                        None,
                        "Add",
                        Tone::Primary,
                        !starting,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.add_registry_agent(&id2, cx);
                    })),
                )
            }
        }
    }

    fn toggle_registry_log(&mut self, id: &str, executor: &str, cx: &mut Context<Self>) {
        let Ok(executor) = executor.parse::<reno_proto::ExecutorId>() else {
            return;
        };
        let show = !self.store.read(cx).is_log_open(&executor);
        if show {
            self.registry.open.insert(id.to_owned());
        }
        self.store
            .update(cx, |store, cx| store.show_job_log(executor, show, cx));
        cx.notify();
    }

    /// One line of the group, closed or open.
    fn registry_line(
        &mut self,
        line: &RegistryLine,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = line.opens() && self.registry.open.contains(&line.id);
        let dim = matches!(line.phase, Phase::CantRun(_));
        let name_color = if dim { theme.muted } else { theme.text };
        let head = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                // A built-in line has no chevron: it does not open.
                div().w(px(13.0)).children(line.opens().then(|| {
                    icon(
                        if open {
                            Icon::PhCaretDown
                        } else {
                            Icon::PhCaretRight
                        },
                        13.0,
                        theme.faint,
                    )
                })),
            )
            .child(self.registry_dot(line, theme))
            .child(kit::registry_icon(
                theme,
                line.icon.as_deref(),
                &line.name,
                18.0,
            ))
            .child(
                div()
                    .flex_none()
                    .w(px(132.0))
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(name_color)
                    .child(SharedString::from(line.name.clone())),
            )
            .child(self.registry_middle(line, open, theme));
        let head: AnyElement = if line.opens() {
            let id = line.id.clone();
            head.id(SharedString::from(format!("registry-line-{}", line.id)))
                .debug_selector({
                    let id = line.id.clone();
                    move || format!("registry-line-{id}")
                })
                .cursor_pointer()
                .role(gpui::Role::Button)
                .aria_label(SharedString::from(line.name.clone()))
                .aria_expanded(open)
                .focusable()
                .tab_stop(true)
                .focus_visible(kit::focus_ring(theme))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_registry_line(&id, cx)))
                .into_any_element()
        } else {
            head.into_any_element()
        };
        // What it needs, faint, between the middle and the control.
        let needs = (!line.needs.is_empty() && matches!(line.phase, Phase::Available)).then(|| {
            div()
                .flex_none()
                .text_size(px(12.0))
                .text_color(theme.faint)
                .child(SharedString::from(line.needs.clone()))
        });
        div()
            .id(SharedString::from(format!("registry-agent-{}", line.id)))
            .debug_selector({
                let id = line.id.clone();
                move || format!("registry-agent-{id}")
            })
            .w_full()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(theme.hairline)
            .when(open, |d| d.bg(theme.row_selected()))
            .child(
                div()
                    .h(px(40.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .text_size(px(13.0))
                    .child(head)
                    .children(needs)
                    .child(self.registry_controls(line, open, theme, cx)),
            )
            .when(open, |d| d.child(self.registry_body(line, theme, cx)))
            .into_any_element()
    }

    /// What opens under a line: the sentence, the facts, the numbered steps.
    fn registry_body(&self, line: &RegistryLine, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .pl(px(56.0))
            .pr(px(14.0))
            .pb(px(14.0))
            .max_w(px(56.0 + 760.0 + 14.0));
        if !line.description.is_empty() {
            body = body.child(
                div()
                    .text_size(px(13.0))
                    .line_height(px(19.0))
                    .child(SharedString::from(line.description.clone())),
            );
        }
        body = body.child(
            div().flex().flex_col().gap(px(5.0)).children(
                line.facts
                    .iter()
                    .map(|f| self.fact_row(&line.id, f, theme, cx)),
            ),
        );
        if let Some(why) = &line.cant_run {
            body = body.child(
                div()
                    .id(SharedString::from(format!("registry-cant-run-{}", line.id)))
                    .debug_selector({
                        let id = line.id.clone();
                        move || format!("registry-cant-run-{id}")
                    })
                    .flex()
                    .items_start()
                    .gap(px(8.0))
                    .text_size(px(13.0))
                    .text_color(theme.danger)
                    .child(icon(Icon::PhX, 14.0, theme.danger))
                    .child(SharedString::from(why.clone())),
            );
        }
        if let Phase::Failed(_) | Phase::Adding(_) = line.phase
            && let Some(log) = self.log_block_for(&line.executor, theme, cx)
        {
            body = body.child(log);
        }
        if !line.steps.is_empty() {
            body = body.child(
                div().flex().flex_col().gap(px(8.0)).children(
                    line.steps
                        .iter()
                        .map(|step| self.step(line, step, theme, cx)),
                ),
            );
        }
        body
    }

    fn fact_row(&self, id: &str, fact: &FactRow, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let color = match fact.tone {
            FactTone::Plain => theme.text,
            FactTone::Warning => theme.warning,
        };
        let value = div()
            .flex_1()
            .min_w_0()
            .text_size(px(12.5))
            .text_color(color)
            .when(fact.mono, |d| d.font_family(kit::MONO).text_size(px(12.0)))
            .child(SharedString::from(fact.value.clone()));
        let value: AnyElement = match &fact.link {
            Some(link) if is_web_link(link) => {
                let link = link.clone();
                value
                    .id(SharedString::from(format!("registry-source-{id}")))
                    .debug_selector({
                        let id = id.to_owned();
                        move || format!("registry-source-{id}")
                    })
                    .text_color(info_color(theme))
                    .underline()
                    .cursor_pointer()
                    .role(gpui::Role::Link)
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(kit::focus_ring(theme))
                    .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&link)))
                    .into_any_element()
            }
            _ => value.into_any_element(),
        };
        div()
            .flex()
            .gap(px(12.0))
            .child(
                div()
                    .flex_none()
                    .w(px(120.0))
                    .text_size(px(12.5))
                    .text_color(theme.muted)
                    .child(fact.key),
            )
            .child(value)
    }

    /// A numbered step card with its control.
    fn step(
        &self,
        line: &RegistryLine,
        step: &StepCard,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = line.id.clone();
        let control: Option<AnyElement> = match &step.control {
            Control::Add => {
                let button = kit::line_button(
                    theme,
                    format!("registry-step-add-{id}"),
                    None,
                    "Add",
                    Tone::Primary,
                    step.enabled,
                );
                Some(if step.enabled {
                    button
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.add_registry_agent(&id, cx);
                        }))
                        .into_any_element()
                } else {
                    button.into_any_element()
                })
            }
            Control::OpenTerminal(_) => Some(
                kit::line_button(
                    theme,
                    format!("registry-step-terminal-{id}"),
                    Some(Icon::PhTerminal),
                    "Open Terminal",
                    Tone::Quiet,
                    false,
                )
                .into_any_element(),
            ),
            Control::SignInBrowser => Some(
                kit::line_button(
                    theme,
                    format!("registry-step-signin-{id}"),
                    None,
                    "Sign in\u{2026}",
                    Tone::Quiet,
                    false,
                )
                .into_any_element(),
            ),
            Control::Check => Some(
                kit::line_button(
                    theme,
                    format!("registry-step-check-{id}"),
                    None,
                    "Check",
                    Tone::Quiet,
                    false,
                )
                .into_any_element(),
            ),
            Control::Keys | Control::None => None,
        };
        let card = kit::step_card(
            theme,
            format!("registry-step-{}-{}", line.id, step.number),
            step.number,
            step.title,
            SharedString::from(step.detail.clone()),
            control,
            step.enabled,
        );
        match &step.command {
            Some(command) => div()
                .flex()
                .flex_col()
                .child(card)
                .child(
                    mono(command.clone())
                        .ml(px(46.0))
                        .mt(px(6.0))
                        .px(px(8.0))
                        .py(px(3.0))
                        .rounded(px(5.0))
                        .bg(theme.raised)
                        .self_start(),
                )
                .into_any_element(),
            None => card.into_any_element(),
        }
    }

    /// "Your own agent\u{2026}": the last line, with its form.
    fn own_agent_line(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let open = self.registry.own_open;
        let head = div()
            .id("registry-own-line")
            .debug_selector(|| "registry-own-line".to_owned())
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer()
            .role(gpui::Role::Button)
            .aria_label("Your own agent")
            .aria_expanded(open)
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .child(icon(
                if open {
                    Icon::PhCaretDown
                } else {
                    Icon::PhCaretRight
                },
                13.0,
                theme.faint,
            ))
            .child(icon(Icon::Plus, 14.0, theme.muted))
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .child("Your own agent\u{2026}"),
            )
            .child(
                div()
                    .text_color(theme.muted)
                    .child("a command you run yourself"),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.registry.own_open = !this.registry.own_open;
                cx.notify();
            }));
        let mut line = div()
            .id("registry-own")
            .w_full()
            .flex()
            .flex_col()
            .when(open, |d| d.bg(theme.row_selected()))
            .child(
                div()
                    .h(px(40.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .text_size(px(13.0))
                    .child(head),
            );
        if open {
            line = line.child(self.own_agent_form(theme, cx));
        }
        line.into_any_element()
    }

    fn own_agent_form(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let name = self.registry.own_name.read(cx).text().to_owned();
        let command = self.registry.own_command.read(cx).text().to_owned();
        let ready = registry::own_agent_ready(&name, &command);
        let adding = self.registry.own == OwnState::Adding;
        let labelled = |label: &'static str, field: AnyElement| {
            div()
                .flex()
                .items_start()
                .gap(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .w(px(120.0))
                        .pt(px(6.0))
                        .text_size(px(12.5))
                        .text_color(theme.muted)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(field))
        };
        let add = {
            let button = kit::line_button(
                theme,
                "registry-own-add",
                None,
                if adding { "Adding\u{2026}" } else { "Add" },
                Tone::Primary,
                ready && !adding,
            );
            if ready && !adding {
                button.on_click(cx.listener(|this, _, _, cx| this.add_own(cx)))
            } else {
                button
            }
        };
        let cancel = kit::line_button(
            theme,
            "registry-own-cancel",
            None,
            "Cancel",
            Tone::Quiet,
            true,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.registry.own_open = false;
            this.registry.own = OwnState::Idle;
            cx.notify();
        }));
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .pl(px(56.0))
            .pr(px(14.0))
            .pb(px(14.0))
            .max_w(px(56.0 + 760.0 + 14.0))
            .child(labelled("Name", self.registry.own_name.clone().into_any_element()))
            .child(labelled(
                "Command",
                self.registry.own_command.clone().into_any_element(),
            ))
            .child(labelled(
                "Environment",
                self.registry.own_env.clone().into_any_element(),
            ))
            .child(
                div()
                    .pl(px(132.0))
                    .text_size(px(12.0))
                    .text_color(theme.faint)
                    .child(
                        "Reno never installs or updates such an agent. Its environment stays on this Mac, for this agent only.",
                    ),
            )
            .when_some(
                match &self.registry.own {
                    OwnState::Failed(reason) => Some(reason.clone()),
                    _ => None,
                },
                |d, reason| {
                    d.child(
                        div()
                            .id("registry-own-failed")
                            .debug_selector(|| "registry-own-failed".to_owned())
                            .pl(px(132.0))
                            .text_size(px(12.5))
                            .text_color(theme.danger)
                            .child(SharedString::from(reason)),
                    )
                },
            )
            .child(div().pl(px(132.0)).flex().gap(px(8.0)).child(add).child(cancel))
    }

    // --- An added registry agent's line ---------------------------------------------------------

    /// The Remove button, and its confirmation in place of the line's middle and controls.
    pub(super) fn remove_controls(
        &self,
        row: &ExecutorRow,
        line: Div,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let Some(reg) = &row.registry else {
            return line;
        };
        let executor = row.id.clone();
        let removing = self.store.read(cx).is_removing(&reg.id);
        let button = {
            let button = kit::line_button(
                theme,
                format!("remove-{executor}"),
                None,
                if removing {
                    "Removing\u{2026}"
                } else {
                    "Remove"
                },
                Tone::Quiet,
                !removing,
            );
            if removing {
                button
            } else {
                button.on_click(cx.listener(move |this, _, _, cx| this.ask_remove(&executor, cx)))
            }
        };
        line.child(button)
    }

    /// The confirm, an alert dialog on the line itself.
    pub(super) fn remove_confirm(
        &self,
        row: &ExecutorRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let reg = row.registry.as_ref()?;
        if !self.registry.removing.contains(&row.id) {
            return None;
        }
        let executor = row.id.clone();
        let cancel = {
            let (executor, registry_id) = (executor.clone(), reg.id.clone());
            kit::line_button(
                theme,
                format!("remove-cancel-{executor}"),
                None,
                "Cancel",
                Tone::Quiet,
                true,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.cancel_remove(&executor, &registry_id, cx);
            }))
        };
        let store = self.store.read(cx);
        let (words, confirm, can_cancel): (String, Option<Stateful<Div>>, bool) =
            if let Some(why) = store.registry_refusal(&reg.id) {
                // The daemon refused (a run started since): the line says so.
                (format!("Could not remove {}: {why}", row.name), None, true)
            } else if reg.remove_blocked {
                (registry::remove_refused_words(&row.name), None, true)
            } else if store.is_removing(&reg.id) {
                (format!("Removing {}...", row.name), None, false)
            } else {
                let registry_id = reg.id.clone();
                let executor = executor.clone();
                (
                    registry::remove_words(&row.name),
                    Some(
                        kit::line_button(
                            theme,
                            format!("remove-confirm-{executor}"),
                            None,
                            "Remove",
                            Tone::Danger,
                            true,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.remove_agent(&registry_id, cx);
                        })),
                    ),
                    true,
                )
            };
        Some(
            div()
                .id(SharedString::from(format!("remove-dialog-{executor}")))
                .debug_selector({
                    let executor = executor.clone();
                    move || format!("remove-dialog-{executor}")
                })
                .role(gpui::Role::AlertDialog)
                .px(px(8.0))
                .aria_label(SharedString::from(words.clone()))
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(icon(Icon::PhWarning, 14.0, theme.danger))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(13.0))
                        .child(SharedString::from(words)),
                )
                .children(can_cancel.then_some(cancel))
                .children(confirm)
                .into_any_element(),
        )
    }

    /// What an added agent's opened line has: its keys and, where there is a way, the sign-in.
    pub(super) fn registry_row_body(
        &mut self,
        row: &ExecutorRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(reg) = row.registry.clone() else {
            return vec![];
        };
        let mut out = vec![];
        if let Some(text) = self.store.read(cx).registry_refusal(&reg.id) {
            out.push(
                div()
                    .id(SharedString::from(format!("remove-refused-{}", row.id)))
                    .debug_selector({
                        let id = row.id.clone();
                        move || format!("remove-refused-{id}")
                    })
                    .text_size(px(12.5))
                    .text_color(theme.danger)
                    .child(SharedString::from(format!(
                        "Could not remove {}: {text}",
                        row.name
                    )))
                    .into_any_element(),
            );
        }
        let choices = reg.choices.clone();
        let mut env_vars: Option<(Vec<String>, Option<String>)> = None;
        let mut browsers = 0;
        for choice in &choices {
            match choice {
                SignInChoice::Terminal { command } => {
                    out.push(self.terminal_card(row, command, theme, cx));
                }
                SignInChoice::Browser { method, .. } => {
                    // The agent's own sign-in needs the daemon to run it.
                    if self.store.read(cx).supports_browser_sign_in() {
                        out.push(self.browser_card(row, method.clone(), browsers, theme, cx));
                        browsers += 1;
                    }
                }
                SignInChoice::Keys { vars, link } => {
                    env_vars.get_or_insert_with(|| (vars.clone(), link.clone()));
                }
                SignInChoice::Nothing => out.push(
                    kit::step_card(
                        theme,
                        format!("registry-signin-card-{}", row.id),
                        2,
                        "Nothing to sign in",
                        format!("{} starts without an account or a key.", row.name),
                        None,
                        false,
                    )
                    .into_any_element(),
                ),
                SignInChoice::Setup(text) => out.push(
                    kit::step_card(
                        theme,
                        format!("registry-signin-card-{}", row.id),
                        2,
                        "Set it up by hand",
                        text.clone(),
                        None,
                        false,
                    )
                    .into_any_element(),
                ),
            }
        }
        // No facts about how it signs in: the generic command of the maintenance record when
        // it has one, and always the keys.
        if choices.is_empty()
            && let Some(command) = row.sign_in.clone()
        {
            out.push(self.terminal_card(row, &command, theme, cx));
        }
        // The keys are one click away on any agent that does not say it needs nothing.
        if !choices.iter().all(|c| *c == SignInChoice::Nothing) || choices.is_empty() {
            out.push(self.env_card(row, env_vars, theme, cx));
        }
        out
    }

    /// "Sign in, in Terminal": the agent's own command and [Open Terminal].
    fn terminal_card(
        &mut self,
        row: &ExecutorRow,
        command: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = format!("Sign in to {}", row.name);
        let (id, run) = (row.id.clone(), command.to_owned());
        let open = kit::line_button(
            theme,
            format!("registry-open-terminal-{}", row.id),
            Some(Icon::PhTerminal),
            "Open Terminal",
            Tone::Quiet,
            true,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.open_terminal(&id, &label, &run, cx);
        }));
        div()
            .flex()
            .flex_col()
            .child(kit::step_card(
                theme,
                format!("registry-signin-card-{}", row.id),
                2,
                "Sign in, in Terminal",
                "Reno opens Terminal with the agent's own command.",
                Some(open.into_any_element()),
                true,
            ))
            .child(
                mono(command.to_owned())
                    .ml(px(46.0))
                    .mt(px(6.0))
                    .px(px(8.0))
                    .py(px(3.0))
                    .rounded(px(5.0))
                    .bg(theme.raised)
                    .self_start(),
            )
            .into_any_element()
    }

    /// "Sign in, in your browser": the agent's own ACP `authenticate`, run by the daemon
    /// (`executor.signin.start`). The page opens here, the code it prints shows under the card.
    fn browser_card(
        &mut self,
        row: &ExecutorRow,
        method: Option<String>,
        n: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Ok(executor) = row.id.parse::<ExecutorId>() else {
            return div().into_any_element();
        };
        let view = self.store.read(cx).signin(&executor).cloned();
        let running = view.as_ref().is_some_and(SigninView::running);
        let suffix = if n == 0 {
            String::new()
        } else {
            format!("-{n}")
        };
        let button = if running {
            let stop = executor.clone();
            kit::line_button(
                theme,
                format!("registry-browser-cancel-{}{suffix}", row.id),
                None,
                "Cancel",
                Tone::Quiet,
                true,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let stop = stop.clone();
                this.store
                    .update(cx, |store, cx| store.cancel_signin(&stop, cx));
            }))
        } else {
            let (start, name) = (executor.clone(), row.name.clone());
            kit::line_button(
                theme,
                format!("registry-browser-signin-{}{suffix}", row.id),
                None,
                "Sign in\u{2026}",
                Tone::Quiet,
                true,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let (start, name, method) = (start.clone(), name.clone(), method.clone());
                this.store
                    .update(cx, |store, cx| store.start_signin(start, name, method, cx));
            }))
        };
        let card = kit::step_card(
            theme,
            format!("registry-browser-card-{}{suffix}", row.id),
            2,
            "Sign in, in your browser",
            "The agent's own sign-in opens in your browser.",
            Some(button.into_any_element()),
            true,
        );
        let mut col = div().flex().flex_col().gap(px(6.0)).child(card);
        if let Some(view) = view {
            let mut notes = div().flex().flex_col().gap(px(4.0)).ml(px(46.0));
            if view.page {
                notes = notes.child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.muted)
                        .child("Opened the page in your browser. Type this code there:")
                        .when(view.lines.is_empty(), |d| {
                            d.child(" waiting for the agent\u{2026}")
                        }),
                );
            } else if view.running() {
                notes = notes.child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.muted)
                        .child("Starting the sign-in\u{2026}"),
                );
            }
            if !view.lines.is_empty() {
                notes = notes.child(
                    div()
                        .id(SharedString::from(format!(
                            "registry-signin-lines-{}",
                            row.id
                        )))
                        .debug_selector({
                            let id = row.id.clone();
                            move || format!("registry-signin-lines-{id}")
                        })
                        .flex()
                        .flex_col()
                        .px(px(8.0))
                        .py(px(5.0))
                        .rounded(px(5.0))
                        .bg(theme.raised)
                        .self_start()
                        .children(view.lines.iter().map(|l| mono(l.clone()))),
                );
            }
            if let Some(command) = &view.terminal {
                notes = notes.child(div().text_size(px(12.5)).text_color(theme.muted).child(
                    SharedString::from(format!("Opened Terminal to run {command}.")),
                ));
            }
            if let SigninPhase::Failed(why) = &view.phase {
                notes = notes.child(
                    div()
                        .id(SharedString::from(format!(
                            "registry-signin-failed-{}",
                            row.id
                        )))
                        .debug_selector({
                            let id = row.id.clone();
                            move || format!("registry-signin-failed-{id}")
                        })
                        .text_size(px(12.5))
                        .text_color(theme.danger)
                        .child(SharedString::from(why.clone())),
                );
            }
            col = col.child(notes);
        }
        col.into_any_element()
    }

    /// "Give it its keys": the environment editor of an added agent.
    fn env_card(
        &mut self,
        row: &ExecutorRow,
        named: Option<(Vec<String>, Option<String>)>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(reg) = &row.registry else {
            return div().into_any_element();
        };
        let id = reg.id.clone();
        let (vars, link) = named.unzip();
        self.open_env(&id, vars.as_deref().unwrap_or_default(), cx);
        let Some(editor) = self.registry.env.get(&id) else {
            return div().into_any_element();
        };
        let state = editor.state.clone();
        let field = editor.field.clone();
        let busy = matches!(state, EnvState::Loading | EnvState::Saving);
        let save = {
            let id = id.clone();
            let button = kit::line_button(
                theme,
                format!("env-save-{}", row.id),
                None,
                match state {
                    EnvState::Saving => "Saving\u{2026}",
                    EnvState::Saved => "Saved",
                    _ => "Save",
                },
                Tone::Primary,
                !busy && state != EnvState::Saved,
            );
            if busy || state == EnvState::Saved {
                button
            } else {
                button.on_click(cx.listener(move |this, _, _, cx| this.save_env(&id, cx)))
            }
        };
        let status = match &state {
            EnvState::Loading => Some(("Reading\u{2026}".to_owned(), theme.faint)),
            EnvState::Failed(reason) => Some((reason.clone(), theme.danger)),
            EnvState::Saved => Some((
                "Saved. It is read the next time the agent starts.".to_owned(),
                theme.muted,
            )),
            _ => None,
        };
        let card = div()
            .id(SharedString::from(format!("env-card-{}", row.id)))
            .debug_selector({
                let id = row.id.clone();
                move || format!("env-card-{id}")
            })
            .w_full()
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
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("Give it its keys"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .line_height(px(18.0))
                                    .text_color(theme.muted)
                                    .child(SharedString::from(match link.flatten() {
                                        Some(link) => format!(
                                            "KEY=VALUE lines. Get a key at {link}. Kept in Reno's environment for this agent only, never in the agent's config, never logged."
                                        ),
                                        None => "KEY=VALUE lines. Kept in Reno's environment for this agent only, never in the agent's config, never logged.".to_owned(),
                                    })),
                            ),
                    )
                    .child(save),
            )
            .child(field)
            .children(status.map(|(text, color)| {
                div()
                    .id(SharedString::from(format!("env-status-{}", row.id)))
                    .debug_selector({
                        let id = row.id.clone();
                        move || format!("env-status-{id}")
                    })
                    .text_size(px(12.0))
                    .text_color(color)
                    .child(SharedString::from(text))
            }));
        card.into_any_element()
    }

    /// The note after a registry agent's version on its line: where it came from.
    pub(super) fn registry_note(&self, row: &ExecutorRow, theme: &Theme) -> Option<Div> {
        let reg = row.registry.as_ref()?;
        let color = match reg.note {
            RegistryNote::Delisted => theme.warning,
            _ => theme.faint,
        };
        Some(
            div()
                .flex_none()
                .text_size(px(12.0))
                .text_color(color)
                .child(format!("\u{b7} {}", reg.note.words())),
        )
    }
}

#[cfg(test)]
impl ExecutorsScreen {
    pub(crate) fn search_field(&self) -> &Entity<Field> {
        &self.registry.search
    }

    pub(crate) fn env_field(&self, id: &str) -> Option<&Entity<Field>> {
        self.registry.env.get(id).map(|e| &e.field)
    }

    pub(crate) fn env_saved(&self, id: &str) -> bool {
        self.registry
            .env
            .get(id)
            .is_some_and(|e| e.state == EnvState::Saved)
    }

    pub(crate) fn env_failure(&self, id: &str) -> Option<String> {
        match self.registry.env.get(id).map(|e| &e.state) {
            Some(EnvState::Failed(why)) => Some(why.clone()),
            _ => None,
        }
    }

    pub(crate) fn own_fields(&self) -> (Entity<Field>, Entity<Field>) {
        (
            self.registry.own_name.clone(),
            self.registry.own_command.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::is_web_link;

    #[test]
    fn only_web_links_are_opened_from_a_registry_entry() {
        assert!(is_web_link("https://github.com/a/b") && is_web_link("http://example.com"));
        assert!(!is_web_link("file:///etc/passwd") && !is_web_link("javascript:alert(1)"));
    }
}
