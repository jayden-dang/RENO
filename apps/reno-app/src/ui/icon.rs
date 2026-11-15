//! SVG icons: Reno's own 16x16, 1.4 px-stroke drawings, bundled in `assets/icons` and served to
//! GPUI through [`Assets`]. Draw one with [`icon`]; it takes its colour from `text_color`.

use std::borrow::Cow;

use gpui::{AssetSource, Hsla, Result, SharedString, Svg, prelude::*, px, svg};

macro_rules! icons {
    ($($variant:ident => $file:literal,)*) => {
        /// Every bundled icon.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub(crate) enum Icon {
            $($variant,)*
        }

        impl Icon {
            /// Every icon, for the asset test.
            #[cfg(test)]
            pub(crate) const ALL: &[Icon] = &[$(Icon::$variant,)*];

            /// The asset path GPUI loads it from.
            pub(crate) const fn path(self) -> &'static str {
                match self {
                    $(Icon::$variant => concat!("icons/", $file, ".svg"),)*
                }
            }

            const fn bytes(self) -> &'static [u8] {
                match self {
                    $(Icon::$variant => include_bytes!(concat!("../../assets/icons/", $file, ".svg")),)*
                }
            }
        }

        const ASSET_PATHS: &[(&str, Icon)] =
            &[$((concat!("icons/", $file, ".svg"), Icon::$variant),)*];
    };
}

icons! {
    Back => "back",
    Board => "board",
    Bolt => "bolt",
    Bot => "bot",
    Branch => "branch",
    Check => "check",
    ChevDown => "chev-down",
    ChevUp => "chev-up",
    Clip => "clip",
    Clock => "clock",
    Cpu => "cpu",
    Eval => "eval",
    Eye => "eye",
    File => "file",
    Flow => "flow",
    Folder => "folder",
    Fwd => "fwd",
    Globe => "globe",
    History => "history",
    Home => "home",
    Inbox => "inbox",
    Link => "link",
    List => "list",
    Lock => "lock",
    Merge => "merge",
    More => "more",
    Msg => "msg",
    Pause => "pause",
    Pin => "pin",
    Play => "play",
    Plus => "plus",
    Question => "question",
    Route => "route",
    Search => "search",
    Settings => "settings",
    Shield => "shield",
    Sidebar => "sidebar",
    Sliders => "sliders",
    Spark => "spark",
    Stop => "stop",
    Team => "team",
    Terminal => "terminal",
    Up => "up",
    Wand => "wand",
    X => "x",
    XClaude => "x-claude",
    XCodex => "x-codex",
    XGrok => "x-grok",
    XOpencode => "x-opencode",
}

/// The app's `AssetSource`: the icon files, compiled in.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ASSET_PATHS
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, icon)| Cow::Borrowed(icon.bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ASSET_PATHS
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| SharedString::from(*p))
            .collect())
    }
}

/// A square icon of `size` px in `color`.
pub(crate) fn icon(icon: Icon, size: f32, color: Hsla) -> Svg {
    svg()
        .path(icon.path())
        .flex_none()
        .size(px(size))
        .text_color(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_served_and_is_a_16px_drawing() {
        for &icon in Icon::ALL {
            let bytes = Assets
                .load(icon.path())
                .unwrap_or_default()
                .unwrap_or_else(|| panic!("{} is not served", icon.path()));
            let text = std::str::from_utf8(&bytes).unwrap_or_default();
            assert!(text.contains("viewBox=\"0 0 16 16\""), "{}", icon.path());
            assert!(text.contains("currentColor"), "{}", icon.path());
        }
        assert_eq!(Assets.load("icons/nope.svg").unwrap_or_default(), None);
        assert_eq!(
            Assets.list("icons/").unwrap_or_default().len(),
            Icon::ALL.len()
        );
    }
}
