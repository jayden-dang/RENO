# Architecture domain: System

Status: Approved
Date: 2026-11-19

## Purpose and boundary

The shape of the workspace: which crates and apps exist, how they depend on each other, which
crate alone knows each format, and the contracts that cross the socket. It explains ARCH-1 to
ARCH-6, ARCH-13 and ARCH-14 from `docs/architecture/INDEX.md`. Processes and timing are in
`runtime.md`; persisted state is in `data.md`.

## Context

Reno is a daemon (`renod`) that coordinates the agent CLIs on one Mac, plus two clients of it: the
command line (`reno`) and the desktop app (`reno-app`). Clients reach the daemon only over its Unix
socket.

Dependencies point one way (ARCH-1):

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
kernel (ARCH-7 puts the ports' implementations outside the kernel). Its only edge on `reno-client`
is a dev-dependency, for tests.

The app and the CLI know the daemon only through `reno-client` (ARCH-2). Every feature the app
has, the CLI can reach.

A crate needs either a dependency that only it may have, or at least two consumers; everything
else is a module (ARCH-3). Imported third-party code counts as a boundary: it keeps its own crate,
notice and lint table.

## Components

| Component | Responsibility |
|---|---|
| `reno-proto` | The shared vocabulary: ids, domain types, `ExecEvent`, the typed protocol, `PROTOCOL_VERSION` and error codes |
| `reno-store` | The only crate that knows SQL (ARCH-5): SQLite bindings, migrations, the writer thread, the read pool, run journals and artifacts. No other crate depends on the SQLite bindings |
| `reno-harness` | The executors, one per agent CLI, behind the kernel's `Executor` port; the only crate that knows a harness's wire format (ARCH-6). Its `wire/` modules are sans-IO: they turn bytes or frames into events and never spawn processes, touch files, sleep or read the clock |
| `reno-kernel` | The brain of the daemon: dispatcher, leases, run supervisors, recovery, scheduler and router. Does no I/O of its own (ARCH-7, see `runtime.md`) |
| `reno-rpc` | The only crate that knows JSON-RPC framing (ARCH-4): ndjson over a Unix socket, the id-to-reply map, subscriptions with resume, an in-memory transport for tests |
| `reno-client` | The typed client over `reno-rpc` plus a GPUI-free state mirror with a pure `apply(event)`; the one way the app and the CLI reach the daemon (ARCH-2) |
| `renod` | The daemon and composition root. The only place protocol methods are mapped to kernel calls (ARCH-4) |
| `reno` | The command line, and Reno's own MCP server for a run. The only crate that knows MCP (ARCH-4) |
| `reno-app` | The desktop app on GPUI |
| `reno-markdown` | Block-level markdown over pulldown-cmark, shared by every frontend |
| `reno-syntax` | Syntax-highlighting contracts for the desktop surfaces; no UI, RPC or engine dependencies |
| `reno-term` | The terminal emulator core, with no renderer or I/O |
| `reno-notify` | Desktop banners and the Dock badge, for the app |
| `xtask` | Repo tooling (`check`, `bench`, `bundle`, conformance, schema diff); outside the dependency graph |

## Contracts

The wire contract is versioned and snapshotted (ARCH-13). Protocol types generate a schema that CI
compares against the committed snapshot. Within a protocol version, changes are additive only.
Optional methods are gated by features exchanged in `daemon.hello`.

Errors are typed inside and stable outside (ARCH-14). Each library crate exposes one error type.
Binaries may use catch-all errors. Every error that crosses the socket has a stable code.

## Not the invariant spine

This narrative domain file does **not** redefine `docs/architecture/INDEX.md` ARCH-N invariants.
