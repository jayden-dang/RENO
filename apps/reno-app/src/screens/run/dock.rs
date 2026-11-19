//! The dock under the transcript, in the chat's column: the steer box, or, while the run holds
//! an approval, the approval panel in its place.

use gpui::{
    AnyElement, Context, FontWeight, IntoElement, KeyBinding, Window, actions, div, prelude::*, px,
};
use reno_proto::Approval;

use super::{COLUMN, RunScreen};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO};
use crate::ui::theme;
use crate::vm::approval_panel::{self, Kind};
use crate::vm::focus_guard::{self, Facts, Target};
use crate::vm::rules;

#[cfg(test)]
thread_local! {
    static COMPOSER_STAYS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Tests of the composer itself use runs that hold an approval: this keeps their box on screen
/// under the panel, as if the owner had been typing. The approval tests do not call it.
#[cfg(test)]
pub(crate) fn keep_composer_under_panels() {
    COMPOSER_STAYS.with(|c| c.set(true));
}

actions!(run_approval, [DenyHeld, AllowHeld]);

/// The key context of the approval panel; its keys work only while it has the keyboard.
const CONTEXT: &str = "ApprovalPanel";

pub(crate) fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        KeyBinding::new("escape", DenyHeld, Some(CONTEXT)),
        KeyBinding::new("enter", AllowHeld, Some(CONTEXT)),
    ]);
}

impl RunScreen {
    pub(super) fn dock(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let held: Vec<Approval> = self
            .store
            .read(cx)
            .state()
            .approvals_for(&self.run)
            .cloned()
            .collect();
        let Some(first) = held.first() else {
            self.panel_for = None;
            self.keep_composer = false;
            self.dock_drawn = true;
            return self
                .dock_frame()
                .child(self.steer.clone())
                .into_any_element();
        };
        if self.panel_for.is_none() {
            // The panel never steals the keyboard: if the owner is typing, their box stays.
            let steer = self.steer.read(cx);
            // The box a screen opens with is focused by default: that is not typing.
            let focused = self.dock_drawn && steer.focus_handle_ref().is_focused(window);
            self.keep_composer = focus_guard::composer_stays(focused, steer.has_text());
            #[cfg(test)]
            {
                self.keep_composer |= COMPOSER_STAYS.with(std::cell::Cell::get);
            }
        }
        self.dock_drawn = true;
        // A draft is never hidden under the panel.
        self.keep_composer |= self.steer.read(cx).has_text();
        self.panel_for = Some(first.id.clone());
        let panel = self.approval_panel(first, held.len(), window, cx);
        let frame = self.dock_frame().gap(px(8.0)).child(panel);
        if self.keep_composer {
            frame.child(self.steer.clone()).into_any_element()
        } else {
            frame.into_any_element()
        }
    }

    fn dock_frame(&self) -> gpui::Div {
        div()
            .flex_none()
            .w_full()
            .max_w(px(COLUMN))
            .pb(px(20.0))
            .pt(px(6.0))
            .flex()
            .flex_col()
    }

    /// ⌘J: gives the panel the keyboard, when a panel is waiting.
    pub(crate) fn jump_to_approval(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panel_for.is_some() {
            window.focus(&self.approval_focus, cx);
            cx.notify();
        }
    }

    /// `RENO_DEMO_MENU`: opens a menu for a screenshot.
    pub(crate) fn open_demo_menu(&mut self, menu: &str, cx: &mut Context<Self>) {
        if menu == "approval-more" {
            self.menu_open = self.panel_for.clone();
            cx.notify();
        } else {
            let menu = menu.to_owned();
            self.steer.update(cx, |c, cx| c.open_demo_menu(&menu, cx));
        }
    }

    /// Who gets Esc and Enter now.
    fn key_target(&self, window: &Window) -> Target {
        focus_guard::key_target(Facts {
            panel_pending: self.panel_for.is_some(),
            panel_claimed: self.approval_focus.is_focused(window),
        })
    }

