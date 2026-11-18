//! The notifier's wiring: the store, the windows and the clock feed `notify::Notifier`; what it
//! decides goes to the [`Desktop`]. One entity for the whole app, installed at app level so it
//! outlives any window (the Dock badge and the banners keep coming with the window closed).

use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Entity, Subscription, Task, WeakEntity, WindowId};
use reno_notify::Delivery;
use reno_proto::RunId;

use crate::app::RootView;
use crate::desktop::Desktop;
use crate::notify::{Effect, Notifier, Snapshot};
use crate::prefs::{Pref, PrefStore, Prefs};
use crate::store::Store;

/// The shortest wait before a pending banner is looked at again.
const MIN_WAKE: Duration = Duration::from_millis(1);

pub(crate) struct Banners {
    store: Entity<Store>,
    desktop: Rc<dyn Desktop>,
    prefs: PrefStore,
    notifier: Notifier,
    /// The Reno windows that are the active window (at most one in practice).
    active: HashSet<WindowId>,
    /// The badge as last set on the desktop.
    badge: Option<String>,
    /// The run on screen, to withdraw its banners when it changes.
    looking: Option<RunId>,
    /// Wakes the notifier when a pending banner stands long enough.
    wake: Option<Task<()>>,
    _store: Subscription,
}

impl Banners {
    /// The notifier for `store`, installed for the whole app: it watches every Reno window's
    /// activation (so "only when in the background" knows), and starts with a first pass.
    pub(crate) fn install(
        cx: &mut App,
        store: Entity<Store>,
        desktop: Rc<dyn Desktop>,
        prefs: PrefStore,
    ) -> Entity<Self> {
        let now = cx.background_executor().now();
        let banners = cx.new(|cx| Self {
            _store: cx.observe(&store, |this: &mut Self, _, cx| this.on_store(cx)),
            store,
            desktop,
            prefs,
            notifier: Notifier::new(now),
            active: HashSet::new(),
            badge: None,
            looking: None,
            wake: None,
        });
        let weak = banners.downgrade();
        cx.observe_new(move |_: &mut RootView, window, cx| {
            let Some(window) = window else {
                return;
            };
            let id = window.window_handle().window_id();
            Self::track(&weak, id, window.is_window_active(), cx);
            let weak = weak.clone();
            cx.observe_window_activation(window, move |_, window, cx| {
                Self::track(&weak, id, window.is_window_active(), cx);
            })
            .detach();
        })
        .detach();
        let weak = banners.downgrade();
        cx.on_window_closed(move |cx, id| {
            Self::track(&weak, id, false, cx);
            // No window shows a run any more: opening it again later is news to withdraw for.
            if cx.windows().is_empty() {
                weak.update(cx, |this, cx| {
                    this.store.update(cx, |store, cx| store.look_at(None, cx));
                })
                .ok();
            }
        })
        .detach();
        banners.update(cx, |this, cx| this.pass(cx));
        banners
    }

    fn track(
        weak: &WeakEntity<Self>,
        window: WindowId,
        active: bool,
        cx: &mut impl gpui::AppContext,
    ) {
        weak.update(cx, |this, cx| this.set_active(window, active, cx))
            .ok();
    }

    pub(crate) fn prefs(&self) -> &Prefs {
        self.prefs.prefs()
    }

    pub(crate) fn delivery(&self) -> Delivery {
        self.desktop.delivery()
    }

    pub(crate) fn set_pref(&mut self, pref: Pref, on: bool, cx: &mut Context<Self>) {
        self.prefs.set(pref, on);
        self.pass(cx);
        cx.notify();
    }

    fn set_active(&mut self, window: WindowId, active: bool, cx: &mut Context<Self>) {
        let changed = if active {
            self.active.insert(window)
        } else {
            self.active.remove(&window)
        };
        if changed {
            self.pass(cx);
        }
    }

    /// The person opened `run` (by a banner, or in the window): its banners go.
    pub(crate) fn run_opened(&mut self, run: &RunId) {
        let effects = self.notifier.run_opened(run);
        self.apply(effects);
    }

    fn on_store(&mut self, cx: &mut Context<Self>) {
        let looking = self.store.read(cx).looking_at().cloned();
        if looking != self.looking {
            self.looking.clone_from(&looking);
            if let Some(run) = looking {
                self.run_opened(&run);
            }
        }
        self.pass(cx);
    }

    /// One pass of the notifier, then a wake-up for what is still pending.
    fn pass(&mut self, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let snapshot = Snapshot::from_state(self.store.read(cx).state());
        let outcome =
            self.notifier
                .step(&snapshot, now, !self.active.is_empty(), self.prefs.prefs());
        self.apply(outcome.effects);
        if outcome.badge != self.badge {
            self.desktop.set_badge(outcome.badge.as_deref());
            self.badge = outcome.badge;
        }
        self.wake = self.notifier.next_due().map(|due| {
            let wait = due.saturating_duration_since(now).max(MIN_WAKE);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                this.update(cx, |this, cx| this.pass(cx)).ok();
            })
        });
    }

    fn apply(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Post(banner) => self.desktop.post(&banner),
                Effect::Withdraw(id) => self.desktop.withdraw(&id),
            }
        }
    }
}
