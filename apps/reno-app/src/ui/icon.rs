//! SVG icons: Reno's own 16x16, 1.4 px-stroke drawings and the Phosphor (`Ph*`, regular weight,
//! 256x256, MIT, see `assets/PHOSPHOR-LICENSE.txt`) set, bundled in `assets/icons` and served to
//! GPUI through [`Assets`]. Draw one with [`icon`]; it takes its colour from `text_color`.
//!
//! The executor logos in `assets/logos` are coloured, so they are drawn with `img`, not tinted:
//! see [`logo_path`].

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
    Mark => "mark",
    Merge => "merge",
    More => "more",
    Msg => "msg",
    Pause => "pause",
    Pin => "pin",
    Play => "play",
    Plus => "plus",
    Question => "question",
    Ring => "ring",
    Route => "route",
    Search => "search",
    Settings => "settings",
    Shield => "shield",
    Bell => "bell",
    Server => "server",
    Sidebar => "sidebar",
    Sliders => "sliders",
    Spark => "spark",
    Star => "star",
    StarFill => "star-fill",
    Stop => "stop",
    Team => "team",
    Terminal => "terminal",
    Up => "up",
    Wand => "wand",
    X => "x",
    PhCaretRight => "ph-caret-right",
    PhCaretDown => "ph-caret-down",
    PhDownload => "ph-download-simple",
    PhRefresh => "ph-arrow-clockwise",
    PhWarning => "ph-warning",
    PhX => "ph-x",
    PhCheck => "ph-check",
    PhQuestion => "ph-question",
    PhTerminal => "ph-terminal-window",
    PhClock => "ph-clock",
    PhEye => "ph-eye",
    PhPlay => "ph-play",
    PhLock => "ph-lock-simple",
    PhCpu => "ph-cpu",
    PhLightning => "ph-lightning",
    PhFlask => "ph-flask",
    PhCube => "ph-cube",
    PhSpinner => "ph-circle-notch",
    PhHouse => "ph-house",
    PhTimer => "ph-timer",
    PhGear => "ph-gear",
    PhFolder => "ph-folder-simple",
}

/// The executor marks bundled in `assets/logos`: (asset path, bytes). The dark-ink marks have a
/// `-light` twin for dark themes.
const LOGOS: &[(&str, &[u8])] = &[
    (
        "logos/antigravity.svg",
        include_bytes!("../../assets/logos/antigravity.svg"),
    ),
    (
        "logos/claude-code.svg",
        include_bytes!("../../assets/logos/claude-code.svg"),
    ),
    (
        "logos/codex.svg",
        include_bytes!("../../assets/logos/codex.svg"),
    ),
    (
        "logos/codex-light.svg",
        include_bytes!("../../assets/logos/codex-light.svg"),
    ),
    (
        "logos/devin.svg",
        include_bytes!("../../assets/logos/devin.svg"),
    ),
    (
        "logos/devin-light.svg",
        include_bytes!("../../assets/logos/devin-light.svg"),
    ),
    (
        "logos/grok.svg",
        include_bytes!("../../assets/logos/grok.svg"),
    ),
    (
        "logos/grok-light.svg",
        include_bytes!("../../assets/logos/grok-light.svg"),
    ),
    (
        "logos/hermes.svg",
        include_bytes!("../../assets/logos/hermes.svg"),
    ),
    (
        "logos/hermes-light.svg",
        include_bytes!("../../assets/logos/hermes-light.svg"),
    ),
    (
        "logos/opencode.svg",
        include_bytes!("../../assets/logos/opencode.svg"),
    ),
    (
        "logos/opencode-light.svg",
        include_bytes!("../../assets/logos/opencode-light.svg"),
    ),
];

/// The asset path of the coloured logo of `executor_id`, for `dark` or light themes; `None` for
/// an executor with no logo (it gets a Phosphor glyph instead).
pub(crate) fn logo_path(executor_id: &str, dark: bool) -> Option<&'static str> {
    let (plain, light) = match executor_id {
        "claude-code" => ("logos/claude-code.svg", None),
        "codex" => ("logos/codex.svg", Some("logos/codex-light.svg")),
        "grok" | "acp.grok" => ("logos/grok.svg", Some("logos/grok-light.svg")),
        "acp.hermes" => ("logos/hermes.svg", Some("logos/hermes-light.svg")),
        "acp.devin" => ("logos/devin.svg", Some("logos/devin-light.svg")),
        "acp.antigravity" => ("logos/antigravity.svg", None),
        "opencode" | "acp.opencode" => ("logos/opencode.svg", Some("logos/opencode-light.svg")),
        _ => return None,
    };
    Some(if dark { light.unwrap_or(plain) } else { plain })
}

/// The app's `AssetSource`: the icon and logo files, compiled in.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = LOGOS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        Ok(ASSET_PATHS
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, icon)| Cow::Borrowed(icon.bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ASSET_PATHS
            .iter()
            .map(|(p, _)| *p)
            .chain(LOGOS.iter().map(|(p, _)| *p))
            .filter(|p| p.starts_with(path))
            .map(SharedString::from)
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
    fn every_icon_is_served_and_is_a_tintable_drawing() {
        for &icon in Icon::ALL {
            let bytes = Assets
                .load(icon.path())
                .unwrap_or_default()
                .unwrap_or_else(|| panic!("{} is not served", icon.path()));
            let text = std::str::from_utf8(&bytes).unwrap_or_default();
            let view_box = if icon.path().starts_with("icons/ph-") {
                "viewBox=\"0 0 256 256\""
            } else {
                "viewBox=\"0 0 16 16\""
            };
            assert!(text.contains(view_box), "{}", icon.path());
            assert!(text.contains("currentColor"), "{}", icon.path());
        }
        assert_eq!(Assets.load("icons/nope.svg").unwrap_or_default(), None);
        assert_eq!(
            Assets.list("icons/").unwrap_or_default().len(),
            Icon::ALL.len()
        );
    }

    #[test]
    fn logos_keep_their_colours_and_dark_ink_marks_have_a_light_twin() {
        for (path, _) in LOGOS {
            let bytes = Assets.load(path).unwrap_or_default().unwrap();
            let text = std::str::from_utf8(&bytes).unwrap_or_default();
            assert!(text.contains("<svg"), "{path}");
            assert!(!text.contains("currentColor"), "{path} must not be tinted");
        }
        for id in [
            "claude-code",
            "codex",
            "acp.grok",
            "acp.hermes",
            "acp.devin",
            "acp.antigravity",
        ] {
            for dark in [true, false] {
                let path = logo_path(id, dark).unwrap_or_else(|| panic!("{id}"));
                assert!(Assets.load(path).unwrap_or_default().is_some(), "{path}");
            }
        }
        assert_ne!(logo_path("codex", true), logo_path("codex", false));
        assert_eq!(
            logo_path("claude-code", true),
            logo_path("claude-code", false)
        );
        assert_eq!(logo_path("acp.fx", true), None);
        // Pi has no mark of its own here (none is bundled): it gets the generic glyph.
        assert_eq!(logo_path("pi", true), None);
        assert_eq!(logo_path("pi", false), None);
        // Nor has Muse Code.
        assert_eq!(logo_path("muse", true), None);
        assert_eq!(logo_path("muse", false), None);
        // Nor has Cursor.
        assert_eq!(logo_path("cursor", true), None);
        assert_eq!(logo_path("cursor", false), None);
        assert_eq!(logo_path("mock", false), None);
    }
}
