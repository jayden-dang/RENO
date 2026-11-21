# ROAD-27 terminal-program-status — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Zed has no working / needs-input detection. It has a `Bell` event and reads the foreground process name and argv (`crates/terminal/src/terminal.rs` ~667, `foreground_process_command_name` ~2968, `pty_info.rs`, GPL).
- Reno builds its own heuristic: output quiet for N ms, alt-screen flag, foreground process → `crates/reno-term/`, `apps/renod/`.
