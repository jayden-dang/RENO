//! Drawing the panel: the tab strip, the Home page and a placeholder tab. These are plain render
//! functions over the model (`PanelState`, `vm::panel_home::Home`); a click goes out as a [`Cmd`]
//! through one handler, like `ui/sidebar.rs`, so the run screen decides what each one does.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, FontWeight, Global, Hsla, IntoElement, Render, SharedString,
    Stateful, Window, div, prelude::*, px,
};

use super::PanelState;
use super::surface::{Counts, Surface};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::theme::Theme;
use crate::vm::panel_home::{Home, HomeKind, HomeRow, Now, NowTone};

/// What a click in the panel asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Cmd {
    /// Show an open tab.
    Activate(Surface),
    Close(Surface),
    /// `+`: a New tab showing the home.
    NewTab,
    /// The hide button.
    Hide,
    /// A drag dropped the tab at `from` onto the one at `to`.
    Reorder {
        from: usize,
        to: usize,
    },
    /// A home row (or its letter).
    Open(HomeKind),
    /// A recently closed row.
    Reopen(Surface),
    /// The dashed "+ Add" of Actions.
    AddAction,
}

pub(crate) type Emit = Rc<dyn Fn(Cmd, &mut Window, &mut App)>;

/// What the person chose about the panel that outlives one run screen: whether it is open, and
/// how wide. `None` is "not chosen": open when the window is wide enough to split, and the
/// default share of the stage.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Prefs {
    pub(crate) open: Option<bool>,
    pub(crate) width: Option<f32>,
}

impl Global for Prefs {}

impl Prefs {
    pub(crate) fn get(cx: &App) -> Self {
        cx.try_global::<Self>().copied().unwrap_or_default()
    }

    pub(crate) fn set(self, cx: &mut App) {
        cx.set_global(self);
    }
}

/// The window's width that is not the run screen's stage: the sidebar and the insets. Set by
/// the root view each frame, so the first frame of a run screen already fits its panel.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StageInset(pub(crate) f32);

impl Global for StageInset {}

pub(crate) fn stage_inset(cx: &App) -> f32 {
    cx.try_global::<StageInset>().map_or(0.0, |s| s.0)
}

// --- the strip -----------------------------------------------------------------------------------

/// What a tab drag carries; it is also the little label that follows the pointer.
#[derive(Clone, Debug)]
pub(crate) struct TabDrag {
    pub(crate) from: usize,
    pub(crate) title: SharedString,
    pub(crate) glyph: Icon,
    pub(crate) fill: Hsla,
    pub(crate) ink: Hsla,
}

impl Render for TabDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(px(28.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(8.0))
            .bg(self.fill)
            .text_size(px(13.0))
            .text_color(self.ink)
            .child(icon(self.glyph, 13.0, self.ink))
            .child(self.title.clone())
    }
}

/// The hover fill: the text colour at a whisper.
fn hover_fill(theme: &Theme) -> Hsla {
    theme.text.opacity(0.06)
}

/// The 42 px strip: Session, the open tabs, `+`, and the hide button at the far right.
pub(crate) fn tab_strip(
    theme: &Theme,
    state: &PanelState,
    counts: &Counts,
    emit: &Emit,
) -> impl IntoElement {
    let tabs = state
        .tabs()
        .iter()
        .enumerate()
        .map(|(ix, surface)| tab(theme, ix, surface, surface == state.active(), counts, emit));
    let new_tab = {
        let emit = emit.clone();
        kit::icon_button(theme, "panel-new-tab", Icon::Plus, 28.0)
            .on_click(move |_, window, cx| emit(Cmd::NewTab, window, cx))
    };
    let hide = {
        let emit = emit.clone();
        kit::icon_button(theme, "panel-hide", Icon::Sidebar, 28.0)
            .on_click(move |_, window, cx| emit(Cmd::Hide, window, cx))
    };
    div()
        .id("panel-tabs")
        .debug_selector(|| "panel-tabs".to_owned())
        .flex_none()
        .h(px(42.0))
        .px(px(8.0))
        .flex()
        .items_center()
        .gap(px(2.0))
        .border_b_1()
        .border_color(theme.hairline)
        .child(
            div()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(2.0))
                .overflow_hidden()
                .children(tabs),
        )
        .child(new_tab)
        .child(div().flex_1())
        .child(hide)
}

