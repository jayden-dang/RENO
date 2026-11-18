//! The fit policy: how the panel and the chat share the stage, or whether the panel overlays it.
//! Pure arithmetic on pixels, so every boundary is a unit test.

/// The panel never gets narrower than this.
pub(crate) const PANEL_MIN: f32 = 440.0;
/// The chat keeps at least this much when the panel splits the stage.
pub(crate) const CHAT_MIN: f32 = 420.0;
/// The gap between the two canvases, which is also the resize gutter.
pub(crate) const GUTTER: f32 = 8.0;
/// Below this window width the panel overlays the chat instead of splitting it.
pub(crate) const OVERLAY_BELOW: f32 = 1100.0;
/// The panel's width when nothing is saved, as a share of the stage.
pub(crate) const DEFAULT_SHARE: f32 = 0.46;

/// What the run screen draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Layout {
    /// The panel is closed; the chat has the stage.
    Hidden,
    /// Two canvases side by side.
    Split { panel_w: f32, chat_w: f32 },
    /// The panel floats over the chat, which keeps the whole stage.
    Overlay { panel_w: f32 },
}

/// Decide the layout. `window_w` is the window's width, `stage_w` the width left for the chat
/// and the panel (the window less the sidebar). `saved` is the width the person dragged to.
pub(crate) fn fit(window_w: f32, stage_w: f32, saved: Option<f32>, open: bool) -> Layout {
    if !open {
        return Layout::Hidden;
    }
    let stage_w = stage_w.max(0.0);
    let wanted = saved
        .filter(|w| w.is_finite())
        .unwrap_or(stage_w * DEFAULT_SHARE);
    let split_room = stage_w - CHAT_MIN - GUTTER;
    if window_w < OVERLAY_BELOW || split_room < PANEL_MIN {
        // The overlay may use the whole stage, but never less than the minimum.
        let panel_w = wanted.clamp(PANEL_MIN, stage_w.max(PANEL_MIN));
        return Layout::Overlay { panel_w };
    }
    let panel_w = wanted.clamp(PANEL_MIN, split_room);
    Layout::Split {
        panel_w,
        chat_w: stage_w - panel_w - GUTTER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_is_hidden() {
        assert_eq!(fit(1600.0, 1300.0, None, false), Layout::Hidden);
    }

    #[test]
    fn default_is_46_percent_of_the_stage() {
        let Layout::Split { panel_w, chat_w } = fit(1600.0, 1300.0, None, true) else {
            panic!("expected split");
        };
        assert!((panel_w - 598.0).abs() < 0.01);
        assert!((chat_w - (1300.0 - panel_w - GUTTER)).abs() < 0.01);
    }

    #[test]
    fn default_respects_the_panel_minimum() {
        // 46% of 900 is 414, raised to 440 (room: 900 - 428 = 472).
        let Layout::Split { panel_w, .. } = fit(1200.0, 900.0, None, true) else {
            panic!("expected split");
        };
        assert_eq!(panel_w, PANEL_MIN);
    }

    #[test]
    fn saved_width_clamps_low_and_high() {
        let Layout::Split { panel_w, .. } = fit(1600.0, 1300.0, Some(100.0), true) else {
            panic!("expected split")
        };
        assert_eq!(panel_w, PANEL_MIN);
        let Layout::Split { panel_w, chat_w } = fit(1600.0, 1300.0, Some(5000.0), true) else {
            panic!("expected split")
        };
        assert_eq!(chat_w, CHAT_MIN);
        assert_eq!(panel_w, 1300.0 - CHAT_MIN - GUTTER);
    }

    #[test]
    fn saved_width_inside_the_range_is_kept() {
        assert_eq!(
            fit(1600.0, 1300.0, Some(700.0), true),
            Layout::Split {
                panel_w: 700.0,
                chat_w: 592.0
            }
        );
    }

    #[test]
    fn non_finite_saved_width_falls_back_to_default() {
        let a = fit(1600.0, 1300.0, Some(f32::NAN), true);
        assert_eq!(a, fit(1600.0, 1300.0, None, true));
    }

    #[test]
    fn overlay_boundary_is_1100() {
        assert!(matches!(
            fit(1099.9, 1000.0, None, true),
            Layout::Overlay { .. }
        ));
        assert!(matches!(
            fit(1100.0, 1000.0, None, true),
            Layout::Split { .. }
        ));
    }

    #[test]
    fn overlay_when_the_stage_cannot_hold_both_minimums() {
        // 440 + 420 + 8 = 868.
        assert!(matches!(
            fit(1100.0, 867.0, None, true),
            Layout::Overlay { .. }
        ));
        assert!(matches!(
            fit(1100.0, 868.0, None, true),
            Layout::Split { .. }
        ));
    }

    #[test]
    fn overlay_width_is_clamped_to_the_stage() {
        let Layout::Overlay { panel_w } = fit(900.0, 700.0, Some(5000.0), true) else {
            panic!("expected overlay")
        };
        assert_eq!(panel_w, 700.0);
        let Layout::Overlay { panel_w } = fit(900.0, 700.0, Some(10.0), true) else {
            panic!("expected overlay")
        };
        assert_eq!(panel_w, PANEL_MIN);
    }

    #[test]
    fn tiny_stage_still_yields_the_minimum() {
        let Layout::Overlay { panel_w } = fit(300.0, 300.0, None, true) else {
            panic!("expected overlay")
        };
        assert_eq!(panel_w, PANEL_MIN);
    }
}
