//! Theme tokens: pure data plus small colour helpers, no rendering.
//!
//! The window is opaque and two-tone. `shell` is the window, rail and sidebar background;
//! `canvas` is the main content panel, an inset rounded panel with a 1px hairline. Raised
//! surfaces (composer, cards) use `raised` or `card` with a hairline. There are no
//! alpha-over-desktop tokens: no window tint, no glass, no translucent surface.
//!
//! Palette values are the locked brief's Grounding values (`reno_dark`, `reno_light`).

// Tokens are consumed screen by screen; the unreached ones are listed in the brief.
#![allow(dead_code)]

use gpui::{Hsla, WindowAppearance, rgb};

use super::motion::mix;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Appearance {
    Light,
    Dark,
}

impl Appearance {
    pub(crate) fn is_dark(self) -> bool {
        self == Self::Dark
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
}

/// The dark palette.
pub(crate) const fn reno_dark() -> Palette {
    Palette {
        canvas: 0x060606,
        shell: 0x0d0d0d,
        raised: 0x343438,
        card: 0x0e0e0e,
        surface: 0x161618,
        text: 0xe8e8ea,
        muted: 0xa9a9ae,
        faint: 0x85858a,
        accent: 0x8b7cf6,
        danger: 0xf87171,
        warning: 0xfacc15,
        success: 0x34d399,
    }
}

/// The light palette. The brief lists no light `card`; it is `#ffffff`, the value the source
/// palette uses.
pub(crate) const fn reno_light() -> Palette {
    Palette {
        canvas: 0xffffff,
        shell: 0xf3f3f5,
        raised: 0xededf0,
        card: 0xffffff,
        surface: 0xffffff,
        text: 0x303035,
        muted: 0x62626a,
        faint: 0x797981,
        accent: 0x5b43e8,
        danger: 0xdc2626,
        warning: 0xa16207,
        success: 0x15803d,
    }
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
        }
    }

    /// A semantic colour pulled toward the text colour, for message copy on a tinted wash of
    /// that colour (the role the source's `*_muted` tokens played).
    pub(crate) fn soften(&self, tone: Hsla) -> Hsla {
        mix(tone, self.text, 0.35)
    }
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
        assert_eq!(
            [d.canvas, d.shell, d.raised, d.card, d.text, d.muted],
            [0x060606, 0x0d0d0d, 0x343438, 0x0e0e0e, 0xe8e8ea, 0xa9a9ae]
        );
        assert_eq!(
            [d.faint, d.accent, d.danger, d.warning, d.success],
            [0x85858a, 0x8b7cf6, 0xf87171, 0xfacc15, 0x34d399]
        );
        let l = reno_light();
        assert_eq!(
            [l.canvas, l.shell, l.raised, l.card, l.text, l.muted],
            [0xffffff, 0xf3f3f5, 0xededf0, 0xffffff, 0x303035, 0x62626a]
        );
        assert_eq!(
            [l.faint, l.accent, l.danger, l.warning, l.success],
            [0x797981, 0x5b43e8, 0xdc2626, 0xa16207, 0x15803d]
        );
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