fn tab(
    theme: &Theme,
    ix: usize,
    surface: &Surface,
    active: bool,
    counts: &Counts,
    emit: &Emit,
) -> Stateful<gpui::Div> {
    let slug = surface.slug();
    let selector = format!("panel-tab-{slug}");
    let ink = if active { theme.text } else { theme.muted };
    let hover = hover_fill(theme);
    let title: SharedString = surface.title().into();
    let closable = surface.closable();
    let pinned = !closable;
    let target = surface.clone();
    let on_click = emit.clone();
    let drag = TabDrag {
        from: ix,
        title: title.clone(),
        glyph: surface.icon(),
        fill: theme.raised,
        ink: theme.text,
    };
    let on_drop = emit.clone();
    let tab = div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex_none()
        .h(px(28.0))
        .pl(px(9.0))
        .pr(px(if closable { 4.0 } else { 10.0 }))
        .flex()
        .items_center()
        .gap(px(6.0))
        .rounded(px(8.0))
        .text_size(px(13.0))
        .text_color(ink)
        .cursor_pointer()
        .when(active, |t| t.bg(theme.raised))
        .when(!active, |t| t.hover(move |s| s.bg(hover)))
        .child(icon(surface.icon(), 13.0, ink))
        .child(title)
        .children(surface.count(counts).map(|n| {
            div()
                .text_size(px(12.0))
                .text_color(theme.faint)
                .child(n.to_string())
        }))
        .on_click(move |_, window, cx| on_click(Cmd::Activate(target.clone()), window, cx));
    // Session stays first, so it neither drags nor takes a drop.
    let tab = if pinned {
        tab
    } else {
        let accent = theme.accent.opacity(0.18);
        tab.on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
            .drag_over::<TabDrag>(move |s, _, _, _| s.bg(accent))
            .on_drop::<TabDrag>(move |dragged, window, cx| {
                on_drop(
                    Cmd::Reorder {
                        from: dragged.from,
                        to: ix,
                    },
                    window,
                    cx,
                );
            })
    };
    if closable {
        let close = emit.clone();
        let target = surface.clone();
        let selector = format!("panel-tab-close-{slug}");
        tab.child(
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .flex_none()
                .size(px(20.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .hover(move |s| s.bg(hover))
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    close(Cmd::Close(target.clone()), window, cx);
                })
                .child(icon(Icon::X, 11.0, theme.faint)),
        )
    } else {
        tab
    }
}

// --- the Home page -------------------------------------------------------------------------------

fn tone_dot(theme: &Theme, tone: NowTone) -> Hsla {
    match tone {
        NowTone::Running => theme.accent,
        NowTone::Waiting | NowTone::Blocked => theme.warning,
        NowTone::Done => theme.success,
        NowTone::Failed => theme.danger,
        NowTone::Stopped => theme.faint,
    }
}

/// The colour of the state word: semantic where the state asks for you, plain when it is done.
pub(crate) fn tone_ink(theme: &Theme, tone: NowTone) -> Hsla {
    match tone {
        NowTone::Done => theme.text,
        other => tone_dot(theme, other),
    }
}

/// "held for you" as "Held for you.": the state word, capitalised, with its full stop.
pub(crate) fn lead_word(lead: &str) -> String {
    let mut chars = lead.chars();
    match chars.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

/// The finding sentence: the state word in bold and in its colour, then the rest, on one line
/// that wraps.
pub(crate) fn finding_text(theme: &Theme, now: &Now, muted: Hsla) -> gpui::Div {
    div()
        .min_w_0()
        .flex()
        .flex_wrap()
        .gap_x(px(5.0))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tone_ink(theme, now.tone))
                .child(SharedString::from(lead_word(&now.lead))),
        )
        .when(!now.text.is_empty(), |d| {
            d.child(
                div()
                    .text_color(muted)
                    .child(SharedString::from(now.text.clone())),
            )
        })
}

/// A key's cap: a small bordered box, in mono.
fn keycap(theme: &Theme, label: &str) -> gpui::Div {
    div()
        .flex_none()
        .min_w(px(20.0))
        .h(px(20.0))
        .px(px(5.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.0))
        .border_1()
        .border_color(theme.hairline)
        .font_family(MONO)
        .text_size(px(11.0))
        .text_color(theme.faint)
        .child(SharedString::from(label.to_owned()))
}

