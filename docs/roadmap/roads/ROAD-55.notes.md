# ROAD-55 app-crate-split — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

Owner decision 2026-11-21: split `apps/reno-app` (~85k lines, one crate).

- Zed keeps UI-free model crates (`project`, `git`, `acp_thread`) apart from `*_ui` crates and tests forbidden edges that would serialize the build: `tooling/xtask/src/workspace.rs` `FORBIDDEN_DEPENDENCIES` ~40.
- Proposed cut: `reno-ui` (theme, clean kit, gallery; never reno-client or reno-proto), `reno-vm` (`vm/`, `summary.rs`, `transcript.rs`, `nav.rs`; no gpui), `reno-app` (app, store, screens, panel, feature widgets) composes both.
- Seams today: `vm/` is GPUI-free in 43 of 44 files (only `vm/terminal.rs`); clean kit files: field, icon, markdown_view, menu, motion, notice, overlay, running_pill, text_area, text_model, toast.
- Cost: `ui` and `vm` depend on each other today. Kit signatures that take vm or proto types (`Banner`, `Section`, `Level`, `Problem`, `RunStatus`) move to `reno-ui` types and the app maps them; `vm` stops naming `ui::icon` and `ui::theme`; `contrast_ratio` moves to `reno-ui`.
- Feature widgets in `ui/` that reach `Store`/`RootView`/`Nav` (composer, terminal_view, palette, sidebar, bar, restart, setup) belong to the app.
- ARCH-3 keeps its words; `docs/architecture/system.md` adds "or a dependency it must never have" to what counts as a boundary (precedent: reno-kernel, ARCH-7). `xtask check` gains both edges.
- Waits for `feat/app-bench` (ROAD-29) and the ROAD-32 sidebar branch to land; measure an incremental `cargo build -p reno-app` and `cargo test -p reno-app` before and after.
