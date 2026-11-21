# ROAD-28 terminal-search-and-links — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- PTY events: the first is handled at once, then collected for 4 ms or until more than 100 arrive, `Wakeup`s collapsed into one: `crates/terminal/src/terminal.rs` `subscribe` ~1414-1475 (GPL) → `apps/reno-app/src/ui/terminal_view.rs`, `vm/terminal.rs` (check whether Reno batches already; only `terminal_view.rs` was checked).
- Scrollback 10,000 lines by default, 100,000 max (~884).
- Rendering batches adjacent same-style cells into `BatchedTextRun` and merges backgrounds into rects: `crates/terminal_view/src/terminal_element.rs` ~98, ~361, ~439.
- Links: a URL regex plus user path regexes with a timeout; hover lookups throttled to 5 px / 100 ms: `crates/terminal/src/alacritty/hyperlinks.rs` ~22-30, `terminal.rs` ~1623; path-like text resolved with many regression tests in `crates/terminal_view/src/terminal_path_like_target.rs`.
- Search on alacritty's `RegexSearch` (`terminal.rs` ~2065-2080).
