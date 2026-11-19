# Project configuration (agent-facing)

Written by `configure-repo`. Skills read this file for repo-specific **machine config** —
commands, globs, paths — plus **posture** and **team** (below). Human-facing engineering
standards (coding rules, testing, house rules) live in `docs/standards/`;
`plan-tasks` sources them from there.

## Project posture

The project's standing intent, lifecycle phase, and compatibility obligation. Skills read
this instead of re-asking: `frame-change` and `clarify-decisions` right-size migration /
compat / deprecation against **Compat obligation**; `interpret-session` reuses all three.
`clarify-decisions` production coverage needs Delivery **Production** + Lifecycle
**Cut Released / Scaling / Maintenance** + operate/launch surface (or an ops ask); MVP,
absent, and Early/Active stay off. Edit these lines when the project moves phase.

**SSOT:** the **Compat obligation** values and what each one means live only here. Skills
carry a one-clause copy of the derivation because a repo configured before the field existed
has no Compat obligation text in this file for them to read — change the meanings here, and
the derivation in `frame-change`, `clarify-decisions`, and `interpret-session` with it.

- **Delivery intent:** `Production` — the quality bar the output must meet. A bar, not a release state: **Production** says how well the code must be built, never that the project has shipped.
- **Lifecycle stage:** `Active development` — where the project is in its life.
- **Compat obligation:** `Internal` — who is already committed to the current shapes, and so what a change is allowed to break. **None:** nothing outside this repo depends on any current schema, endpoint, or file format. **Internal:** other services or teams in the same org read them. **External:** users, customers, or third-party clients depend on them. **Derived when the line is absent** — Lifecycle Idea / Early development / Active development → **None**; Cut Released / Scaling / Maintenance → **External**. Written here as an override: the installed daemon's SQLite DB (migrations V15→V16 and on) and the daemon wire protocol carry state on the owner's machine, so a change to either needs a migration or a compatible bridge.
- **Default PR base:** `main`
- **Default landing action:** `merge` — also standing permission: `land-branch` pushes the base after a green local merge and closes the tickets it completed.

These are distinct from the product **Goals** in `docs/product/vision.md` (what success
looks like): posture is *how carefully to build right now*, not *what to build*.

## Team

Who works on this repo and how skills should package collaboration.
Skills that plan, review, or hand off read this section when present and
right-size **packaging** only (Solo / Small / Multi) — Iron Law gates never
change. Edit freely; re-run `/configure-repo` to re-draft from git/CODEOWNERS.
If this section is absent, skills do not invent a team.

**SSOT:** **band** derivation and the **packaging** matrix live only here.
Consumers **read** this section; they do not re-copy these rules into skill bodies.

### Roster

- Tech Lead — Jayden Đặng

Suggested roles (freeform allowed): Tech Lead, Backend Engineer, Frontend Engineer, Full-stack Engineer, Designer, Product Manager, QA, DevOps/SRE, Docs.

### Ownership notes (optional)

None (no CODEOWNERS).

### Workflow band

- **Override (optional):** blank — skills use the derived band.
- **Derive (when override blank):**
  1. Headcount from **Roster only**: each `Role — Name` = 1; each `N × Role` / `N Role(s)` adds N.
     Ignore Ownership notes and placeholders (`<…>`).
  2. Buckets: empty roster → **no band** (same packaging as Team absent); 1 → **Solo**;
     2–4 → **Small**; ≥5 → **Multi**.
  3. Specialty upgrade only: if **Small** and ≥3 distinct role titles (case-insensitive, trimmed),
     upgrade to **Multi**. Never downgrade Multi→Small.

### Packaging matrix

| Band | Packaging |
|---|---|
| **Solo** | Lean multi-person ritual language; no invented peer reviewers/assignees; agent-as-pair; full gates |
| **Small** | Design-review checkpoints; ownership boundaries via optional freeform notes; name people when roster has names |
| **Multi** | CODEOWNERS-aware review language when ownership notes exist; explicit review responsibilities as prose; write-handoff/docs emphasis |
| **(no band)** | Team absent, or empty roster with blank override — pre-feature default; do not invent a team; do not hard-fail |

## verify commands

Run in this order; all must pass before any completion claim.

