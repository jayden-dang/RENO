//! Theme tokens: pure data plus small colour helpers, no rendering.
//!
//! The window is opaque and two-tone. `shell` is the window, rail and sidebar background;
//! `canvas` is the main content panel, an inset rounded panel with a 1px hairline. Raised
//! surfaces (composer, cards) use `raised` or `card` with a hairline. There are no
//! alpha-over-desktop tokens: no window tint, no glass, no translucent surface.
//!
//! Palette values are the token table of the app-shell brief (docs/design/2026-11-19-app-shell-ui-brief.md,
//! `reno_dark`, `reno_light`); it replaced the deleted brief's palette.

// Tokens are consumed screen by screen; the unreached ones are listed in the brief.
#![allow(dead_code)]

use gpui::{App, Global, Hsla, WindowAppearance, rgb};

use super::motion::mix;
use crate::prefs::{AccentName, ThemeName};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Appearance {
    Light,
    Dark,
}

impl Appearance {
    pub(crate) fn is_dark(self) -> bool {
        self == Self::Dark
    }

    pub(crate) fn toggled(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }
}

impl From<WindowAppearance> for Appearance {
    fn from(value: WindowAppearance) -> Self {
        match value {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Dark,
        }
    }
}

/// Hairline strength: the text colour at 9% (the brief allows 9 to 10%).
pub(crate) const HAIRLINE_ALPHA: f32 = 0.09;

/// The ink on a light fill: the dark theme's `shell`.
const DARK_INK: u32 = 0x080a0c;

/// A palette as `0xRRGGBB` values, so the data is checkable without a renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Palette {
    /// The main content panel (the old `bg`).
    pub(crate) canvas: u32,
    /// The window, rail and sidebar background.
    pub(crate) shell: u32,
    /// Selected rows and pressed controls.
    pub(crate) raised: u32,
    pub(crate) card: u32,
    /// Solid raised surfaces on the canvas: the composer, approval cards, tables, code blocks
    /// (UI brief amendment 6).
    pub(crate) surface: u32,
    pub(crate) text: u32,
    pub(crate) muted: u32,
    pub(crate) faint: u32,
    pub(crate) accent: u32,
    pub(crate) danger: u32,
    pub(crate) warning: u32,
    pub(crate) success: u32,
    /// The terminal's 16 ANSI colours: black, red, green, yellow, blue, magenta, cyan, white,
    /// then their bright forms. Red, green and yellow are the danger, success and warning
    /// tokens, so a terminal and the rest of the app say the same colours.
    pub(crate) ansi: [u32; 16],
}

/// The dark palette.
pub(crate) const fn reno_dark() -> Palette {
    Palette {
        shell: 0x080a0c,
        canvas: 0x0b0e11,
        card: 0x0e1216,
        surface: 0x141a1f,
        raised: 0x262c33,
        text: 0xdfe5ea,
        muted: 0x98a3ad,
        faint: 0x6f7a85,
        accent: 0x2dd4bf,
        warning: 0xfbbf24,
        danger: 0xf87171,
        success: 0x34d399,
        ansi: [
            0x3b3b41, 0xf87171, 0x34d399, 0xfbbf24, 0x7aa2f7, 0xc084fc, 0x22d3ee, 0xd4d4d8,
            0x71717a, 0xfca5a5, 0x6ee7b7, 0xfde047, 0x93c5fd, 0xd8b4fe, 0x67e8f9, 0xfafafa,
        ],
    }
}

/// The light palette.
pub(crate) const fn reno_light() -> Palette {
    Palette {
        shell: 0xe6edef,
        canvas: 0xf4f7f8,
        card: 0xffffff,
        surface: 0xffffff,
        raised: 0xd8e1e4,
        text: 0x16232b,
        muted: 0x4b5b66,
        faint: 0x6b7b86,
        accent: 0x0f766e,
        warning: 0xb45309,
        danger: 0xdc2626,
        success: 0x15803d,
        ansi: [
            0x303035, 0xdc2626, 0x15803d, 0xb45309, 0x1d4ed8, 0x7e22ce, 0x0e7490, 0x62626a,
            0x52525b, 0xb91c1c, 0x166534, 0x854d0e, 0x1e40af, 0x6b21a8, 0x155e75, 0x3f3f46,
        ],
    }
}

