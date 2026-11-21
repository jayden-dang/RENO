# ROAD-37 settings-and-design-system — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Semantic `Color` enum (Default, Muted, Accent, Error, Warning, Success, … `Custom(Hsla)`) resolved through the theme at render time; components store `Color`, not `Hsla`: `crates/ui/src/styles/color.rs` (GPL) → `apps/reno-app/src/ui/theme.rs`, `ui/kit.rs`.
- Sizes and styles as enums with `rems()` plus a `Custom(..)` escape: `ButtonSize`, `ButtonStyle`, `TintColor` (`crates/ui/src/components/button/button_like.rs` ~125, ~454), `IconSize`, `LabelSize` (GPL).
- Components as `#[derive(IntoElement)]` structs with `impl RenderOnce`, builder setters, `id: impl Into<ElementId>` first; small shared traits `Clickable`, `Disableable`, `Toggleable` (`ToggleState`), `FixedWidth` in `crates/ui/src/traits/`; a low-level "Like" base (`ButtonLike`, `LabelLike`) (GPL).
- Status triple per severity (`x`, `x_background`, `x_border`; missing background = foreground at 25%): `crates/theme/src/styles/status.rs`, `fallback_themes.rs` (GPL).
- Elevation levels 1/2/3 (panel / popover-palette / modal) bundling background, border and layered shadow: `crates/ui/src/styles/elevation.rs`, `traits/styled_ext.rs` (GPL).
- State ladder tokens `element_*` and `ghost_element_*` (background, hover, active, selected, disabled): `crates/theme/src/styles/colors.rs` (GPL).
- Theme JSON as a partial over built-in defaults via `Refineable` (`crates/refineable`, Apache); user overrides merged the same way (`crates/theme_settings`, GPL).
- Gallery: `crates/component` + `crates/component_preview` (GPL), components self-register with scope and status, doc comments become gallery text. A plain list behind `reno-app --gallery` is enough here.
- Live element inspector for debug builds: `crates/gpui/src/inspector.rs` (Apache, present at the pinned rev) + `crates/inspector_ui` (GPL), `cmd-alt-i`. Check which gpui feature it needs.
- Icon rules: `crates/icons/README.md` (16x16 viewBox, 1.2px stroke, snake_case file, PascalCase variant).
- Settings: one content tree with every field `Option<T>`, a documented defaults file, layered resolution, schema from `schemars`, append-only dated migrations (`crates/settings_content`, `crates/settings/src/settings_store.rs` ~1370, `crates/migrator`, GPL) → `apps/reno-app/src/prefs.rs`.
- Doing ROAD-55's split first avoids moving the design-system code twice.
