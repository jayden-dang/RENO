# Architecture domain: Data

Status: Approved
Date: 2026-11-19

## Purpose and boundary

Where Reno's state lives and the rules every write follows. It explains ARCH-8 and ARCH-9 from
`docs/architecture/INDEX.md`. Only `reno-store` touches the database (ARCH-5, see `system.md`).

## Entities and stores

| Entity / store | Notes |
|---|---|
| SQLite database | The only source of truth (ARCH-8), in the daemon's data directory. Schema changes ship as migrations; the owner's installed database carries state across upgrades (Compat obligation `Internal` in `docs/agents/project.md`) |
| Run journal | A run's streamed events, kept for display and replay. Never read to decide state |
| Definitions repository | Bots, triggers and the policy as TOML files in a git repository (`~/.reno` by default), read through the kernel's `Definitions` port |
| Keychain | Secret values, read through the kernel's `Secrets` port. Never stored in a definition file |

## Consistency rules

- Every state change commits in the same transaction as the event that records it, under one global sequence (ARCH-8).
- A run journal's end marker is written only after the run's final state commits, is repaired from state after a crash, and is never read to decide state (ARCH-8).
- A claim returns a fence. Every write that has an effect carries it, and a stale fence is rejected (ARCH-9).
- An external effect records its intent before it acts (ARCH-9).

## Not the invariant spine

Data narrative does **not** redefine ARCH-N invariants.
