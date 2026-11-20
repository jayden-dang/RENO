//! What the wizard's steps draw: the job's control on an agent, an agent's state mark, the look
//! controls and the project rows. Each is a method of [`SetupFlow`] so its clicks reach the
//! flow.

use gpui::{
    AnyElement, Context, FontWeight, Hsla, IntoElement, Role, SharedString, Toggled, div,
    prelude::*, px, rgb,
};

use super::flow::SetupFlow;
use crate::prefs::{self, AccentName, Mode, ThemeName};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, Tone};
use crate::ui::theme::{Theme, palette_for};
use crate::vm::first_run::{Action, AgentLine, AgentState, ProjectLine};

fn hex(c: u32) -> Hsla {
    rgb(c).into()
}

/// The colour an agent's state is drawn in.
pub(crate) fn state_tone(theme: &Theme, state: &AgentState) -> Hsla {
    match state {
        AgentState::Ready => theme.success,
        AgentState::NeedsSignIn | AgentState::NeedsUpdate | AgentState::Unknown => theme.warning,
        AgentState::NotInstalled | AgentState::Unusable | AgentState::Failed(_) => theme.danger,
        AgentState::Signing | AgentState::Installing => theme.accent,
    }
}

impl SetupFlow {
    /// The job's control on an agent: Sign in or Install, or a spinner while it runs. `None`
    /// when the agent has nothing to do (ready, or no way to fix it from here).
    pub(crate) fn control(
        &self,
        theme: &Theme,
        line: &AgentLine,
        primary: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if line.state.working() {
            return Some(
                div()
                    .flex_none()
                    .h(px(26.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(12.5))
                    .text_color(theme.muted)
                    .child(kit::spinner(
                        SharedString::from(format!("setup-spin-{}", line.id)),
                        13.0,
                        theme.accent,
                    ))
                    .child(line.state.words())
                    .into_any_element(),
            );
        }
        let action = line.action.clone()?;
        let (verb, glyph) = match action {
            Action::SignIn(_) => ("sign-in", None),
            Action::Install { .. } => ("install", Some(Icon::PhDownload)),
        };
        let id = line.id.clone();
        let label = if matches!(line.state, AgentState::Failed(_)) {
            "Try again"
        } else {
            action.label()
        };
        Some(
            kit::line_button(
                theme,
                format!("setup-{verb}-{}", line.id),
                glyph,
                label,
                if primary { Tone::Primary } else { Tone::Quiet },
                true,
            )
            .on_click(cx.listener(move |this, _, _, cx| this.act(&id, &action, cx)))
            .into_any_element(),
        )
    }

    /// A small mark of an agent's state: a check when ready, else a dot in the state's colour.
    pub(crate) fn state_mark(theme: &Theme, state: &AgentState) -> AnyElement {
        match state {
            AgentState::Ready => icon(Icon::PhCheck, 14.0, theme.success).into_any_element(),
            other => kit::dot(state_tone(theme, other), 7.0).into_any_element(),
        }
    }

    /// Mode, theme and accent, each kept as it is chosen.
    pub(crate) fn look_controls(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let p = prefs::current(cx);
        let appearance = theme.appearance;
        let mode = kit::segmented(theme).children(
            [
                (Mode::System, "system", "System"),
                (Mode::Light, "light", "Light"),
                (Mode::Dark, "dark", "Dark"),
            ]
            .map(|(value, key, label)| {
                kit::segment(theme, format!("setup-mode-{key}"), label, value == p.mode)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_mode(value, cx)))
            }),
        );
        let themes = kit::segmented(theme).children(
            [
                (ThemeName::Vercel, "vercel", "Vercel"),
                (ThemeName::Reno, "reno", "Reno"),
                (ThemeName::Slate, "slate", "Slate"),
            ]
            .map(|(name, key, label)| {
                let palette = palette_for(name, appearance, p.accent);
                kit::segment(theme, format!("setup-theme-{key}"), label, name == p.theme)
                    .gap(px(6.0))
                    .child(
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
                    .on_click(cx.listener(move |this, _, _, cx| this.set_theme(name, cx)))
            }),
        );
        let accents = div().flex().items_center().gap(px(8.0)).children(
            [
                (AccentName::Blue, "blue", "Blue accent"),
                (AccentName::Teal, "teal", "Teal accent"),
                (AccentName::Violet, "violet", "Violet accent"),
            ]
            .map(|(name, key, label)| {
                let fill = hex(palette_for(p.theme, appearance, name).accent);
                let on = name == p.accent;
                let id = format!("setup-accent-{key}");
                div()
                    .id(SharedString::from(id.clone()))
                    .debug_selector(move || id.clone())
                    .role(Role::Button)
                    .aria_label(label)
                    .aria_toggled(if on { Toggled::True } else { Toggled::False })
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(kit::focus_ring(theme))
                    .size(px(24.0))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_2()
                    .border_color(if on { fill } else { gpui::transparent_black() })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.set_accent(name, cx)))
                    .child(div().size(px(16.0)).rounded_full().bg(fill))
            }),
        );
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(14.0))
            .child(mode)
            .child(themes)
            .child(accents)
            .into_any_element()
    }

    /// The projects Reno knows, one row each; the chosen one is marked.
    pub(crate) fn project_rows(
        &self,
        theme: &Theme,
        row_height: f32,
        mono_paths: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let projects = self.projects(cx);
        if projects.is_empty() {
            return div()
                .text_size(px(12.5))
                .text_color(theme.muted)
                .child("No project found yet. Start a task in a folder and it shows up here.")
                .into_any_element();
        }
        let chosen = self.setup.project.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(projects.into_iter().map(|project: ProjectLine| {
                let on = chosen.as_deref() == Some(project.name.as_str());
                let id = format!("setup-project-{}", project.name);
                let hover = theme.raised;
                let path = project.path.display().to_string();
                let pick = project.clone();
                div()
                    .id(SharedString::from(id.clone()))
                    .debug_selector(move || id.clone())
                    .h(px(row_height))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(if on {
                        theme.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .when(on, |r| r.bg(theme.accent.opacity(0.08)))
                    .when(!on, |r| r.hover(move |s| s.bg(hover)))
                    .cursor_pointer()
                    .role(Role::Button)
                    .aria_label(SharedString::from(format!("Use {}", project.name)))
                    .aria_toggled(if on { Toggled::True } else { Toggled::False })
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(kit::focus_ring(theme))
                    .on_click(cx.listener(move |this, _, _, cx| this.choose_project(&pick, cx)))
                    .child(icon(Icon::PhFolder, 15.0, theme.muted))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .child(project.name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(if mono_paths { 12.0 } else { 12.5 }))
                            .text_color(theme.faint)
                            .when(mono_paths, |d| d.font_family(kit::MONO))
                            .child(path),
                    )
                    .when(on, |r| r.child(icon(Icon::PhCheck, 14.0, theme.accent)))
            }))
            .into_any_element()
    }
}
