# ROAD-54 replace-mocks-with-real-contracts — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- A connection status published to the UI: Connecting, Connected, ConnectionLost, Reconnecting, ReconnectionError{next_reconnection}, UpgradeRequired; backoff 500 ms doubling to 30 s: `crates/client/src/client.rs` ~87-89, ~278, ~695 (GPL) → `apps/reno-app/src/vm/daemon_restart.rs`, `crates/reno-client/src/mirror.rs`.
- A `version` subcommand and an exit code that means "server not running, spawn it" (`crates/remote/src/proxy.rs`, code 90).
- Every I/O boundary has a real implementation and a fake (`Fs`/`FakeFs`, `HttpClient`/`FakeHttpClient`, `Connection::in_memory`); `crates/http_client` is Apache.

## Mocked contracts the MILE-10 surfaces left behind (2026-11-21)

Each sits behind `reno-client` with a fake and a `Capabilities` flag that is false on the real
daemon. Their shapes are in the briefs' `## Contract` sections; the backend builds to them.

- `QuestionDesk` (questions inbox) — `docs/design/2026-11-19-app-shell-ui-brief.md`; ROAD-1 replaces it.
- `ActivityDesk` (plan, background tasks, subagents) — `docs/design/2026-11-19-run-activity-ui-brief.md`.
- `CompletionDesk` (harness commands, skills, project files), `MessageQueue` (queued messages),
  `PlaceDesk` (projects, branches, worktrees, clone; `task.run` needs base ref and checkout kind)
  — `docs/design/2026-11-20-composer-ui-brief.md`.
- `ThreadDesk` (`thread.update {title, pinned, archived}`, archived filter on `thread.list`) —
  `docs/design/2026-11-20-sidebar-ui-brief.md`.
- `DaemonDesk` (installed vs running renod, restart; CLI `reno daemon restart`) and
  `PlaceDesk.add_project` / `project_facts` — `docs/design/2026-11-20-first-run-ui-brief.md`.
