//! The frame around a code block or a diagram: its Copy and wrap controls (TRANSCRIPT-7.1, 7.2,
//! 10.6). They are icon buttons at the top right, shown only while the pointer is over the block
//! (or a button has keyboard focus). A diagram also has Preview and Code tabs (TRANSCRIPT-10.1).
//!
//! Copy puts only the block's code on the clipboard and says so with a toast. Whether a block
//! wraps, which tab a diagram is on, and how far it is zoomed, are remembered per
//! block in bounded stores, so they survive the row scrolling away.

use std::hash::{Hash, Hasher};
use std::time::Duration;

use gpui::{AnyElement, App, ClipboardItem, ElementId, Stateful, Task, div, prelude::*, px};

use super::lru::Lru;
use crate::ui::icon::Icon;
use crate::ui::kit;
use crate::ui::theme::Theme;
use crate::ui::toast;

/// Blocks whose wrap choice, and diagrams whose tab, is remembered.
const WRAP_ENTRIES: usize = 512;

/// What a frame is around.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Kind {
    Code,
    Diagram,
}

/// A frame's contents and what its controls do.
pub(crate) struct Card<'a> {
    pub(crate) kind: Kind,
    /// What Copy puts on the clipboard.
    pub(crate) copy: &'a str,
    /// Identifies the block for its remembered wrap choice.
    pub(crate) key: u64,
    /// Show a wrap toggle (a diagram has none).
    pub(crate) wrappable: bool,
    pub(crate) wrapped: bool,
    /// Which block of its draw this is: two identical blocks share a `key` (and a wrap choice)
    /// but not an element id.
    pub(crate) ordinal: usize,
}

struct Wraps(Lru<u64, bool>);

impl gpui::Global for Wraps {}

impl Default for Wraps {
    fn default() -> Self {
        Self(Lru::new(WRAP_ENTRIES, usize::MAX))
    }
}

/// Which face of a diagram shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DiagramTab {
    /// The drawing.
    #[default]
    Preview,
    /// The source.
    Code,
}

/// The tab each diagram is on, keyed by the hash of its source: two identical diagrams share a
/// tab, and an edited source is a new key, so it opens on Preview again.
struct Tabs(Lru<u64, DiagramTab>);

impl gpui::Global for Tabs {}

impl Default for Tabs {
    fn default() -> Self {
        Self(Lru::new(WRAP_ENTRIES, usize::MAX))
    }
}

/// The tab the diagram `key` is on: its drawing until the owner picks the source.
pub(crate) fn tab(key: u64, cx: &mut App) -> DiagramTab {
    cx.default_global::<Tabs>()
        .0
        .get(&key)
        .copied()
        .unwrap_or_default()
}

/// Puts the diagram `key` on `tab` and redraws.
pub(crate) fn set_tab(key: u64, tab: DiagramTab, cx: &mut App) {
    cx.default_global::<Tabs>().0.insert(key, tab, 1);
    super::settled(cx);
}

/// The most a diagram is zoomed: twice its natural size.
pub(crate) const MAX_ZOOM: f32 = 2.0;
/// How long after the last zoom step the drawing is made again at the new size.
pub(crate) const RERASTER_AFTER: Duration = Duration::from_millis(300);
/// How much one point of wheel travel zooms, as a rate of the current size.
const ZOOM_PER_POINT: f32 = 0.002;
/// A zoom this near the fit is the fit.
const FIT_EPSILON: f32 = 0.005;

/// How far a diagram is zoomed in: a share of its natural size. A diagram that is not zoomed fits
/// the column, and has no `Zoom`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Zoom {
    pub(crate) factor: f32,
}

impl Zoom {
    /// The zoom after the wheel travelled `delta` points (up zooms in) from `from`, never below
    /// `fit` (the share of its size at which it fits the column) nor above [`MAX_ZOOM`]. `None`
    /// is the fit itself.
    fn stepped(from: f32, delta: f32, fit: f32) -> Option<Zoom> {
        let factor = (from.max(fit) * (delta * ZOOM_PER_POINT).exp()).min(MAX_ZOOM);
        (factor > fit + FIT_EPSILON).then_some(Zoom { factor })
    }

