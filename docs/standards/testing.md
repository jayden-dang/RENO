# Testing standards

Status: Approved
Date: 2026-11-19

## Purpose and boundary

How Reno's tests are written and what they may touch. It explains ARCH-16 from
`docs/architecture/INDEX.md`. The commands that run the suite are in `docs/agents/project.md`
(verify commands).

## Strategy

| Layer | Purpose | When required |
|---|---|---|
| unit | Pure logic and actors against faked ports (`Clock`, `Executor`, `Workspace`) | Every change to production code |
| integration | A crate's public behaviour through its `tests/` binaries, e.g. a driver over its fake CLI, the store over a temp database | A change that crosses a module boundary or a wire format |
| e2e | The CLI against a real `renod` on a temp data dir (`cargo test -p reno --test e2e`) | A change to a command or a protocol method |
| snapshot | The protocol schema (`reno-proto/tests/schema.rs`) and the Codex schema; `insta` snapshots | A change to a protocol type; an intended change updates the committed snapshot in the same commit |
| live (opt-in) | A real agent CLI, run by the owner: `RENO_CONFORMANCE_LIVE`, `RENO_PI_LIVE`, `RENO_MUSE_LIVE`, `RENO_CURSOR_LIVE`, `RENO_OPENCODE_LIVE` and the other `RENO_*LIVE*` flags, with `-- --ignored` | Before raising a driver's `cli_verified` (`docs/KEEPING-UP.md`) |

## Test-first rules

- No production code without a failing test first.
- Tests need no network, no real harness and no wall clock (ARCH-16): time comes from the `Clock` port, harness runs from the mock executor or a fake CLI.

## Mocks and boundaries

- Fake at the kernel's ports and at process boundaries only: the `Executor` (mock executor, fake CLIs), the `Clock`, `launchctl` (`RENO_LAUNCHCTL` points at a fake), the Keychain behind `Secrets`.
- A test must never reach the owner's machine: not `/usr/bin/open` or a real browser, not the Keychain, not `launchctl`, not `~/.reno`, the default data directory or a harness CLI's own config. Guard such a path so it panics or fails under test (as `reno-app`'s `open` does), so a leak fails the test instead of passing silently.
- Live tests write to throwaway HOME, config and data directories, never the owner's.

## Not a substitute for acceptance

Unit conventions here do not replace `validate-feature` / e2e acceptance.
