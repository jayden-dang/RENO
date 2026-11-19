# Architecture: Reno

Status: Approved
Date: 2026-11-19

The rules that keep Reno modular as it grows. Each one is checked by the compiler, a test, or
`cargo xtask check`. This file is the only place an `ARCH-N` is defined; the domain files below
explain them and never redefine them. A change that breaks an invariant edits this file in the
same commit, with a reason. IDs are never renumbered or reused: retire one by striking it through
and adding a fresh ID.

## Invariants

### Layering

- **ARCH-1** Dependencies point one way.
- **ARCH-2** The app and the CLI never link the kernel, the store or the harness layer.
- **ARCH-3** A crate exists only for a boundary or for reuse.

### Who knows what

- **ARCH-4** Only `reno-rpc` knows JSON-RPC framing.
- **ARCH-5** Only `reno-store` knows SQL.
- **ARCH-6** Only `reno-harness` knows a harness's wire format.
- **ARCH-7** The kernel does no I/O of its own.

### State

- **ARCH-8** SQLite is the only source of truth.
- **ARCH-9** Every effect is fenced.

### Concurrency

- **ARCH-10** Nothing polls.
- **ARCH-11** Every queue is bounded, and every task is owned.
- **ARCH-12** A slow reader never costs unbounded memory.

### Contracts

- **ARCH-13** The wire contract is versioned and snapshotted.
- **ARCH-14** Errors are typed inside and stable outside.
- **ARCH-15** No executor is trusted beyond what it declares.

### Tests and dependencies

- **ARCH-16** Tests need no network, no real harness and no wall clock.
- **ARCH-17** Dependencies stay permissive and current.

## Domains

- System — [system.md](./system.md): crate graph, components, who knows what, contracts (ARCH-1 to ARCH-6, ARCH-13, ARCH-14)
- Runtime — [runtime.md](./runtime.md): processes, kernel ports, clocks, queues, executor trust (ARCH-7, ARCH-10 to ARCH-12, ARCH-15)
- Data — [data.md](./data.md): the store, journals and fences (ARCH-8, ARCH-9)
- Tests and dependencies — [../standards/testing.md](../standards/testing.md) and [../standards/INDEX.md](../standards/INDEX.md) (ARCH-16, ARCH-17)

The long-form design record, with the reasoning and the numbered sections that code comments cite
as `ARCHITECTURE <n>`, is `docs/ARCHITECTURE.md`. It is internal and not published.
