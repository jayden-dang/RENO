//! Settings › General, Appearance and Accounts (brief:
//! docs/design/2026-11-19-settings-pages-ui-brief.md). Every choice is saved through
//! `prefs::update` (the composer's agent list through the store's composer prefs) and applies at
//! once.

use gpui::{
    AnyElement, Context, FontWeight, Hsla, IntoElement, ParentElement, Role, SharedString, Styled,
    Toggled, div, prelude::*, px, rgb,
};
use reno_proto::InstallStatus;

use super::SettingsScreen;
use crate::prefs::{self, AccentName, Mode, Motion, Prefs, SendWith, ThemeName, ThinkingMode};
use crate::summary;
use crate::ui::kit;
use crate::ui::motion::{self, ReduceMotion};
use crate::ui::page;
use crate::ui::theme::{Theme, palette_for};
use crate::vm::accounts::{self, Row};

/// The motion preference a saved choice stands for.
pub(crate) fn reduce_motion(choice: Motion) -> ReduceMotion {
    match choice {
        Motion::System => ReduceMotion::System,
        Motion::On => ReduceMotion::On,
        Motion::Off => ReduceMotion::Off,
    }
}

/// A pinned Reduce motion (Settings › Appearance) holds from the first frame; System leaves the
/// launch preference (`RENO_REDUCE_MOTION`, or the OS) alone.
pub(crate) fn apply_saved_motion(cx: &mut gpui::App) {
    let saved = prefs::current(cx).reduce_motion;
    if saved != Motion::System {
        motion::set_preference(reduce_motion(saved), cx);
    }
}

fn help(theme: &Theme, text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(12.5))
        .line_height(px(18.0))
        .text_color(theme.muted)
        .child(text.into())
}

/// A card row: the label (and its help) left, the control right.
fn row(
    theme: &Theme,
    label: &'static str,
    help_text: Option<SharedString>,
    control: impl IntoElement,
    last: bool,
) -> AnyElement {
    page::table_row(theme, last)
        .min_h(px(if help_text.is_some() { 56.0 } else { 44.0 }))
        .gap(px(14.0))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(div().font_weight(FontWeight::MEDIUM).child(label))
                .children(help_text.map(|h| help(theme, h))),
        )
        .child(div().flex_none().child(control))
        .into_any_element()
}

fn card(theme: &Theme, rows: Vec<AnyElement>) -> AnyElement {
    page::table_frame(theme, page::table())
        .children(rows)
        .into_any_element()
}

fn section(theme: &Theme, title: &'static str, body: AnyElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(page::section_head(theme, title, None, None).pb(px(0.0)))
        .child(body)
        .into_any_element()
}

/// A segmented choice whose segments save `set(value)`.
fn choice<T: Copy + PartialEq + 'static>(
    theme: &Theme,
    stem: &'static str,
    options: &[(T, &'static str, &'static str)],
    current: T,
    set: fn(&mut Prefs, T),
    cx: &mut Context<SettingsScreen>,
) -> AnyElement {
    kit::segmented(theme)
        .children(options.iter().map(|&(value, key, label)| {
            kit::segment(theme, format!("{stem}-{key}"), label, value == current)
                .on_click(cx.listener(move |_, _, _, cx| prefs::update(cx, |p| set(p, value))))
        }))
        .into_any_element()
}

/// The math sizes, 90 to 150 % in steps of 10.
fn math_size_choice(theme: &Theme, current: u8, cx: &mut Context<SettingsScreen>) -> AnyElement {
    kit::segmented(theme)
        .children(prefs::MATH_SIZES.map(|size| {
            kit::segment(
                theme,
                format!("math-size-{size}"),
                format!("{size} %"),
                size == current,
            )
            .on_click(cx.listener(move |_, _, _, cx| prefs::update(cx, |p| p.math_size = size)))
        }))
        .into_any_element()
}