fn group_label(theme: &Theme, label: impl Into<SharedString>) -> gpui::Div {
    div()
        .px(px(10.0))
        .pb(px(6.0))
        .text_size(px(12.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.faint)
        .child(label.into())
}

/// One 36 px row: icon, label, a detail that truncates, the keycap. A disabled row stays,
/// dimmed, with its reason as the detail.
fn home_row(theme: &Theme, row: &HomeRow, emit: &Emit) -> Stateful<gpui::Div> {
    let slug = row.label.to_lowercase().replace(' ', "-");
    let selector = format!("home-row-{slug}");
    let hover = hover_fill(theme);
    let (label_ink, detail_ink) = if row.enabled {
        (theme.text, theme.muted)
    } else {
        (theme.faint, theme.faint)
    };
    let kind = row.kind;
    let emit = emit.clone();
    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .h(px(36.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(11.0))
        .rounded(px(8.0))
        .text_size(px(13.5))
        .when(row.enabled, |r| {
            r.cursor_pointer()
                .hover(move |s| s.bg(hover))
                .on_click(move |_, window, cx| emit(Cmd::Open(kind), window, cx))
        })
        .when(!row.enabled, |r| r.cursor_default())
        .child(icon(
            row.kind.icon(),
            15.0,
            if row.enabled {
                theme.muted
            } else {
                theme.faint
            },
        ))
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::MEDIUM)
                .text_color(label_ink)
                .child(row.label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(12.5))
                .text_color(detail_ink)
                .child(SharedString::from(row.detail.clone())),
        )
        .child(
            div()
                .when(!row.enabled, |d| d.opacity(0.4))
                .child(keycap(theme, &row.letter.to_string())),
        )
}

