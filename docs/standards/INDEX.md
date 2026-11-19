# Engineering standards index

Status: Approved
Date: 2026-11-19

## Purpose and boundary

The engineering rules every change to Reno follows that a formatter or linter cannot check.
`cargo fmt` and `cargo clippy -D warnings` are enforced in CI and are not repeated here. Rust 2024,
`rust-version` 1.98. It explains ARCH-17 from `docs/architecture/INDEX.md`.

## Standards catalog

| Document | Path | Scope |
|---|---|---|
| Testing | `docs/standards/testing.md` | Test layers, test-first, what a test may touch |
| House rules | `docs/standards/INDEX.md` (below) | Dependencies, installs, harness boundaries |

## House rules

- Follow the architecture invariants in `docs/architecture/INDEX.md`. A change that breaks one edits that file in the same commit, with a reason.
- External dependencies are declared once in the workspace manifest and named by members with `.workspace = true`; they are added as a task needs them.
- Dependencies stay permissive and current (ARCH-17): `cargo-deny` allows only permissive licenses, rejects known advisories, and fetches git sources only from an allow-list. `cargo-machete` rejects unused dependencies.
- Reno reads and launches harness CLIs; it never writes their config, auth or Keychain entries.
- Install a binary through a temp file and `mv`, never `cp` onto one that may be running (`docs/INSTALL.md`).

## Source of truth rule

Canonical engineering standards live under `docs/standards/`. `docs/product/guidelines.md` is a **pointer only** after migration (legacy fallback only while unmigrated content remains). Never maintain parallel SSOTs.

## Not ARCH-N

Standards docs do **not** redefine greppable `ARCH-N` architecture invariants.
