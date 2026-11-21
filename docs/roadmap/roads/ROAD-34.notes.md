# ROAD-34 palette-and-keymap — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Actions as namespaced types (`actions!`, `#[derive(Action)]`) registered in an inventory; the palette and the keymap JSON schema are both generated from it (`crates/gpui_macros/src/derive_action.rs`, Apache) → `apps/reno-app/src/actions.rs`.
- Keymap files: `assets/keymaps/default-macos.json`, format `[{ "context": "Editor && !menu", "bindings": {..} }]`; layering default → base → user, each binding tagged with `KeybindSource`; the user file is watched and reloaded (`crates/zed/src/zed.rs` `reload_keymaps` ~2340, GPL).
- A feature hides its own actions from the palette: `crates/command_palette_hooks/src/lib.rs` `CommandPaletteFilter` (GPL).
- Matching: `nucleo` with background matching split per CPU and an `AtomicBool` cancel (`crates/fuzzy_nucleo`, `crates/fuzzy/src/strings.rs:117-193`, GPL); `PickerDelegate` with "a new query drops the old task" and a ~4 ms synchronous finalize on first open to avoid an empty flash (`crates/picker/src/picker.rs` ~164, ~237, ~1226, GPL). `nucleo-matcher` itself is MPL-2.0 (in `deny.toml`'s allow-list; confirm with `cargo deny`).
- Reno today: `apps/reno-app/src/ui/palette.rs:72` filters by substring on the main thread.
- Docs can render keybindings from the real registry and fail the build on unknown action names: `crates/docs_preprocessor/src/main.rs` ~261-378 (GPL).
