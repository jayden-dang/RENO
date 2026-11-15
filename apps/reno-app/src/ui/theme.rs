//! A minimal stand-in for the product theme that `glass.rs` was written against.
//! The real token set lands with T0.13a.

use gpui::{Hsla, Rgba, WindowAppearance, rgb};

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

#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    pub(crate) appearance: Appearance,
    pub(crate) accent_strong: Hsla,
    pub(crate) text: Hsla,
    pub(crate) muted: Hsla,
    /// Panel fill: the brief's `surface` (card at 38%).
    pub(crate) surface: Hsla,
    /// Hairline: text at 9%.
    pub(crate) line: Hsla,
}

impl Theme {
    pub(crate) fn for_appearance(appearance: Appearance) -> Self {
        // Token values come from the locked brief (dark and light palettes).
        let (text, muted, card, accent) = if appearance.is_dark() {
            (0xe8e8ea, 0xa9a9ae, 0x0e0e0e, 0x8b7cf6)
        } else {
            (0x303035, 0x62626a, 0xffffff, 0x5b43e8)
        };
        let text: Hsla = rgb(text).into();
        Self {
            appearance,
            accent_strong: rgb(accent).into(),
            text,
            muted: rgb(muted).into(),
            surface: Hsla::from(rgb(card)).opacity(0.38),
            line: text.opacity(0.09),
        }
    }
}

/// Linear blend of two colours in sRGB, alpha included.
pub(crate) fn mix(from: Hsla, to: Hsla, amount: f32) -> Hsla {
    let (a, b) = (Rgba::from(from), Rgba::from(to));
    let lerp = |x: f32, y: f32| x + (y - x) * amount;
    Rgba {
        r: lerp(a.r, b.r),
        g: lerp(a.g, b.g),
        b: lerp(a.b, b.b),
        a: lerp(a.a, b.a),
    }
    .into()
}
