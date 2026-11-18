# Architecture invariants

These are the rules that keep Reno modular as it grows. Each one is checked by the compiler, a
test, or `cargo xtask check`. A change that breaks one changes this file in the same commit, with
a reason.

## Layering

**ARCH-1 — Dependencies point one way.**

```
                 reno-proto
        ┌─────────┼───────────┬───────────┐
   reno-store  reno-harness  reno-rpc     │
        └────┬────┘              │        │
        reno-kernel          reno-client ─┘
             │                ┌──┴───────────┐
           renod ◄─ reno-rpc  reno (CLI+mcp) reno-app ◄─ reno-markdown, reno-syntax, reno-term
```

`cargo xtask check` asserts this edge list from `cargo metadata`. The `xtask` tool itself sits
outside it.

`renod` is the composition root, so besides the kernel and `reno-rpc` it also names `reno-store`
and `reno-harness` directly: it opens the store and builds the executors that it hands to the
kernel (ARCH-7 puts the ports' implementations outside the kernel). Its only edge on
`reno-client` is a dev-dependency, for tests.

**ARCH-2 — The app and the CLI never link the kernel, the store or the harness layer.** They
know the daemon only through `reno-client`. Every feature the app has, the CLI can reach.

**ARCH-3 — A crate exists only for a boundary or for reuse.** It needs either a dependency that
only it may have, or at least two consumers. Everything else is a module. Imported third-party
code counts as a boundary: it keeps its own crate, notice and lint table.

## Who knows what

**ARCH-4 — Only `reno-rpc` knows JSON-RPC framing.** Only `renod` maps protocol methods to
kernel calls. Only `apps/reno` knows MCP.

**ARCH-5 — Only `reno-store` knows SQL.** No other crate depends on the SQLite bindings.

**ARCH-6 — Only `reno-harness` knows a harness's wire format.** Its `wire/` modules are sans-IO:
they turn bytes or frames into events and never spawn processes, touch files, sleep or read the
clock.

**ARCH-7 — The kernel does no I/O of its own.** It reaches the world only through `reno-store` and
its ports:
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

The ports are implemented outside the kernel and faked in tests. The kernel never reads the
system clock, spawns a process, or opens a file itself.

## State

**ARCH-8 — SQLite is the only source of truth.** Every state change commits in the same
transaction as the event that records it, under one global sequence. A run's journal holds its
streamed events for display and replay. Its end marker is written only after the run's final
state commits, it is repaired from state after a crash, and it is never read to decide state.

**ARCH-9 — Every effect is fenced.** A claim returns a fence. Every write that has an effect
carries it, and a stale fence is rejected. An external effect records its intent before it acts.

## Concurrency

**ARCH-10 — Nothing polls.** Every timer is a deadline: the next trigger, the next lease expiry,
a live run's heartbeat, a checkpoint after the last write. Changes to files and git refs arrive
as events.

The one thing a deadline cannot see is the Mac sleeping: the monotonic timer stops, the wall clock
does not. The daemon turns the system's wake notification into a signal (`WakeSignal`) that the
scheduler, the dispatcher and every run's supervisor hear, and each then reads the clock again.
There is no periodic wake: an idle scheduler arms no timer, and a busy one arms one, for its
earliest deadline.

One rule says which clock a deadline is on. **Wall clock** (re-read on wake): trigger due times and
retry times, approval expiry (`expires_at`), standing-rule expiry, run leases, the budget day. On
wake a pending approval past its `expires_at` expires at once through the same path as its timer,
every live run's lease is renewed before the queue is looked at again, and the scheduler fires
what came due. A run token's `expires_at` is a wall-clock safety net (the greater of four times
the run's wall-time budget and 24 hours); the token's real bound is the run being live and not
revoked. **Awake time** (the monotonic timer, deliberately): a run's `max_wall_time` budget, the
cancel grace and the heartbeat period. A sleeping Mac does no work, so a run is not charged for
the hours it slept; a run's screen therefore shows time awake, which can be less than the time
since it started.

A watch that cannot start is the one other timer. The platform has a finite pool of
file-watch streams shared by every process on the machine, so a start can be refused for a while. The daemon then keeps serving what it
has (the last good definitions, a trigger shown as failed), says so (`daemon.status`,
`trigger.list`, one warning in the log) and retries the start after a doubling delay with a
ceiling. That timer is armed only while a start is failing, ends with the first start that works,
and never asks whether files changed; once the watch runs, changes arrive as events again.

**ARCH-11 — Every queue is bounded, and every task is owned.** Each channel has a capacity. Each
spawned task belongs to a task tracker and a cancellation token, so shutdown waits for it.
Actors reply by one-shot channel and never form a cycle of bounded channels.

**ARCH-12 — A slow reader never costs unbounded memory.** Subscriptions are bounded per client. A
client that falls behind gets a gap notice and resumes from a sequence number.

## Contracts

**ARCH-13 — The wire contract is versioned and snapshotted.** Protocol types generate a schema
that CI compares against the committed snapshot. Within a protocol version, changes are additive
only. Optional methods are gated by features exchanged in `daemon.hello`.

**ARCH-14 — Errors are typed inside and stable outside.** Each library crate exposes one error
type. Binaries may use catch-all errors. Every error that crosses the socket has a stable code.

**ARCH-15 — No executor is trusted beyond what it declares.** An executor that cannot honour the
requested access mode is ineligible; it is never run with more access than asked.

## Tests and dependencies

**ARCH-16 — Tests need no network, no real harness and no wall clock.** Tests against real
harnesses are opt-in (`RENO_LIVE=claude,codex`).

**ARCH-17 — Dependencies stay permissive and current.** `cargo-deny` allows only permissive
licenses, rejects known advisories, and fetches git sources only from an allow-list.
`cargo-machete` rejects unused dependencies.
