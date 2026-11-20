//! The setup entity: where a person is in first-run setup, what each click does, and the events
//! the root view hears when it ends. The three variants draw it (`wizard`, `checklist`, `page`);
//! none has a rule of its own. Each choice applies as it is made: the look at once, a sign-in
//! or an install as the existing executor action, the project as the new-thread project, so
//! Skip at any point leaves nothing half-done.

use std::path::PathBuf;

use gpui::{Context, Entity, EventEmitter, ScrollHandle, Subscription};
use reno_proto::{ExecutorId, JobKind};

use crate::prefs::{self, AccentName, Mode, ThemeName};
use crate::store::Store;
use crate::summary;
use crate::vm::executors;
use crate::vm::first_run::{self, Action, AgentLine, Item, ProjectLine, SignInRoute, Step};

/// What the root view hears.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FlowEvent {
    /// Setup is over: finished, or skipped. The root view keeps `onboarded`.
    Over { skipped: bool },
    /// The first project was chosen: Home works in it.
    Project(PathBuf),
}

pub(crate) struct SetupFlow {
    pub(crate) store: Entity<Store>,
    pub(crate) setup: first_run::Setup,
    /// C's page.
    pub(crate) scroll: ScrollHandle,
    _store: Subscription,
}

impl EventEmitter<FlowEvent> for SetupFlow {}

impl SetupFlow {
    pub(crate) fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        // The checklist opens on the agents: that is the finding's job.
        let mut setup = first_run::Setup::new();
        setup.open_item = Some(Item::Agents);
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
            setup,
            scroll: ScrollHandle::new(),
        }
    }

    /// The agents on this Mac, as setup lists them.
    pub(crate) fn lines(&self, cx: &gpui::App) -> Vec<AgentLine> {
        let store = self.store.read(cx);
        let rows = executors::setup_rows(
            store.state(),
            store.now_ms(),
            store.registry().listing.as_ref(),
        );
        first_run::agent_lines(&rows, store.supports_browser_sign_in(), &|id| {
            id.parse::<ExecutorId>()
                .ok()
                .and_then(|id| store.signin(&id).map(crate::store::SigninView::running))
                .unwrap_or(false)
        })
    }

    /// The finding sentence.
    pub(crate) fn finding(&self, cx: &gpui::App) -> String {
        first_run::finding(&self.lines(cx))
    }

    /// The projects Reno knows.
    pub(crate) fn projects(&self, cx: &gpui::App) -> Vec<ProjectLine> {
        self.store
            .read(cx)
            .place_desk()
            .map(|desk| {
                desk.projects()
                    .into_iter()
                    .map(|p| ProjectLine {
                        name: p.name,
                        path: p.path,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    // --- What a click does ---------------------------------------------------------------

    /// The job's control on an agent: the executor action the Executors screen has.
    pub(crate) fn act(&mut self, id: &str, action: &Action, cx: &mut Context<Self>) {
        let Ok(executor) = id.parse::<ExecutorId>() else {
            return;
        };
        let name = summary::executor_name(id);
        match action {
            Action::Install => {
                self.store.update(cx, |store, cx| {
                    store.start_executor_job(executor, JobKind::Install, false, cx);
                });
            }
            Action::SignIn(SignInRoute::Browser(method)) => {
                let method = method.clone();
                self.store.update(cx, |store, cx| {
                    store.start_signin(executor, name, method, cx);
                });
            }
            Action::SignIn(SignInRoute::Terminal(command)) => {
                if let Err(why) = crate::terminal::open(cx, &format!("Sign in to {name}"), command)
                {
                    crate::ui::toast::push(cx, crate::ui::toast::Tone::Error, "Terminal", why);
                }
            }
        }
        cx.notify();
    }

    /// A choice of the look: kept at once.
    pub(crate) fn set_look(
        &mut self,
        change: impl FnOnce(&mut prefs::Prefs),
        cx: &mut Context<Self>,
    ) {
        prefs::update(cx, change);
        self.setup.looked = true;
        cx.notify();
    }

    pub(crate) fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.set_look(|p| p.mode = mode, cx);
    }

    pub(crate) fn set_theme(&mut self, theme: ThemeName, cx: &mut Context<Self>) {
        self.set_look(|p| p.theme = theme, cx);
    }

    pub(crate) fn set_accent(&mut self, accent: AccentName, cx: &mut Context<Self>) {
        self.set_look(|p| p.accent = accent, cx);
    }

    pub(crate) fn set_notifications(&mut self, on: bool, cx: &mut Context<Self>) {
        prefs::update(cx, |p| p.notifications = on);
        self.setup.notified = true;
        cx.notify();
    }

    /// The first project: Home works in it from now on.
    pub(crate) fn choose_project(&mut self, project: &ProjectLine, cx: &mut Context<Self>) {
        self.setup.project = Some(project.name.clone());
        cx.emit(FlowEvent::Project(project.path.clone()));
        cx.notify();
    }

    pub(crate) fn go(&mut self, step: Step, cx: &mut Context<Self>) {
        self.setup.step = step;
        cx.notify();
    }

    pub(crate) fn next(&mut self, cx: &mut Context<Self>) {
        let step = self.setup.step.next();
        self.go(step, cx);
    }

    pub(crate) fn back(&mut self, cx: &mut Context<Self>) {
        let step = self.setup.step.back();
        self.go(step, cx);
    }

    pub(crate) fn toggle(&mut self, item: Item, cx: &mut Context<Self>) {
        self.setup.toggle(item);
        cx.notify();
    }

    /// B's card is put away; setup is over for good.
    pub(crate) fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.skip(cx);
    }

    /// "Skip setup": over, and never shown again. Nothing is left staged: the look and the
    /// project are already kept, and a sign-in or install already started goes on by itself.
    pub(crate) fn skip(&mut self, cx: &mut Context<Self>) {
        cx.emit(FlowEvent::Over { skipped: true });
    }

    /// "Start building".
    pub(crate) fn finish(&mut self, cx: &mut Context<Self>) {
        cx.emit(FlowEvent::Over { skipped: false });
    }

    /// `RENO_DRAFT38_FIRST_RUN`: a step of the wizard, an item of the checklist, `done`.
    pub(crate) fn hook(&mut self, word: &str, cx: &mut Context<Self>) {
        if let Some(step) = Step::parse(word) {
            self.setup.step = step;
        }
        if let Some(item) = Item::parse(word) {
            self.setup.open_item = Some(item);
        }
        if word == "done" {
            self.setup.open_item = None;
        }
        cx.notify();
    }
}
