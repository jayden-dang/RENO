# ROAD-11 workspace-and-git-views — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- An async `GitRepository` trait that shells out to the `git` binary with porcelain `-z` output (no git2/gix); worktree add/remove/list/rename/repair are first-class: `crates/git/src/repository.rs` ~785-899, `git_status_args` ~3676 (GPL) → `crates/reno-harness/src/proc/git_workspace.rs`, `crates/reno-proto/src/workspace.rs`.
- A per-repo job queue with keys (`WriteIndex`, `RefreshStatuses`, …) so redundant queued jobs are skipped; above 64 dirty paths it stops path-scoped status: `crates/project/src/git_store.rs` ~132, ~1059, ~7018 (GPL).
- `.git` watched with 100 ms coalescing (`git_store.rs` ~1086); bare `.git` events deferred to avoid rescan loops (`crates/worktree/src/worktree.rs` ~5007).
- `worktree_created_at` uses `.git/worktrees/<name>` creation time to tell two worktrees at the same path apart.
- An in-memory fake of the whole trait for tests: `crates/fs/src/fake_git_repo.rs` (GPL).