/// The palette for a theme in an appearance with an accent (Settings › Appearance; values from
/// docs/design/2026-11-19-settings-pages-ui-brief.md). Reno with Teal is today's palette. A theme
/// changes the neutrals only: the semantic colours and the terminal's set are shared.
pub(crate) const fn palette_for(
    theme: ThemeName,
    appearance: Appearance,
    accent: AccentName,
) -> Palette {
    let base = Palette::for_appearance(appearance);
    let mut p = match (theme, appearance) {
        (ThemeName::Reno, _) => base,
        (ThemeName::Slate, Appearance::Dark) => Palette {
            shell: 0x0a0d14,
            canvas: 0x0e121b,
            card: 0x121828,
            surface: 0x172033,
            raised: 0x283247,
            text: 0xe2e8f0,
            muted: 0x94a3b8,
            faint: 0x6f7d93,
            ..base
        },
        (ThemeName::Slate, Appearance::Light) => Palette {
            shell: 0xe2e8f0,
            canvas: 0xf1f5f9,
            card: 0xffffff,
            surface: 0xffffff,
            raised: 0xd3dbe6,
            text: 0x0f172a,
            muted: 0x475569,
            faint: 0x64748b,
            ..base
        },
    };
    p.accent = match (accent, appearance) {
        (AccentName::Teal, Appearance::Dark) => 0x2dd4bf,
        (AccentName::Teal, Appearance::Light) => 0x0f766e,
        (AccentName::Blue, Appearance::Dark) => 0x60a5fa,
        (AccentName::Blue, Appearance::Light) => 0x1d4ed8,
        (AccentName::Violet, Appearance::Dark) => 0xa78bfa,
        (AccentName::Violet, Appearance::Light) => 0x6d28d9,
    };
    p
}