fn switch(
    theme: &Theme,
    id: &'static str,
    on: bool,
    flip: fn(&mut Prefs),
    cx: &mut Context<SettingsScreen>,
) -> AnyElement {
    kit::switch(theme, id, on)
        .on_click(cx.listener(move |_, _, _, cx| prefs::update(cx, flip)))
        .into_any_element()
}

fn hex(c: u32) -> Hsla {
    rgb(c).into()
}

impl SettingsScreen {
    // --- Appearance ----------------------------------------------------------------------

    pub(super) fn appearance_page(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = prefs::current(cx);
        let mode = choice(
            theme,
            "mode",
            &[
                (Mode::System, "system", "System"),
                (Mode::Light, "light", "Light"),
                (Mode::Dark, "dark", "Dark"),
            ],
            p.mode,
            |p, v| p.mode = v,
            cx,
        );
        let motion_choice = kit::segmented(theme)
            .children(
                [
                    (Motion::System, "system", "System"),
                    (Motion::On, "on", "On"),
                    (Motion::Off, "off", "Off"),
                ]
                .map(|(value, key, label)| {
                    kit::segment(
                        theme,
                        format!("reduce-motion-{key}"),
                        label,
                        value == p.reduce_motion,
                    )
                    .on_click(cx.listener(move |_, _, _, cx| {
                        prefs::update(cx, |p| p.reduce_motion = value);
                        motion::set_preference(reduce_motion(value), cx);
                    }))
                }),
            )
            .into_any_element();
        vec![
            page::title(theme, "Appearance", "How Reno looks on this Mac.", false)
                .into_any_element(),
            card(
                theme,
                vec![
                    row(
                        theme,
                        "Mode",
                        Some("System follows macOS, day and night.".into()),
                        mode,
                        false,
                    ),
                    row(theme, "Theme", None, self.themes(theme, &p, cx), false),
                    row(theme, "Accent", None, self.accents(theme, &p, cx), false),
                    row(theme, "Reduce motion", None, motion_choice, false),
                    row(
                        theme,
                        "Typeset math",
                        Some("Off shows formulas as their TeX source.".into()),
                        switch(
                            theme,
                            "typeset-math",
                            p.typeset_math,
                            |p| p.typeset_math = !p.typeset_math,
                            cx,
                        ),
                        !p.typeset_math,
                    ),
                ]
                .into_iter()
                .chain((p.typeset_math).then(|| {
                    row(
                        theme,
                        "Math size",
                        Some("As a share of the text size.".into()),
                        math_size_choice(theme, p.math_size, cx),
                        true,
                    )
                }))
                .collect(),
            ),
        ]
    }

