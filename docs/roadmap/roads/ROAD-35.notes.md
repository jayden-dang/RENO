# ROAD-35 panel-surfaces — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Hunk-level review of agent edits: per-file baseline plus a patch of unreviewed ranges, `keep_edits_in_range`, `reject_edits_in_ranges`, `keep_all`, `reject_all`, `undo_last_reject`: `crates/action_log/src/action_log.rs` ~640-923 (GPL) → `apps/reno-app/src/vm/review.rs`, `vm/changes.rs`. Zed tracks open buffers; Reno needs a git or filesystem baseline in the daemon.
- Edits by the user inside an agent hunk fold into the baseline (`ChangeAuthor`).
- Diffs computed on a background task with `imara_diff`, then published: `crates/buffer_diff/src/buffer_diff.rs` ~1214, ~2224 (GPL).
- Review UI: `crates/agent_ui/src/agent_diff.rs` with an undo-reject toast (GPL).
