# Architecture domain: Runtime

Status: Approved
Date: 2026-11-19

## Purpose and boundary

How Reno runs: its processes, how the kernel reaches the world, which clock each deadline is on,
and how queues, tasks and executors are bounded. It explains ARCH-7, ARCH-10 to ARCH-12 and
ARCH-15 from `docs/architecture/INDEX.md`. The crate graph is in `system.md`.

## Topology

| Process / service | Role |
|---|---|
| `renod` | The daemon. Installed, it runs under a launchd LaunchAgent (`reno daemon install`) that starts it at login and restarts it after a crash; `reno daemon start` also starts it directly. Owns the store, the kernel and the socket |
| `reno` | The command line; a short-lived client of the socket. Also started by the daemon as `reno mcp`, the MCP server one run talks to |
| `reno-app` | The desktop app (`Reno.app`); a long-lived client of the socket |
| Agent CLIs | Child processes `renod` launches per run (Claude Code, Codex, Pi, Muse, opencode, ACP agents; Cursor through a Node shim over its SDK) |

## Environments

| Environment | Notes |
|---|---|
| Local macOS | The only environment. Nothing is deployed; each user runs their own daemon |

## Kernel ports

The kernel does no I/O of its own (ARCH-7). It reaches the world only through `reno-store` and its
ports:

- `Clock` — time;
- `Executor` — harness runs;
- `Workspace` — worktrees, check commands, git refs;
- `Watch` — file and ref changes;
- `WakeSignal` — the system woke from sleep (a handle the daemon raises; the kernel only listens);
- `Definitions` — the definitions repository;
- `Secrets` — the Keychain;
- `RunLink` — a run's token file and the `reno mcp` server that reads it;
- `CommandRunner` and `SelfUpdateControl` — the commands of executor installs and updates, and the
  switch of a CLI's own updater (`MaintenancePorts`). The commands are compiled into the adapters
  and run outside the policy engine: a person asked for them.

The ports are implemented outside the kernel and faked in tests. The kernel never reads the system
clock, spawns a process, or opens a file itself.

## Timers and clocks

Nothing polls (ARCH-10). Every timer is a deadline: the next trigger, the next lease expiry, a live
run's heartbeat, a checkpoint after the last write. Changes to files and git refs arrive as events.

The one thing a deadline cannot see is the Mac sleeping: the monotonic timer stops, the wall clock
does not. The daemon turns the system's wake notification into a signal (`WakeSignal`) that the
scheduler, the dispatcher and every run's supervisor hear, and each then reads the clock again.
There is no periodic wake: an idle scheduler arms no timer, and a busy one arms one, for its
earliest deadline.

One rule says which clock a deadline is on:

- **Wall clock** (re-read on wake): trigger due times and retry times, approval expiry
  (`expires_at`), standing-rule expiry, run leases, the budget day, the idle limit of a thread (one
  deadline, for the thread that has been idle the longest). On wake a pending approval past its
  `expires_at` expires at once through the same path as its timer, every live run's lease is
  renewed before the queue is looked at again, and the scheduler fires what came due. A run
  token's `expires_at` is a wall-clock safety net (the greater of four times the run's wall-time
  budget and 24 hours); the token's real bound is the run being live and not revoked.
- **Awake time** (the monotonic timer, deliberately): a run's `max_wall_time` budget, the cancel
  grace and the heartbeat period. A sleeping Mac does no work, so a run is not charged for the
  hours it slept; a run's screen therefore shows time awake, which can be less than the time since
  it started.

A watch that cannot start is the one other timer. The platform has a finite pool of file-watch
streams shared by every process on the machine, so a start can be refused for a while. The daemon
then keeps serving what it has (the last good definitions, a trigger shown as failed), says so
(`daemon.status`, `trigger.list`, one warning in the log) and retries the start after a doubling
delay with a ceiling. That timer is armed only while a start is failing, ends with the first start
that works, and never asks whether files changed; once the watch runs, changes arrive as events
again.

## Queues and tasks

Every queue is bounded, and every task is owned (ARCH-11). Each channel has a capacity. Each
spawned task belongs to a task tracker and a cancellation token, so shutdown waits for it. Actors
reply by one-shot channel and never form a cycle of bounded channels.

A slow reader never costs unbounded memory (ARCH-12). Subscriptions are bounded per client. A
client that falls behind gets a gap notice and resumes from a sequence number.

## Executor trust

No executor is trusted beyond what it declares (ARCH-15). An executor that cannot honour the
requested access mode is ineligible; it is never run with more access than asked. A driver never
claims more than a green run verified (`ExecutorDescriptor::cli_verified`, see
`docs/KEEPING-UP.md`).

## Not the invariant spine

Runtime narrative does **not** redefine ARCH-N invariants.