    fn approval_panel(
        &mut self,
        approval: &Approval,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme::current(cx);
        let view = approval_panel::view(&approval.subject);
        let id = approval.id.clone();
        let (busy, similar, choices, hint) = {
            let store = self.store.read(cx);
            let state = store.state();
            let summary = state.run(&approval.run_id);
            let bot = summary.and_then(|r| crate::summary::run_bot(r));
            let project = state
                .supports(reno_proto::FEATURE_PROJECT_RULES)
                .then(|| summary.and_then(|r| r.cwd.as_deref()))
                .flatten()
                .and_then(|cwd| store.root_of(cwd))
                .and_then(|root| root.file_name().map(|n| n.to_string_lossy().into_owned()));
            // "Allow similar commands" is not built on the real daemon (ROAD-5): the menu shows
            // on the fake, and only where the daemon keeps standing rules.
            let similar =
                store.capabilities().allow_similar && state.supports(reno_proto::FEATURE_RULES);
            let hint = rules::hint(
                &approval.subject,
                if bot.is_some() {
                    reno_proto::RememberScope::Bot
                } else {
                    reno_proto::RememberScope::Run
                },
                bot,
            );
            (
                store.is_deciding(&approval.id),
                similar,
                rules::approve_choices(bot, project.as_deref()),
                hint,
            )
        };
        let menu_open = similar && self.menu_open.as_ref() == Some(&id);
        let allow = {
            let id = id.clone();
            cx.listener(move |this, _, _, cx| this.approve(id.clone(), None, cx))
        };
        let deny = {
            let id = id.clone();
            cx.listener(move |this, _, _, cx| this.deny(id.clone(), None, cx))
        };
        let on_fill = theme.on_fill(theme.accent);

        let deny_button = kit::pill_button(&theme, "deny", "Deny", kit::Tone::Quiet)
            .child(kit::keycap(&theme, "Esc", None))
            .when(busy, |b| b.opacity(0.4))
            .when(!busy, |b| b.on_click(deny));
        let allow_button = kit::pill_button(&theme, "approve", "Allow once", kit::Tone::Primary)
            .child(kit::keycap(&theme, "\u{23ce}", Some(on_fill)))
            .when(busy, |b| b.opacity(0.4))
            .when(!busy, |b| b.on_click(allow));
        let more_button = similar.then(|| {
            let toggle = {
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
            kit::icon_button(
                &theme,
                "approve-more",
                if menu_open {
                    Icon::ChevUp
                } else {
                    Icon::ChevDown
                },
                30.0,
            )
            .when(!busy, |b| b.on_click(toggle))
        });
        let menu =
            menu_open.then(|| {
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
                            .child(format!("Allow similar commands \u{b7} {hint}")),
                    )
                    .children(choices.into_iter().enumerate().skip(1).map(
                        |(ix, (scope, label))| {
                            let id = id.clone();
                            kit::pill_button(
                                &theme,
                                format!("approve-scope-{ix}"),
                                label,
                                kit::Tone::Quiet,
                            )
                            .when(!busy, |b| {
                                b.on_click(cx.listener(move |this, _, _, cx| {
                                    this.approve(id.clone(), scope, cx);
                                }))
                            })
                        },
                    ))
            });

        div()
            .id("approval-panel")
            .debug_selector(|| "approval-panel".to_owned())
            .key_context(CONTEXT)
            .track_focus(&self.approval_focus)
            // A click on the panel is the owner's choice to answer it by keyboard.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.approval_focus, cx)),
            )
            .on_action(cx.listener(|this, _: &DenyHeld, window, cx| {
                if this.key_target(window) == Target::Panel
                    && let Some(id) = this.panel_for.clone()
                {
                    this.deny(id, None, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &AllowHeld, window, cx| {
                if this.key_target(window) == Target::Panel
                    && let Some(id) = this.panel_for.clone()
                {
                    this.approve(id, None, cx);
                }
            }))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(18.0))
            .rounded(px(26.0))
            .bg(theme.surface)
            .border_1()
            .border_color(if self.approval_focus.is_focused(window) {
                theme.text.opacity(0.2)
            } else {
                theme.hairline
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(13.5))
                    .text_color(theme.muted)
                    .child(icon(kind_icon(view.kind), 15.0, theme.muted))
                    .child(
                        div()
                            .id("approval-kind")
                            .debug_selector(|| "approval-kind".to_owned())
                            .child(view.kind.label()),
                    )
                    .child(div().flex_1())
                    .when(count > 1, |d| {
                        d.child(div().text_color(theme.faint).child(format!("1 of {count}")))
                    }),
            )
            .child(
                div()
                    .text_size(px(15.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(view.sentence),
            )
            .child(
                div()
                    .id("approval-detail")
                    .debug_selector(|| "approval-detail".to_owned())
                    .w_full()
                    .max_h(px(140.0))
                    .overflow_hidden()
                    .px(px(14.0))
                    .py(px(10.0))
                    .rounded(px(12.0))
                    .bg(theme.shell)
                    .font_family(MONO)
                    .text_size(px(12.5))
                    .text_color(theme.muted)
                    .child(view.detail),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .child(deny_button)
                    .child(allow_button)
                    .children(more_button),
            )
            .children(menu)
            .into_any_element()
    }
}

fn kind_icon(kind: Kind) -> Icon {
    match kind {
        Kind::Terminal => Icon::Terminal,
        Kind::Network => Icon::Globe,
        Kind::Edit => Icon::Pencil,
        Kind::Read => Icon::Eye,
        Kind::Tool => Icon::Wand,
        Kind::Other => Icon::Shield,
    }
}