/// The "New tab" page.
pub(crate) fn home_page(theme: &Theme, home: &Home, closed: &[Surface], emit: &Emit) -> AnyElement {
    let place = &home.place;
    let where_line = div()
        .flex()
        .items_center()
        .gap(px(7.0))
        .min_w_0()
        .text_size(px(13.0))
        .font_weight(FontWeight::MEDIUM)
        .children(place.project.as_ref().map(|name| {
            div()
                .flex()
                .items_center()
                .gap(px(7.0))
                .child(icon(Icon::Folder, 14.0, theme.muted))
                .child(SharedString::from(name.clone()))
        }))
        .children(place.branch.as_ref().map(|branch| {
            div()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(7.0))
                .when(place.project.is_some(), |d| {
                    d.child(div().text_color(theme.faint).child("/"))
                })
                .child(icon(Icon::Branch, 14.0, theme.muted))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_family(MONO)
                        .text_size(px(12.0))
                        .font_weight(FontWeight::NORMAL)
                        .child(SharedString::from(branch.clone())),
                )
        }))
        .child(div().flex_1())
        .children(place.diff.as_ref().map(|pill| {
            let open = emit.clone();
            div()
                .id("home-diff-pill")
                .debug_selector(|| "home-diff-pill".to_owned())
                .flex_none()
                .px(px(9.0))
                .py(px(2.0))
                .rounded(px(999.0))
                .border_1()
                .border_color(theme.hairline)
                .font_family(MONO)
                .text_size(px(11.5))
                .cursor_pointer()
                .hover({
                    let fill = hover_fill(theme);
                    move |s| s.bg(fill)
                })
                .flex()
                .gap(px(6.0))
                .when(pill.additions > 0, |d| {
                    d.child(
                        div()
                            .text_color(theme.success)
                            .child(format!("+{}", pill.additions)),
                    )
                })
                .when(pill.deletions > 0, |d| {
                    d.child(
                        div()
                            .text_color(theme.danger)
                            .child(format!("\u{2212}{}", pill.deletions)),
                    )
                })
                .on_click(move |_, window, cx| open(Cmd::Open(HomeKind::Changes), window, cx))
        }));

    let now = &home.now;
    let now_line = div()
        .id("home-now")
        .debug_selector(|| "home-now".to_owned())
        .flex()
        .items_start()
        .gap(px(9.0))
        .text_size(px(14.0))
        .line_height(px(21.0))
        .child(
            div()
                .flex_none()
                .pt(px(7.0))
                .child(kit::dot(tone_dot(theme, now.tone), 7.0)),
        )
        .child(finding_text(theme, now, theme.muted));

    let meta = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .text_size(px(12.0))
        .text_color(theme.muted)
        .child(kit::exec_glyph(theme, &home.meta.executor, 13.0))
        .child(SharedString::from(home.meta.text()));

    let head = div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .pb(px(20.0))
        .border_b_1()
        .border_color(theme.hairline)
        .child(where_line)
        .child(now_line)
        .child(meta);

    let group = |label: &'static str, rows: &[HomeRow]| {
        div()
            .flex()
            .flex_col()
            .gap(px(1.0))
            .child(group_label(theme, label))
            .children(rows.iter().map(|row| home_row(theme, row, emit)))
    };

    let actions = {
        let add = emit.clone();
        div()
            .flex()
            .flex_col()
            .child(group_label(
                theme,
                format!(
                    "Actions \u{b7} {}",
                    home.place.project.as_deref().unwrap_or("this folder")
                ),
            ))
            .child(
                div().px(px(10.0)).flex().flex_wrap().gap(px(6.0)).child(
                    kit::line_button(
                        theme,
                        "home-add-action",
                        Some(Icon::Plus),
                        "Add",
                        Tone::Quiet,
                        true,
                    )
                    .border_dashed()
                    .text_color(theme.muted)
                    .on_click(move |_, window, cx| add(Cmd::AddAction, window, cx)),
                ),
            )
    };

    let recent = (!closed.is_empty()).then(|| {
        div()
            .flex()
            .flex_col()
            .gap(px(1.0))
            .child(group_label(theme, "Recently closed"))
            .children(closed.iter().enumerate().map(|(n, surface)| {
                let emit = emit.clone();
                let target = surface.clone();
                let selector = format!("home-closed-{n}");
                div()
                    .id(SharedString::from(selector.clone()))
                    .debug_selector(move || selector.clone())
                    .h(px(36.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(11.0))
                    .rounded(px(8.0))
                    .text_size(px(13.5))
                    .cursor_pointer()
                    .hover({
                        let fill = hover_fill(theme);
                        move |s| s.bg(fill)
                    })
                    .on_click(move |_, window, cx| emit(Cmd::Reopen(target.clone()), window, cx))
                    .child(icon(surface.icon(), 15.0, theme.muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(SharedString::from(surface.title())),
                    )
                    .when(n == 0, |r| r.child(keycap(theme, "\u{2318}\u{21e7}T")))
            }))
    });

    let hint = div()
        .px(px(10.0))
        .text_size(px(11.5))
        .text_color(theme.faint)
        .child("Press a letter to open \u{b7} \u{2318}T new tab \u{b7} \u{2318}\u{21e7}T reopen closed");

    div()
        .id("panel-home")
        .debug_selector(|| "panel-home".to_owned())
        .size_full()
        .overflow_y_scroll()
        .flex()
        .justify_center()
        .child(
            div()
                .w_full()
                .max_w(px(520.0))
                .px(px(22.0))
                .pt(px(26.0))
                .pb(px(28.0))
                .flex()
                .flex_col()
                .gap(px(22.0))
                .child(head)
                .child(group("Inspect", &home.inspect))
                .child(group("Work", &home.work))
                .child(actions)
                .children(recent)
                .child(hint),
        )
        .into_any_element()
}

/// The page of a tab whose feature is not built: what it will be, said once.
pub(crate) fn planned_page(theme: &Theme, kind: HomeKind) -> AnyElement {
    div()
        .id("panel-planned")
        .debug_selector(move || {
            format!(
                "panel-planned-{}",
                kind.label().to_lowercase().replace(' ', "-")
            )
        })
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
                .child(icon(kind.icon(), 20.0, theme.muted))
                .child(
                    div()
                        .text_size(px(15.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(kind.label()),
                )
                .child(
                    div()
                        .text_size(px(13.5))
                        .text_color(theme.muted)
                        .child(format!("{} Arrives in a later phase.", kind.planned_note())),
                ),
        )
        .into_any_element()
}