    /// The level as the readout says it.
    pub(crate) fn percent(self) -> u32 {
        (self.factor * 100.0).round() as u32
    }
}

/// A diagram's zoom, the share of its size its drawing was last made at, and the deadline that
/// makes it again.
struct Zoomed {
    zoom: Zoom,
    /// Until the deadline fires the old drawing is stretched to the new size.
    raster: f32,
    /// Dropping it (a new step, a reset, the store forgetting the diagram) cancels the wait.
    due: Option<Task<()>>,
}

/// The zoom of each diagram, keyed by the hash of its source like [`Tabs`].
struct Zooms {
    by_key: Lru<u64, Zoomed>,
    #[cfg(test)]
    fired: usize,
}

impl gpui::Global for Zooms {}

impl Default for Zooms {
    fn default() -> Self {
        Self {
            by_key: Lru::new(WRAP_ENTRIES, usize::MAX),
            #[cfg(test)]
            fired: 0,
        }
    }
}

/// How far the diagram `key` is zoomed; `None` while it fits the column.
pub(crate) fn zoom(key: u64, cx: &mut App) -> Option<Zoom> {
    cx.default_global::<Zooms>()
        .by_key
        .get(&key)
        .map(|held| held.zoom)
}

/// The share of its natural size the diagram `key` is drawn at: the size it had when the owner
/// last stopped zooming, and one while it fits.
pub(crate) fn raster_factor(key: u64, cx: &mut App) -> f32 {
    cx.default_global::<Zooms>()
        .by_key
        .get(&key)
        .map_or(1.0, |held| held.raster)
}

/// Back to fit.
pub(crate) fn reset_zoom(key: u64, cx: &mut App) {
    if cx.default_global::<Zooms>().by_key.remove(&key).is_some() {
        super::settled(cx);
    }
}

/// The wheel travelled `delta` points over the diagram `key`, which fits the column at `fit` of
/// its size: zoom, and arm the redraw at the new size [`RERASTER_AFTER`] after the last step. A
/// step that changes nothing (at either end) arms nothing. Whether the drawing got bigger.
pub(crate) fn zoom_by(key: u64, delta: f32, fit: f32, cx: &mut App) -> bool {
    let zooms = &mut cx.default_global::<Zooms>().by_key;
    let held = zooms.remove(&key);
    let from = held.as_ref().map_or(fit, |held| held.zoom.factor);
    let next = Zoom::stepped(from, delta, fit);
    let unchanged = match (&held, next) {
        (Some(held), Some(next)) => held.zoom == next,
        (None, None) => true,
        _ => false,
    };
    if unchanged {
        if let Some(held) = held {
            zooms.insert(key, held, 1);
        }
        return false;
    }
    let grew = next.is_some_and(|zoom| zoom.factor > from);
    if let Some(zoom) = next {
        let raster = held.as_ref().map_or(1.0, |held| held.raster);
        let due = cx.spawn(async move |cx| {
            cx.background_executor().timer(RERASTER_AFTER).await;
            cx.update(|cx| draw_again_at(key, cx));
        });
        cx.default_global::<Zooms>().by_key.insert(
            key,
            Zoomed {
                zoom,
                raster,
                due: Some(due),
            },
            1,
        );
    }
    super::settled(cx);
    grew
}

/// The deadline: the drawing of `key` is made again at the size the owner settled on.
fn draw_again_at(key: u64, cx: &mut App) {
    let zooms = cx.default_global::<Zooms>();
    let Some(mut held) = zooms.by_key.remove(&key) else {
        return;
    };
    held.raster = held.zoom.factor.max(1.0);
    if let Some(done) = held.due.take() {
        done.detach();
    }
    zooms.by_key.insert(key, held, 1);
    #[cfg(test)]
    {
        zooms.fired += 1;
    }
    super::settled(cx);
}

/// How many times the deadline has fired, for its test.
#[cfg(test)]
pub(crate) fn rerasters(cx: &mut App) -> usize {
    cx.default_global::<Zooms>().fired
}