impl Palette {
    pub(crate) const fn for_appearance(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Dark => reno_dark(),
            Appearance::Light => reno_light(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    pub(crate) appearance: Appearance,
    pub(crate) canvas: Hsla,
    pub(crate) shell: Hsla,
    pub(crate) raised: Hsla,
    pub(crate) card: Hsla,
    pub(crate) surface: Hsla,
    pub(crate) text: Hsla,
    pub(crate) muted: Hsla,
    pub(crate) faint: Hsla,
    pub(crate) accent: Hsla,
    pub(crate) danger: Hsla,
    pub(crate) warning: Hsla,
    pub(crate) success: Hsla,
    /// Hairline: text at [`HAIRLINE_ALPHA`].
    pub(crate) hairline: Hsla,
    /// The terminal's ANSI-16 colours, from [`Palette::ansi`].
    pub(crate) ansi: [Hsla; 16],
}

impl Theme {
    pub(crate) fn for_appearance(appearance: Appearance) -> Self {
        Self::from_palette(appearance, &Palette::for_appearance(appearance))
    }

    pub(crate) fn from_palette(appearance: Appearance, palette: &Palette) -> Self {
        let text: Hsla = rgb(palette.text).into();
        Self {
            appearance,
            canvas: rgb(palette.canvas).into(),
            shell: rgb(palette.shell).into(),
            raised: rgb(palette.raised).into(),
            card: rgb(palette.card).into(),
            surface: rgb(palette.surface).into(),
            text,
            muted: rgb(palette.muted).into(),
            faint: rgb(palette.faint).into(),
            accent: rgb(palette.accent).into(),
            danger: rgb(palette.danger).into(),
            warning: rgb(palette.warning).into(),
            success: rgb(palette.success).into(),
            hairline: text.opacity(HAIRLINE_ALPHA),
            ansi: palette.ansi.map(|c| rgb(c).into()),
        }
    }

    /// The text colour for something drawn on `fill` (an accent or semantic fill): whichever of
    /// the shell's dark ink and white contrasts more, never white on light teal.
    pub(crate) fn on_fill(&self, fill: Hsla) -> Hsla {
        use crate::vm::terminal::contrast_ratio;
        let dark: Hsla = rgb(DARK_INK).into();
        let white: Hsla = rgb(0xffffff).into();
        if contrast_ratio(dark, fill) >= contrast_ratio(white, fill) {
            dark
        } else {
            white
        }
    }

    /// The selected sidebar row: `raised` pulled halfway toward `shell`, still opaque.
    pub(crate) fn row_selected(&self) -> Hsla {
        mix(self.shell, self.raised, 0.45)
    }

    /// A semantic colour pulled toward the text colour, for message copy on a tinted wash of
    /// that colour (the role the source's `*_muted` tokens played).
    pub(crate) fn soften(&self, tone: Hsla) -> Hsla {
        mix(tone, self.text, 0.35)
    }
}

/// The theme in force, set by the root view each frame so child views read it without
/// threading it through every constructor.
#[derive(Clone, Copy, Debug)]
struct CurrentTheme(Theme);

impl Global for CurrentTheme {}

/// Publishes `theme` for [`current`].
pub(crate) fn set_current(theme: Theme, cx: &mut App) {
    cx.set_global(CurrentTheme(theme));
}

/// The theme in force; dark before the root has published one.
pub(crate) fn current(cx: &App) -> Theme {
    cx.try_global::<CurrentTheme>()
        .map_or_else(|| Theme::for_appearance(Appearance::Dark), |t| t.0)
}

#[cfg(test)]
mod tests {
    use gpui::Rgba;

    use super::*;

    fn hex(color: Hsla) -> u32 {
        let c = Rgba::from(color.opacity(1.0));
        let byte = |v: f32| (v * 255.0).round() as u32;
        (byte(c.r) << 16) | (byte(c.g) << 8) | byte(c.b)
    }

    #[test]
    fn palettes_match_the_brief() {
        let d = reno_dark();
        // The app-shell brief's token table (docs/design/2026-11-19-app-shell-ui-brief.md).
        assert_eq!(
            [d.shell, d.canvas, d.card, d.surface, d.raised, d.text],
            [0x080a0c, 0x0b0e11, 0x0e1216, 0x141a1f, 0x262c33, 0xdfe5ea]
        );
        assert_eq!(
            [d.muted, d.faint, d.accent, d.warning, d.danger, d.success],
            [0x98a3ad, 0x6f7a85, 0x2dd4bf, 0xfbbf24, 0xf87171, 0x34d399]
        );
        let l = reno_light();
        assert_eq!(
            [l.shell, l.canvas, l.card, l.surface, l.raised, l.text],
            [0xe6edef, 0xf4f7f8, 0xffffff, 0xffffff, 0xd8e1e4, 0x16232b]
        );
        assert_eq!(
            [l.muted, l.faint, l.accent, l.warning, l.danger, l.success],
            [0x4b5b66, 0x6b7b86, 0x0f766e, 0xb45309, 0xdc2626, 0x15803d]
        );
    }

    #[test]
    fn text_on_a_fill_is_picked_by_contrast_never_white_on_light_teal() {
        use crate::vm::terminal::contrast_ratio;
        for appearance in [Appearance::Dark, Appearance::Light] {
            let theme = Theme::for_appearance(appearance);
            for fill in [theme.accent, theme.warning, theme.danger, theme.success] {
                let ink = theme.on_fill(fill);
                let other = if hex(ink) == 0xffffff {
                    rgb(0x080a0c).into()
                } else {
                    rgb(0xffffff).into()
                };
                assert!(
                    contrast_ratio(ink, fill) >= contrast_ratio(other, fill),
                    "{appearance:?} {:#08x}",
                    hex(fill)
                );
                assert!(
                    contrast_ratio(ink, fill) >= 4.5,
                    "readable on {:#08x}",
                    hex(fill)
                );
            }
        }
        // The dark theme's teal is light: its text is dark, not white.
        let dark = Theme::for_appearance(Appearance::Dark);
        assert_eq!(hex(dark.on_fill(dark.accent)), 0x080a0c);
        // The light theme's teal is deep: its text is white.
        let light = Theme::for_appearance(Appearance::Light);
        assert_eq!(hex(light.on_fill(light.accent)), 0xffffff);
    }

    #[test]
    fn theme_colours_round_trip_the_palette() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let palette = Palette::for_appearance(appearance);
            let theme = Theme::for_appearance(appearance);
            assert_eq!(hex(theme.canvas), palette.canvas);
            assert_eq!(hex(theme.shell), palette.shell);
            assert_eq!(hex(theme.accent), palette.accent);
            assert_eq!(hex(theme.danger), palette.danger);
            assert_eq!(theme.canvas.a, 1.0, "tokens are opaque");
            assert_eq!(theme.card.a, 1.0, "tokens are opaque");
        }
    }

