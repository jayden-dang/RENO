# ROAD-54 replace-mocks-with-real-contracts — notes

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