    fn themes(&self, theme: &Theme, p: &Prefs, cx: &mut Context<Self>) -> AnyElement {
        let appearance = theme.appearance;
        kit::segmented(theme)
            .children(
                [
                    (ThemeName::Vercel, "vercel", "Vercel"),
                    (ThemeName::Reno, "reno", "Reno"),
                    (ThemeName::Slate, "slate", "Slate"),
                ]
                .map(|(name, key, label)| {
                    let palette = palette_for(name, appearance, p.accent);
                    kit::segment(theme, format!("theme-{key}"), label, name == p.theme)
                        .gap(px(6.0))
                        .child(
                            // The theme in a split circle: its canvas and its muted ink.
                            div()
                                .size(px(14.0))
                                .flex()
                                .rounded_full()
                                .overflow_hidden()
                                .border_1()
                                .border_color(hex(palette.text).opacity(0.25))
                                .child(div().w(px(7.0)).h_full().bg(hex(palette.canvas)))
                                .child(div().flex_1().h_full().bg(hex(palette.muted))),
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            prefs::update(cx, |p| p.theme = name);
                        }))
                }),
            )
            .into_any_element()
    }

    fn accents(&self, theme: &Theme, p: &Prefs, cx: &mut Context<Self>) -> AnyElement {
        let appearance = theme.appearance;
        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .children(
                [
                    (AccentName::Blue, "blue", "Blue accent"),
                    (AccentName::Teal, "teal", "Teal accent"),
                    (AccentName::Violet, "violet", "Violet accent"),
                ]
                .map(|(name, key, label)| {
                    let fill = hex(palette_for(p.theme, appearance, name).accent);
                    let on = name == p.accent;
                    let id = format!("accent-{key}");
                    div()
                        .id(SharedString::from(id.clone()))
                        .debug_selector(move || id.clone())
                        .role(Role::Button)
                        .aria_label(label)
                        .aria_toggled(if on { Toggled::True } else { Toggled::False })
                        .focusable()
                        .tab_stop(true)
                        .focus_visible(kit::focus_ring(theme))
                        .size(px(22.0))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .border_2()
                        .border_color(if on { fill } else { gpui::transparent_black() })
                        .cursor_pointer()
                        .on_click(cx.listener(move |_, _, _, cx| {
                            prefs::update(cx, |p| p.accent = name);
                        }))
                        .child(div().size(px(16.0)).rounded_full().bg(fill))
                }),
            )
            .into_any_element()
    }

    // --- General -------------------------------------------------------------------------

    pub(super) fn general_page(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = prefs::current(cx);
        let send = choice(
            theme,
            "send",
            &[
                (SendWith::Enter, "enter", "Enter"),
                (SendWith::CommandEnter, "cmdenter", "\u{2318} Enter"),
            ],
            p.send_with,
            |p, v| p.send_with = v,
            cx,
        );
        let send_help = match p.send_with {
            SendWith::Enter => "\u{21e7} Enter starts a new line.",
            SendWith::CommandEnter => "Enter starts a new line; \u{2318} Enter sends.",
        };
        let thinking = choice(
            theme,
            "thinking",
            &[
                (ThinkingMode::Auto, "auto", "Auto"),
                (ThinkingMode::Preview, "preview", "Preview"),
                (ThinkingMode::Open, "open", "Always open"),
                (ThinkingMode::Closed, "closed", "Always closed"),
            ],
            p.thinking,
            |p, v| p.thinking = v,
            cx,
        );
        let thinking_help = match p.thinking {
            ThinkingMode::Auto => "Open while the agent reasons, closed once it has.",
            ThinkingMode::Preview => {
                "A window of up to 16 lines while the agent reasons, closed once it has."
            }
            ThinkingMode::Open => "Reasoning stays open.",
            ThinkingMode::Closed => "Reasoning stays closed until you open it.",
        };
        vec![
            page::title(
                theme,
                "General",
                "How the composer, the Files tab and the agent list behave.",
                false,
            )
            .into_any_element(),
            section(
                theme,
                "Conversation",
                card(
                    theme,
                    vec![
                        row(
                            theme,
                            "Send messages with",
                            Some(send_help.into()),
                            send,
                            false,
                        ),
                        row(
                            theme,
                            "Escape stops the agent",
                            None,
                            switch(
                                theme,
                                "escape-stops",
                                p.escape_stops,
                                |p| {
                                    p.escape_stops = !p.escape_stops;
                                },
                                cx,
                            ),
                            false,
                        ),
                        row(
                            theme,
                            "Thinking",
                            Some(thinking_help.into()),
                            thinking,
                            false,
                        ),
                        row(
                            theme,
                            "Expand new edits",
                            None,
                            switch(
                                theme,
                                "expand-new-edits",
                                p.expand_new_edits,
                                |p| p.expand_new_edits = !p.expand_new_edits,
                                cx,
                            ),
                            false,
                        ),
                        row(
                            theme,
                            "Expand new commands",
                            None,
                            switch(
                                theme,
                                "expand-new-commands",
                                p.expand_new_commands,
                                |p| p.expand_new_commands = !p.expand_new_commands,
                                cx,
                            ),
                            true,
                        ),
                    ],
                ),
            ),
            section(
                theme,
                "Files tab",
                card(
                    theme,
                    vec![row(
                        theme,
                        "Show hidden files",
                        None,
                        switch(
                            theme,
                            "hidden-files",
                            p.hidden_files,
                            |p| {
                                p.hidden_files = !p.hidden_files;
                            },
                            cx,
                        ),
                        true,
                    )],
                ),
            ),
            section(
                theme,
                "In the composer",
                card(theme, self.offered_rows(theme, cx)),
            ),
        ]
    }

    /// One row per installed built-in agent (not the test executor), with its switch.
    fn offered_rows(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let store = self.store.read(cx);
        let prefs = store.composer_prefs();
        let agents: Vec<(String, bool)> = store
            .state()
            .executors()
            .iter()
            .filter(|e| e.source.is_builtin() && e.descriptor.id.as_str() != "mock")
            .filter(|e| {
                e.health
                    .as_ref()
                    .is_some_and(|h| matches!(h.installed, InstallStatus::Installed { .. }))
            })
            .map(|e| {
                let id = e.descriptor.id.to_string();
                let on = prefs.offered(&id);
                (id, on)
            })
            .collect();
        let n = agents.len();
        agents
            .into_iter()
            .enumerate()
            .map(|(i, (id, on))| {
                let store = self.store.clone();
                let key = id.clone();
                page::table_row(theme, i + 1 == n)
                    .min_h(px(44.0))
                    .gap(px(12.0))
                    .child(kit::logo(theme, &id, 18.0))
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::MEDIUM)
                            .child(summary::executor_name(&id)),
                    )
                    .child(kit::switch(theme, format!("offer-{id}"), on).on_click(
                        move |_, _, cx| {
                            store.update(cx, |s, cx| {
                                s.update_composer_prefs(|p| p.set_offered(&key, !on), cx);
                            });
                        },
                    ))
                    .into_any_element()
            })
            .collect()
    }

    // --- Accounts ------------------------------------------------------------------------

    pub(super) fn accounts_page(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let store = self.store.read(cx);
        let view = accounts::view(store.state().executors(), store.now_ms());
        let n = view.rows.len();
        let rows = view
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| account_row(theme, r, i + 1 == n))
            .collect();
        vec![
            div()
                .id("accounts-finding")
                .debug_selector(|| "accounts-finding".to_owned())
                .child(page::title(
                    theme,
                    view.title.clone(),
                    view.detail.clone(),
                    view.warn,
                ))
                .into_any_element(),
            card(theme, rows),
        ]
    }
}

