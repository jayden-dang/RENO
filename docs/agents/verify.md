# Verify / control — cold-start drive recipe

Written by `configure-repo` (Decision M). Read by `validate-feature` and `write-dogfood`. A later
agent that has never seen this app follows this file instead of inventing commands.

**App:** Reno (daemon `renod`, CLI `reno`, GPUI app `reno-app`)
**Primary surface:** CLI + daemon
**Other surfaces:** desktop GUI (`Reno.app`) — owner-driven, not part of this recipe

**Hard rules.** Drive only an isolated daemon you start yourself. Never `reno daemon start|stop|restart|install|uninstall`
(they reach launchd and the owner's live daemon), never `launchctl`, never the Keychain (`reno secret`), and never a path that
opens a real browser. Approvals are decided by the owner, not by an agent: you may create a pending approval and read it, not
approve or deny it.

## Launch

Start command (must match `docs/agents/project.md` **Run locally (dev)**):

```
cargo build -p reno -p renod
D=$(mktemp -d /tmp/reno-v.XXXX)
target/debug/renod --data-dir $D --socket $D/reno.sock --defs-dir $D/defs & echo $! > $D/pid
```

Ready when: `target/debug/reno --data-dir $D daemon status` exits 0.
Teardown: `kill $(cat $D/pid)` — the PID this Launch started, never by process name.

## Doctor

One read-only check: is this instance worth driving?

```
target/debug/reno --data-dir $D daemon status && target/debug/reno --data-dir $D executor list
```

Pass: status exits 0 and the list names the `mock` executor. Fail: do not Drive; fix Launch or record the blocker.

## Drive

Harness: PTY / plain CLI with `--json` output where offered (stable fields, not text layout).

Isolation: two instances can run side by side — each has its own `$D`, socket and defs dir. Never point a drive at the
default data directory.

## Evidence

Keep under `.skills/verify/` (gitignored). A proof captures **the action and the resulting state**, including a
**re-open / re-read** (restart the isolated daemon and read again) when the feature claims persistence. A final output
alone is not enough. Mocks only at a production boundary: the `mock` executor stands in for a paid model.

Cleanup never deletes evidence.

## Cleanup

Stop what Launch started (`kill $(cat $D/pid)`). Remove `$D`. Leave `.skills/verify/`.

## Features

Index:

| Feature | How a user reaches it | Observable end state |
|---|---|---|
| Run a task | `reno task run --executor mock "<prompt>"` | `reno run list` shows the run completed, and still does after a daemon restart |
| Pause / resume | `reno pause`, `reno resume`, `reno budget` | `reno budget` shows paused, and still does after a daemon restart; clear again with `reno resume` |
| Executors and definitions | `reno executor list`, `reno defs list`, `reno defs check --dir $D/defs` | The listings match the isolated defs dir; `defs check` exits 0 |

### Run a task

- **Sub-features:** `reno run list|follow|cancel`, `reno thread`, `reno fs`, `reno git`
- **How to get to it (user POV):** run `reno task run --executor mock "say hi"` against the daemon.
- **Driving it with CLI:** `reno --data-dir $D task run --executor mock --cwd $W --json "say hi"` (`W` = a throwaway `git init` repo with one empty commit); the first line is `{"runId": ...}`, then one `{"seq","event"}` per line. Exit 0 only if the run completed. Do not cut the stream short with `head`: read `run list` only after it ends, or the run still shows `running`.
- **Gotchas:** name `--executor mock` explicitly; without it the daemon tries Claude Code then Codex (paid, signed-in). Write runs create a git worktree for the `--cwd` — pass a throwaway git repo as `--cwd`, not this checkout.
- **Persist check:** kill the daemon, start it again with the same `$D`, then `reno --data-dir $D run list` still shows the run.

### Pause / resume

- **Sub-features:** per-bot pause, held triggers catching up on resume
- **How to get to it (user POV):** `reno pause`, later `reno resume`.
- **Driving it with CLI:** `reno --data-dir $D pause`, `reno --data-dir $D budget`, `reno --data-dir $D resume`.
- **Gotchas:** pause persists across daemon restarts, so always `resume` before Cleanup (or delete `$D`).
- **Persist check:** restart the daemon on the same `$D`; `budget` still reports paused.

### Executors and definitions

- **Sub-features:** `reno executor` look-again, `reno defs show`
- **How to get to it (user POV):** `reno executor list`, `reno defs list`.
- **Driving it with CLI:** `reno --data-dir $D executor list`, `reno --data-dir $D defs list`, `reno --data-dir $D defs check --dir $D/defs`.
- **Gotchas:** the defs dir seeds an example bot and the default policy on first start. `defs check` defaults to the owner's live `~/.reno` — always pass `--dir $D/defs`. `reno executor` reports the real CLIs installed on this machine; do not sign in or update them.
- **Persist check:** presentational — read-only listings, no store.