/// The zoom readout and its reset, at the right of the tabs while the drawing is zoomed.
fn zoom_readout(key: u64, zoom: Zoom, theme: &Theme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .ml(px(10.0))
        .child(
            div()
                .debug_selector(|| "diagram-zoom-level".to_owned())
                .text_size(px(12.0))
                .text_color(theme.muted)
                .child(format!("{}%", zoom.percent())),
        )
        .child(
            kit::line_button(
                theme,
                "diagram-zoom-reset",
                None,
                "Reset zoom",
                kit::Tone::Quiet,
                true,
            )
            .on_click(move |_, _, cx| reset_zoom(key, cx)),
        )
        .into_any_element()
}

/// The Preview and Code tabs of the diagram `key`, `current` pressed, and the zoom readout when
/// the drawing is `zoomed`.
pub(crate) fn tab_bar(
    key: u64,
    current: DiagramTab,
    zoomed: Option<Zoom>,
    theme: &Theme,
) -> AnyElement {
    let tab = |face, id, label| {
        kit::segment(theme, id, label, current == face)
            .on_click(move |_, _, cx| set_tab(key, face, cx))
    };
    div()
        .flex()
        .items_center()
        .px(px(8.0))
        .pt(px(8.0))
        .child(kit::segmented(theme).children([
            tab(DiagramTab::Preview, "diagram-tab-preview", "Preview"),
            tab(DiagramTab::Code, "diagram-tab-code", "Code"),
        ]))
        .children(
            zoomed
                .filter(|_| current == DiagramTab::Preview)
                .map(|zoom| zoom_readout(key, zoom, theme)),
        )
        .into_any_element()
}

/// A key for a block: its kind, language and text.
pub(crate) fn key_of(language: Option<&str>, text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    language.hash(&mut hasher);
    text.hash(&mut hasher);
    hasher.finish()
}

/// Whether the block `key` wraps its long lines. Unwrapped (scrolling sideways) until the owner
/// says otherwise.
pub(crate) fn wraps(key: u64, cx: &mut App) -> bool {
    cx.default_global::<Wraps>()
        .0
        .get(&key)
        .copied()
        .unwrap_or(false)
}

/// Flips the block's wrap choice and redraws.
pub(crate) fn toggle_wrap(key: u64, cx: &mut App) {
    let wraps = cx.default_global::<Wraps>();
    let now = wraps.0.get(&key).copied().unwrap_or(false);
    wraps.0.insert(key, !now, 1);
    super::settled(cx);
}

/// Puts `text` on the clipboard and says so.
pub(crate) fn copy(text: &str, what: &'static str, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
    toast::push(cx, toast::Tone::Info, "Copied", what);
}

/// A control that shows only while the pointer is over the block or it has keyboard focus.
fn button(theme: &Theme, label: &'static str, glyph: Icon) -> Stateful<gpui::Div> {
    let accent = theme.accent;
    kit::icon_button(theme, label, glyph, 24.0)
        .opacity(0.0)
        .group_hover("md-card", |s| s.opacity(1.0))
        .focus_visible(move |s| s.opacity(1.0).border_1().border_color(accent))
}

fn controls(card: &Card<'_>, theme: &Theme) -> Vec<Stateful<gpui::Div>> {
    let code = card.copy.to_owned();
    let what = match card.kind {
        Kind::Code => "code",
        Kind::Diagram => "diagram source",
    };
    let mut out = vec![
        button(theme, "Copy code", Icon::Copy)
            .tooltip(kit::tip("Copy"))
            .on_click(move |_, _, cx| copy(&code, what, cx)),
    ];
    if card.wrappable {
        let key = card.key;
        let (label, tip) = if card.wrapped {
            ("Unwrap lines", "Scroll long lines")
        } else {
            ("Wrap lines", "Wrap long lines")
        };
        let mut b = button(theme, label, Icon::Wrap)
            .tooltip(kit::tip(tip))
            .on_click(move |_, _, cx| toggle_wrap(key, cx));
        if card.wrapped {
            b = b.bg(theme.raised);
        }
        out.push(b);
    }
    out
}

