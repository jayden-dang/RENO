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
           renod ◄─ reno-rpc  reno (CLI+mcp) reno-app ◄─ reno-markdown, reno-syntax
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
- `Definitions` — the definitions repository;
- `Secrets` — the Keychain.

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
as events. One bounded exception: the scheduler never sleeps longer than an hour, because the
monotonic timer does not advance while the Mac sleeps; that wake only reads the wall clock and
writes nothing. It goes away once the daemon gets the system's wake notification.

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
