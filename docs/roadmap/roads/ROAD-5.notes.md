# ROAD-5 always-allow-into-harness-settings — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Turning a command into an allow pattern that understands sub-commands, so `cargo build && rm -rf /` cannot pass a `^cargo` rule: `crates/agent/src/pattern_extraction.rs` (brush-parser), options built in `ToolPermissionContext::build_permission_options` (GPL).
- Only the pattern-extraction idea applies: Zed's own `tool_permissions` policy layer is the opposite of the vision (the harness decides; Reno adds no policy).