    #[test]
    fn the_ansi_palette_shares_the_semantic_colours_in_both_appearances() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let p = Palette::for_appearance(appearance);
            assert_eq!(p.ansi[1], p.danger, "red is danger ({appearance:?})");
            assert_eq!(p.ansi[2], p.success, "green is success ({appearance:?})");
            assert_eq!(p.ansi[3], p.warning, "yellow is warning ({appearance:?})");
            let theme = Theme::for_appearance(appearance);
            for (i, c) in theme.ansi.iter().enumerate() {
                assert_eq!(hex(*c), p.ansi[i]);
                assert_eq!(c.a, 1.0);
            }
        }
    }

    #[test]
    fn reno_with_teal_is_todays_palette() {
        for a in [Appearance::Dark, Appearance::Light] {
            assert_eq!(
                palette_for(ThemeName::Reno, a, AccentName::Teal),
                Palette::for_appearance(a)
            );
        }
    }

    #[test]
    fn slate_and_the_accents_match_the_settings_pages_brief() {
        // docs/design/2026-11-19-settings-pages-ui-brief.md, "Themes and accents".
        let d = palette_for(ThemeName::Slate, Appearance::Dark, AccentName::Teal);
        assert_eq!(
            [
                d.shell, d.canvas, d.card, d.surface, d.raised, d.text, d.muted, d.faint
            ],
            [
                0x0a0d14, 0x0e121b, 0x121828, 0x172033, 0x283247, 0xe2e8f0, 0x94a3b8, 0x6f7d93
            ]
        );
        let l = palette_for(ThemeName::Slate, Appearance::Light, AccentName::Teal);
        assert_eq!(
            [
                l.shell, l.canvas, l.card, l.surface, l.raised, l.text, l.muted, l.faint
            ],
            [
                0xe2e8f0, 0xf1f5f9, 0xffffff, 0xffffff, 0xd3dbe6, 0x0f172a, 0x475569, 0x64748b
            ]
        );
        assert_eq!(d.danger, reno_dark().danger, "semantic colours are shared");
        assert_eq!(d.ansi, reno_dark().ansi, "so is the terminal's set");
        let accent = |a, t| palette_for(ThemeName::Reno, a, t).accent;
        assert_eq!(accent(Appearance::Dark, AccentName::Blue), 0x60a5fa);
        assert_eq!(accent(Appearance::Light, AccentName::Blue), 0x1d4ed8);
        assert_eq!(accent(Appearance::Dark, AccentName::Violet), 0xa78bfa);
        assert_eq!(accent(Appearance::Light, AccentName::Violet), 0x6d28d9);
    }

    #[test]
    fn every_theme_and_accent_reads_in_both_appearances() {
        use crate::vm::terminal::contrast_ratio;
        for name in [ThemeName::Reno, ThemeName::Slate] {
            for a in [Appearance::Dark, Appearance::Light] {
                for accent in [AccentName::Teal, AccentName::Blue, AccentName::Violet] {
                    let t = Theme::from_palette(a, &palette_for(name, a, accent));
                    let at = |fg, bg| contrast_ratio(fg, bg);
                    assert!(at(t.text, t.canvas) >= 7.0, "{name:?} {a:?} text");
                    assert!(at(t.muted, t.canvas) >= 4.5, "{name:?} {a:?} muted");
                    assert!(at(t.faint, t.canvas) >= 4.0, "{name:?} {a:?} faint");
                    assert!(at(t.accent, t.surface) >= 4.5, "{name:?} {a:?} {accent:?}");
                }
            }
        }
    }

    #[test]
    fn the_hues_read_on_the_canvas_in_both_appearances() {
        // Red, green, yellow, blue, magenta, cyan, and their bright forms, against the canvas
        // the terminal sits on: what a program colours must stay legible without the
        // contrast floor doing the work.
        for appearance in [Appearance::Dark, Appearance::Light] {
            let theme = Theme::for_appearance(appearance);
            for i in [1, 2, 3, 4, 5, 6, 9, 10, 11, 12, 13, 14] {
                let ratio = crate::vm::terminal::contrast_ratio(theme.ansi[i], theme.canvas);
                assert!(ratio >= 4.0, "{appearance:?} ansi {i}: {ratio}");
            }
        }
    }

    #[test]
    fn hairline_is_text_at_nine_percent() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let theme = Theme::for_appearance(appearance);
            assert!((theme.hairline.a - 0.09).abs() < 1e-6);
            assert!((0.09..=0.10).contains(&HAIRLINE_ALPHA));
            assert_eq!(hex(theme.hairline), hex(theme.text));
        }
    }

    #[test]
    fn soften_stays_opaque_and_moves_toward_text() {
        let theme = Theme::for_appearance(Appearance::Dark);
        let soft = theme.soften(theme.danger);
        assert_eq!(soft.a, 1.0);
        let (d, s, t) = (
            gpui::Rgba::from(theme.danger),
            gpui::Rgba::from(soft),
            gpui::Rgba::from(theme.text),
        );
        assert!(
            s.g > d.g && s.g < t.g,
            "green channel moves toward the text"
        );
    }

    #[test]
    fn window_appearance_maps_both_ways() {
        assert!(Appearance::from(WindowAppearance::VibrantDark).is_dark());
        assert!(!Appearance::from(WindowAppearance::VibrantLight).is_dark());
    }
}
