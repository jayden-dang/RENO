//! ROAD-35 draft (throwaway): three directions for following a run -- its plan, the background
//! tasks it left running and its subagents / side chats. Behind `--fake --draft35`; without the
//! flag nothing here is installed and the app is unchanged.
//!
//! The data is the MOCKED contract `reno_client::fake::draft35::DraftActivity`, held in one
//! `Draft35` entity (a global) that a 250 ms ticker advances. The variants' views are
//! `RunScreen` methods in `screens/run/draft35.rs`; this file is the state, the switcher and the
//! keys.

use std::time::Duration;

use gpui::{
    App, AppContext as _, Context, Entity, FontWeight, Global, IntoElement, KeyBinding, Render,
    Window, actions, div, prelude::*, px,
};
use reno_client::fake::draft35::DraftActivity;

use crate::ui::icon::{Icon, icon};
use crate::ui::kit;
use crate::ui::theme;

actions!(draft35, [PrevVariant, NextVariant]);

/// How often the fake advances.
const TICK: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Variant {
    D,
    A,
    B,
}

impl Variant {
    const ALL: [Self; 3] = [Self::D, Self::A, Self::B];

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "d" => Some(Self::D),
            "a" => Some(Self::A),
            "b" => Some(Self::B),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::D => "D \u{2014} Trays + cockpit",
            Self::A => "A \u{2014} Trays",
            Self::B => "B \u{2014} Session cockpit",
        }
    }

    fn step(self, by: isize) -> Self {
        let at = Self::ALL.iter().position(|v| *v == self).unwrap_or(0) as isize;
        Self::ALL[(at + by).rem_euclid(3) as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Mode {
    Busy,
    Empty,
}

impl Mode {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "busy" => Some(Self::Busy),
            "empty" => Some(Self::Empty),
            _ => None,
        }
    }

    fn activity(self) -> DraftActivity {
        match self {
            Self::Busy => DraftActivity::busy(),
            Self::Empty => DraftActivity::empty(),
        }
    }
}

pub(crate) struct Draft35 {
    pub(crate) variant: Variant,
    pub(crate) mode: Mode,
    pub(crate) activity: DraftActivity,
    /// A: the plan tray and the background tray are open.
    pub(crate) plan_open: bool,
    pub(crate) tasks_open: bool,
    /// B: the task whose full output shows.
    pub(crate) output_of: Option<&'static str>,
}

struct Handle(Entity<Draft35>);

impl Global for Handle {}

impl Draft35 {
    /// The draft's state while `--draft35` is on.
    pub(crate) fn get(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<Handle>().map(|h| h.0.clone())
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.mode = mode;
        self.activity = mode.activity();
        self.output_of = None;
        cx.notify();
    }

    fn set_variant(&mut self, variant: Variant, cx: &mut Context<Self>) {
        self.variant = variant;
        cx.notify();
    }
}

/// Turns the draft on: the state, the ticker, the keys and the switcher.
pub(crate) fn install(variant: Variant, mode: Mode, open: Option<&str>, cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-alt-left", PrevVariant, None),
        KeyBinding::new("cmd-alt-right", NextVariant, None),
    ]);
    let draft = cx.new(|_| Draft35 {
        variant,
        mode,
        activity: mode.activity(),
        plan_open: false,
        tasks_open: false,
        output_of: None,
    });
    for part in open.unwrap_or_default().split(',') {
        draft.update(cx, |d, _| match part {
            "plan" => d.plan_open = true,
            "tasks" => d.tasks_open = true,
            "stop:dev" => d.activity.stop_task("dev"),
            _ => {}
        });
    }
    let ticking = draft.downgrade();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(TICK).await;
            let alive = ticking.update(cx, |d, cx| {
                d.activity.tick(TICK.as_millis() as u64);
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        }
    })
    .detach();
    cx.set_global(Handle(draft.clone()));
    let switcher = cx.new(|cx| {
        cx.observe(&draft, |_, _, cx| cx.notify()).detach();
        Switcher { draft }
    });
    cx.set_global(SwitcherHandle(switcher));
}

struct SwitcherHandle(Entity<Switcher>);

impl Global for SwitcherHandle {}

/// The floating switcher, for the root view to put on top of the window.
pub(crate) fn switcher(cx: &App) -> Option<Entity<Switcher>> {
    cx.try_global::<SwitcherHandle>().map(|h| h.0.clone())
}

/// ⌘⌥← / ⌘⌥→: the previous or next variant, wrapping.
pub(crate) fn cycle(by: isize, cx: &mut App) {
    if let Some(draft) = Draft35::get(cx) {
        draft.update(cx, |d, cx| d.set_variant(d.variant.step(by), cx));
    }
}

/// The bar's count chip for variant B ("2 running \u{b7} 1 agent"), when there is something.
pub(crate) fn bar_chip(cx: &App) -> Option<String> {
    let draft = Draft35::get(cx)?;
    let d = draft.read(cx);
    if d.variant != Variant::B || d.activity.is_empty() {
        return None;
    }
    let tasks = d.activity.running_tasks();
    let agents_total = d.activity.running_agents();
    Some(format!(
        "{tasks} running \u{b7} {agents_total} agent{}",
        if agents_total == 1 { "" } else { "s" }
    ))
}

pub(crate) struct Switcher {
    draft: Entity<Draft35>,
}

impl Render for Switcher {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let (variant, mode) = {
            let d = self.draft.read(cx);
            (d.variant, d.mode)
        };
        let arrow = |id: &'static str, glyph: &'static str, by: isize| {
            let draft = self.draft.clone();
            div()
                .id(id)
                .debug_selector(move || id.to_owned())
                .size(px(22.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .cursor_pointer()
                .text_color(theme.muted)
                .hover(|s| s.bg(theme.raised).text_color(theme.text))
                .child(glyph)
                .on_click(move |_, _, cx| {
                    draft.update(cx, |d, cx| d.set_variant(d.variant.step(by), cx));
                })
        };
        let pick = |id: &'static str, label: &'static str, this: Mode| {
            let draft = self.draft.clone();
            kit::segment(&theme, id, label, mode == this).on_click(move |_, _, cx| {
                draft.update(cx, |d, cx| d.set_mode(this, cx));
            })
        };
        div()
            .id("draft35-switcher")
            .debug_selector(|| "draft35-switcher".to_owned())
            .absolute()
            .bottom(px(64.0))
            .left(px(12.0))
            .flex()
            .child(
                div()
                    .occlude()
                    .w(px(224.0))
                    .p(px(6.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .rounded(px(8.0))
                    .bg(theme.card)
                    .border_1()
                    .border_color(theme.accent.opacity(0.5))
                    .shadow_md()
                    .text_size(px(12.5))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .child(arrow("draft35-prev", "\u{25c0}", -1))
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .justify_center()
                                    .whitespace_nowrap()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(variant.name()),
                            )
                            .child(arrow("draft35-next", "\u{25b6}", 1)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(2.0))
                            .child(icon(Icon::Wand, 13.0, theme.accent))
                            .child(pick("draft35-busy", "Busy run", Mode::Busy))
                            .child(pick("draft35-empty", "Empty run", Mode::Empty)),
                    ),
            )
    }
}