fn account_row(theme: &Theme, r: &Row, last: bool) -> AnyElement {
    let id = format!("account-{}", r.executor);
    let meter = r.meter.as_ref().map(|m| {
        let tone = if m.near_limit() {
            theme.warning
        } else {
            theme.accent
        };
        let meter_id = format!("account-meter-{}", r.executor);
        let words = m.words();
        div()
            .id(SharedString::from(meter_id.clone()))
            .debug_selector(move || meter_id.clone())
            .aria_label(SharedString::from(format!(
                "{}, {}% used{}",
                m.label,
                (m.used * 100.0).round(),
                m.resets
                    .as_ref()
                    .map_or_else(String::new, |r| format!(", {r}"))
            )))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .w(px(80.0))
                    .h(px(6.0))
                    .rounded(px(3.0))
                    .bg(theme.raised)
                    .child(
                        div()
                            .h_full()
                            .rounded(px(3.0))
                            .bg(tone)
                            .w(gpui::relative(m.used.clamp(0.02, 1.0))),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(theme.muted)
                    .whitespace_nowrap()
                    .child(words),
            )
    });
    page::table_row(theme, last)
        .id(SharedString::from(id.clone()))
        .debug_selector(move || id.clone())
        .min_h(px(52.0))
        .gap(px(12.0))
        .child(kit::logo(theme, &r.executor, 18.0))
        .child(
            div()
                .w(px(190.0))
                .flex_none()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().font_weight(FontWeight::MEDIUM).child(r.name.clone()))
                .child(
                    div()
                        .truncate()
                        .text_size(px(12.5))
                        .text_color(theme.muted)
                        .child(r.who.clone()),
                ),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .children(r.plan.clone().map(|p| kit::tag(theme.muted, p))),
        )
        .children(meter)
        .into_any_element()
}