fn frame(theme: &Theme) -> gpui::Div {
    div()
        .group("md-card")
        .debug_selector(|| "md-card".to_owned())
        .relative()
        .w_full()
        .min_w_0()
        .rounded(px(10.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
        .overflow_hidden()
}

/// `body` inside its frame, with the controls at the top right.
pub(crate) fn card(card: &Card<'_>, theme: &Theme, body: AnyElement) -> AnyElement {
    let id = ElementId::Name(format!("md-card-{}-{}", card.key, card.ordinal).into());
    let surface = theme.surface.opacity(0.92);
    frame(theme)
        .id(id)
        .child(body)
        .child(
            div()
                .absolute()
                .top(px(6.0))
                .right(px(6.0))
                .flex()
                .gap(px(2.0))
                .p(px(1.0))
                .rounded(px(9.0))
                .group_hover("md-card", move |s| s.bg(surface))
                .children(controls(card, theme)),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn wrap_is_off_until_chosen_remembered_per_block_and_bounded(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let key = key_of(Some("rust"), "fn a() {}");
            assert!(!wraps(key, cx));
            toggle_wrap(key, cx);
            assert!(wraps(key, cx));
            assert!(!wraps(key_of(Some("rust"), "fn b() {}"), cx), "per block");
            toggle_wrap(key, cx);
            assert!(!wraps(key, cx));
            for n in 0..(WRAP_ENTRIES + 50) {
                toggle_wrap(key_of(None, &n.to_string()), cx);
            }
            assert!(cx.default_global::<Wraps>().0.len() <= WRAP_ENTRIES);
        });
    }

    #[test]
    fn a_zoom_step_is_held_between_fit_and_twice_the_size() {
        let fit = 0.8;
        assert_eq!(Zoom::stepped(fit, -300.0, fit), None, "out from fit is fit");
        assert_eq!(Zoom::stepped(fit, 0.0, fit), None, "no travel, no zoom");
        let in_a_notch = Zoom::stepped(fit, 60.0, fit).expect("zoomed in");
        assert!(in_a_notch.factor > fit && in_a_notch.factor < MAX_ZOOM);
        assert_eq!(Zoom::stepped(1.9, 1000.0, fit).map(|z| z.factor), Some(2.0));
        assert_eq!(Zoom::stepped(1.3, -1000.0, fit), None, "all the way out");
        let half_way = Zoom::stepped(1.5, -100.0, fit).expect("still zoomed");
        assert!(half_way.factor < 1.5 && half_way.factor > fit);
        // A column that grew since the zoom was set: a zoom under the new fit steps from the fit.
        let stale = Zoom::stepped(0.6, 60.0, 0.9).expect("zoomed in from fit");
        assert!(stale.factor > 0.9);
        assert_eq!(Zoom { factor: 1.5 }.percent(), 150);
    }

    #[gpui::test]
    fn a_diagram_opens_on_its_drawing_and_remembers_its_tab_per_block(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let key = key_of(Some("mermaid"), "flowchart LR\n A --> B");
            assert_eq!(tab(key, cx), DiagramTab::Preview);
            set_tab(key, DiagramTab::Code, cx);
            assert_eq!(tab(key, cx), DiagramTab::Code);
            assert_eq!(
                tab(key_of(Some("mermaid"), "other"), cx),
                DiagramTab::Preview
            );
            set_tab(key, DiagramTab::Preview, cx);
            assert_eq!(tab(key, cx), DiagramTab::Preview);
            for n in 0..(WRAP_ENTRIES + 50) {
                set_tab(key_of(None, &n.to_string()), DiagramTab::Code, cx);
            }
            assert!(cx.default_global::<Tabs>().0.len() <= WRAP_ENTRIES);
        });
    }

    /// Copy and Wrap are hidden until the pointer is over the block: at rest they have no
    /// opacity. A diagram has the same Copy, and no Wrap.
    #[test]
    fn the_controls_are_hidden_at_rest_on_a_code_block_and_a_diagram() {
        let theme = Theme::for_appearance(crate::ui::theme::Appearance::Dark);
        let card = |kind, wrappable| Card {
            kind,
            copy: "x",
            key: 1,
            wrappable,
            wrapped: false,
            ordinal: 0,
        };
        let code = controls(&card(Kind::Code, true), &theme);
        let diagram = controls(&card(Kind::Diagram, false), &theme);
        assert_eq!((code.len(), diagram.len()), (2, 1), "Copy and Wrap; Copy");
        for mut control in code.into_iter().chain(diagram) {
            assert_eq!(control.style().opacity, Some(0.0));
        }
    }
}