| Check | Command |
|---|---|
| Typecheck | `cargo check --workspace --all-targets` |
| Lint | `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings` |
| Unit tests | `cargo test --workspace` |
| E2E / smoke | `cargo test -p reno --test e2e` and `cargo xtask check` (asserts the ARCH crate edge list from `cargo metadata`) |

Single test file: `cargo test -p <crate> --test <file>` — builds only that test binary.

Task gate (tests of the changed units and the dependents that call what changed, plus a compile of every other dependent; `none` when the whole suite is fast):
`cargo test -p <changed-crate> -p <caller-crate>` · `cargo check --workspace --all-targets`

Wave gate (compile + lint every target after a parallel merge; `none` = typecheck + lint):
`cargo check --workspace --all-targets && cargo clippy --workspace --all-targets -- -D warnings`

The whole suite runs once, at close. Each task runs its task gate instead, escalating
to the whole suite when the change reaches what the build graph cannot see (schema,
migrations, shared fixtures, manifests, generated contracts, a root package).

Not part of the gates: `cargo xtask conformance-live` and the `RENO_*_LIVE=1` tests spend paid
model calls and need signed-in harness CLIs. The owner runs those (see `docs/KEEPING-UP.md`).

The traceability check is not a command here — the `audit-trace` skill runs it as
`grep`/`git` over `docs/specs/` (and optional architecture). It is **docs-only**
and does not grep application tests for requirement IDs.

Legacy annotations in consumer code (if any) are ignored; do not require new ones.

Specs directory override: (blank — `docs/specs/`)

## Run locally (dev)

How to start the app for user-facing acceptance checks (read by `validate-feature`
and `write-dogfood`). The recipe lives in `docs/agents/verify.md`. **Never drive the owner's
live install:** always run an isolated daemon (own `--data-dir`, `--socket`, `--defs-dir`), never
`reno daemon start/install` (they reach launchd), and never a path that opens a real browser,
reads the Keychain, or runs `launchctl`.

| Surface | Start command | Ready signal |
|---|---|---|
| Daemon + CLI (isolated) | `cargo build -p reno -p renod`, then `D=$(mktemp -d /tmp/reno-v.XXXX); target/debug/renod --data-dir $D --socket $D/reno.sock --defs-dir $D/defs &` | `target/debug/reno --data-dir $D daemon status` exits 0 |
| GUI app (GPUI) | `cargo xtask bundle`, then open `target/bundle/Reno.app` | Owner-driven only; agents do not drive it |

Browser drive (kimi-webbridge): not applicable — Reno has no web UI. Do not add a Playwright suite.

## Remote environments

Read by `debug-remote` and `assess-observability`. Never put tokens here.

None — not deployed

## release steps

There is no release yet (`docs/INSTALL.md`: "There is no release yet"). No version bump, tag or
publish step exists. The ordered local ship path, consumed by `cut-release`:

1. `cargo build --release -p renod -p reno`
2. `cargo xtask bundle` (builds `target/bundle/Reno.app`, id `dev.reno.app`, ad-hoc signed)
3. Install the binaries into `~/.local/bin` via a temp file then `mv -f` — never `cp` onto a running binary (the kernel kills it with SIGKILL, exit 137):
   `for b in reno renod; do cp target/release/$b ~/.local/bin/.$b.new && mv -f ~/.local/bin/.$b.new ~/.local/bin/$b; done`
4. `launchctl kickstart -k gui/$(id -u)/dev.reno.renod`
5. Smoke check: `reno daemon status && reno executor list`

## Paths

Library docs: Context7 MCP (preferred)

- Specs: `docs/specs/`
- ADRs: `docs/adr/`
- Glossary: `CONTEXT.md`
- Out-of-scope KB: `.out-of-scope/`
- Architecture invariants (ARCH-1 to ARCH-17): `docs/architecture/INDEX.md`; domain narratives `docs/architecture/system.md`, `runtime.md`, `data.md`
- Engineering standards: `docs/standards/` (`docs/product/guidelines.md` is a pointer)
- Product vision: `docs/product/vision.md`
- Long-form internal design (gitignored; code comments cite its sections as `ARCHITECTURE <n>`): `docs/ARCHITECTURE.md`
