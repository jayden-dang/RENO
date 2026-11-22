# Tasks: Bots run on Reno's own harness, with every tool call seen by Reno

> **For agentic workers:** after the plan is approved, pick one execute skill:
> - `build` (wave mode): a continuous, dependency-aware scheduler;
> - `build` (story mode): human-gated story review units;
> - `build` (inline mode): the controller implements, with no implementer subagents.
>
> The chosen skill writes `Execution-mode:`. Steps use checkbox (`- [ ]`) syntax for tracking.

Feature code: RENOHARNESS
Status: Draft
Date: 2026-11-22
Execution-mode: unset
Max-concurrency: auto
Requirements: ./requirements.md
Design: ./design.md

**Goal:** A bot that lists `reno` runs on the installed Pi with only its own Keychain key. Every tool call is held
by Reno's gate until the kernel's unchanged approval ladder answers it, and the run never starts unless the gate is
confirmed live.

**Architecture:** The Pi driver gains a `Flavor::Reno`. That flavor launches Pi:
- with a per-attempt `.reno` directory holding the gate extension, the agent dir and the socket;
- with a durable per-run session dir under the data dir.

A `GateHost` in the driver accepts the gate's one connection, arms before the first prompt, and turns each call into
`ExecEvent::NeedsApproval`. Two default-no-op `Executor` methods let the kernel remove session dirs at a run's final
state and at start.

**Tech Stack:** Rust 2024 (tokio, serde_json, uuid), Node ≥ 22 for the gate extension (`gate.mjs`), Python 3 for the
fake Pi fixture, `cargo test`.

## Global Constraints

Every task follows these sources, by reference:
- `docs/standards/INDEX.md` (sha256 `c9022b57a31d…`): house rules. Dependencies come from the workspace manifest;
  Reno never writes a harness's config, auth or Keychain entries.
- `docs/standards/testing.md` (sha256 `bfd06e83b4ca…`): test-first, and ARCH-16 (no network, no real harness, no
  wall clock). A test never touches the Keychain, `~/.reno`, `~/.pi` or the default data dir.
- `docs/agents/project.md` (sha256 `e33f8b2f1429…`): the verify commands.
  - Task gate: `cargo test -p <changed-crate> -p <caller-crate>` and `cargo check --workspace --all-targets`.
  - Close: the full list, including `cargo xtask check`.
- Architecture spine `docs/architecture/INDEX.md` (sha256 `e768140c57da…`): every task inherits ARCH-6 (only
  `reno-harness` knows Pi's wire and the gate protocol), ARCH-7 (no I/O in the kernel), ARCH-8 and ARCH-9, ARCH-10
  (no polling), ARCH-11 (bounded queues, owned tasks), ARCH-13 (no wire change), ARCH-15, ARCH-16 and ARCH-18.
- Charter limits for this feature: no new wire method, no new `ExecEvent`, `RejectReason` or `StartRefusal` variant,
  no migration, and no new `BotDef` field (requirements Out of Scope).
- Platform: the gate code is `#[cfg(unix)]`. Off unix, `PiExecutor::reno().start` returns
  `ExecError::InvalidSpec`, so it fails closed and is never ungated.
- Test filters: `cargo test` takes one name filter per invocation, so commands chain with `&&`.
- Naming: no reference agent project other than Pi in code, comments, tests or commits.
- Team: Solo (`docs/agents/project.md` `## Team`). No assignees.

## File Structure

| Path | Change | Responsibility |
|---|---|---|
| `crates/reno-harness/src/drivers/pi/mod.rs` | Modify | `Flavor`, `RENO_EXECUTOR_ID`, `PiExecutor::reno`, `with_arming_budget`, the flavor branches in `descriptor`, `probe`, `maintenance` and `start` (with `require_access` per flavor), `launch_pi(.., flavor)`, arming in `Run::start_up`, the gate arm in `Player::play`, `run_closed`/`retain_runs`, docs, and unit tests of `launch_pi` |
| `crates/reno-harness/src/drivers/pi/gate.rs` | Create | `GateHost` (`#[cfg(unix)]`): the socket, the one-time accept, the peer uid, the nonce, the open-call bound and the writer task |
| `crates/reno-harness/src/drivers/pi/gate.mjs` | Create | The gate extension on Pi's `tool_call` hook |
| `crates/reno-harness/src/drivers/pi/rpc.rs` | Modify | The per-run `refuse_host_bash` flag on `Client` |
| `crates/reno-harness/src/wire/pi/launch.rs` | Modify | The `Launch` fields `session_dir`, `builtins` (which also drops `--no-mcp`), `no_approve` and `gate`, their argv, and the struct literals in its tests |
| `crates/reno-harness/src/wire/pi/frames.rs` | Modify | The `get_commands()` builder and `lists_extension_command(data, name)` parser |
| `crates/reno-harness/src/wire/pi/subject.rs` | Create | `subject()`: a Pi gate call to an `ApprovalSubject` (sans-IO) |
| `crates/reno-harness/src/wire/pi/mod.rs` | Modify | Declares `subject` |
| `crates/reno-harness/src/child_env.rs` | Modify | `pub const RENO: DriverEnv` (BASE only) |
| `crates/reno-harness/src/exec/mod.rs` | Modify | `Executor::run_closed` and `Executor::retain_runs`, as default no-ops |
| `crates/reno-harness/tests/pi.rs` | Modify | `reno` tests through `start` against the fake Pi, from Task 6 on, plus the live test |
| `crates/reno-harness/tests/fixtures/fake-pi.py` | Modify | Plays the gate: the arming mode from `FAKE_PI_GATE`, `get_commands` with `reno-gate`, gate calls after `tool_execution_start`, `--session-dir`, and `scenario:approval-subjects` |
| `crates/reno-harness/tests/fixtures/pi-rpc-probe.ts` | Modify | A scripted-tool mode for the live Reno test |
| `crates/reno-harness/tests/misc/main.rs` | Modify | Declares `pi_gate` |
| `crates/reno-harness/tests/misc/pi_gate.rs` | Create | Runs `gate.mjs` under node against a scripted socket |
| `crates/reno-harness/tests/fixtures/pi-gate-test.mjs` | Create | The node driver for that test |
| `crates/reno-harness/tests/conformance/targets.rs` | Modify | `reno_target()` with an approval plan |
| `crates/reno-harness/tests/conformance/main.rs` | Modify | Registers `reno_target()` and pins `labels("reno")` |
| `crates/reno-kernel/src/router.rs` | Modify | `RouteRequest.unfit_config`, which becomes `RejectReason::Excluded` |
| `crates/reno-kernel/src/kernel.rs` | Modify | `plan_route` reads `inherit_user_config` and fills `unfit_config` |
| `crates/reno-kernel/src/dispatcher.rs` | Modify | The excluded-only failure text for a bot run |
| `crates/reno-kernel/src/workspaces.rs` | Modify | `run_ended` calls `run_closed` for `Succeeded` and `Cancelled` |
| `crates/reno-kernel/src/recovery.rs` | Modify | Awaits `retain_runs(keep)` before re-queue and claims |
| `crates/reno-kernel/src/tests/approvals.rs` | Modify | Ladder tests for gate-shaped subjects |
| `crates/reno-kernel/src/tests/protect.rs` | Modify | `.reno` component tests for Pi subjects |
| `crates/reno-kernel/src/tests/routing.rs` | Modify | The `inherit_user_config` exclusion, its message, and the no-fallback test |
| `crates/reno-kernel/src/tests/recovery.rs` | Modify | Keep-set and sweep-order tests, with a test-local recording executor |
| `crates/reno-kernel/src/tests/workspaces.rs` | Modify | `run_closed` on final states, with the same test-local executor |
| `crates/reno-kernel/src/tests/secrets.rs` | Modify | A missing secret refuses a `reno` bot's start |
| `crates/reno-kernel/src/supervisor/protect.rs` | Modify | Docs: Pi "asks nothing" is true only of `pi` |
| `crates/reno-proto/src/defs/mod.rs` | Modify | `BUILTIN_EXECUTORS` gains `"reno"` |
| `crates/reno-proto/src/defs/bot.rs` | Modify | Docs on `inherit_user_config` |
| `crates/reno-proto/src/defs/tests.rs` | Modify | Validation tests for `reno`, and the README docs test |
| `apps/renod/src/executors.rs` | Modify | `RENO_ID`, the probe-free `harness_candidates(data_dir)`, and a `discover` that gains `data_dir` |
| `apps/renod/src/daemon.rs` | Modify | Passes the data dir to `discover` |
| `apps/renod/README.md` | Modify | The executor list (line 119) names `reno` |
| `README.md` | Modify | Rewrites line 58 |
| `docs/KEEPING-UP.md` | Modify | The Pi bump checklist: re-run the gate matrix, the reload-path check and the ambient-credential check |

---

### Task 1: Register the `reno` executor (fails closed until armed)

**Files:**
- Modify: `crates/reno-harness/src/drivers/pi/mod.rs` (`PiExecutor` struct at lines 200-269, `descriptor` at 343-396, `probe`, `maintenance`, and `start` at 466-550, where `require_access` at 471 becomes per flavor)
- Modify: `apps/renod/src/executors.rs` (`discover` at lines 233-277, `PI_ID` at 39)
- Modify: `apps/renod/src/daemon.rs` (the `discover` call site)
- Modify: `crates/reno-proto/src/defs/mod.rs` (`BUILTIN_EXECUTORS`, lines 120-133)
- Test: `crates/reno-harness/tests/pi.rs`, `apps/renod/src/executors.rs` (`#[cfg(test)]`, beside `built_in_ids_are_valid` at line 322), `crates/reno-proto/src/defs/tests.rs`

**Reuse:** rung 2. `PiExecutor` (`drivers/pi/mod.rs:200-269`) gains a `Flavor`. Registration is one more entry in
`discover` (`apps/renod/src/executors.rs:249-274`). `BUILTIN_EXECUTORS` (`crates/reno-proto/src/defs/mod.rs:120-133`)
gains `"reno"`.

**Interfaces:**
- Produces:
  - `pub const RENO_EXECUTOR_ID: &str = "reno"` and `PiExecutor::reno(sessions_root: PathBuf) -> PiExecutor`;
  - the private `enum Flavor { Owner, Reno { sessions_root: PathBuf } }`;
  - `PiExecutor::with_arming_budget(Duration) -> Self`, like `with_probe_budget`;
  - in renod: `pub const RENO_ID: &str`, `fn harness_candidates(data_dir: &Path) -> Vec<Arc<dyn Executor>>` (pure
    construction, no probing), and
    `discover(hold_mock: bool, stream_mock: Option<StreamMock>, harnesses: bool, data_dir: &Path)`.
- Consumes: none.

**Depends-on:** none

- [ ] Write failing tests:
  - The `reno` descriptor declares `[Supervised, FullAccess]`, `[Unrestricted]`, `resume: true` and
    `isolates_user_config: true`.
  - Its probe reports `SignedIn::Unknown` and the curated models, never reading the owner's credentials.
  - `maintenance()` is `None`.
  - `PiExecutor::new()`'s descriptor is today's.
  - `start` on `reno` returns `InvalidSpec`.
  - `harness_candidates(tmp)` holds one `pi` and one `reno`.
  - A bot listing `reno` validates offline, while `pi` and every other built-in id still validate as today.
- [ ] Run `cargo test -p reno-harness --test pi reno_ && cargo test -p renod executors && cargo test -p reno-proto defs`.
  Expect: unresolved `PiExecutor::reno` / `RENO_EXECUTOR_ID`, and unknown executor `reno`.
- [ ] Implement:
  - the `Flavor` field and the builders, with the branches in `descriptor`, `probe` and `maintenance`;
  - `require_access` against the flavor's modes;
  - in `start`, `Flavor::Reno` returns `ExecError::InvalidSpec("the Reno harness is not armed in this build")`
    until Task 6, so nothing runs ungated;
  - `harness_candidates` and the `discover` parameter, with `data_dir.join("harness/.reno/pi-sessions")` as
    `sessions_root`, and the daemon call site;
  - `"reno"` in `BUILTIN_EXECUTORS`.
- [ ] Run the same commands, and expect a pass. Then run `cargo check --workspace --all-targets`.
- [ ] `git commit -m "feat(harness): register the reno executor on the Pi driver, refusing runs until armed"`

_Requirements: RENOHARNESS-1.1, RENOHARNESS-1.2, RENOHARNESS-1.7, RENOHARNESS-10.6, RENOHARNESS-10.7, RENOHARNESS-10.8_

---

### Task 2: Route bots to `reno` only when it can take them

**Files:**
- Modify: `crates/reno-kernel/src/router.rs` (`RouteRequest` at lines 59-76, the `unfit_effort` check near 226)
- Modify: `crates/reno-kernel/src/kernel.rs` (`plan_route`, lines 1575-1597: add `inherit_user_config` to the
  bot-read tuple, and fill `unfit_config`)
- Modify: `crates/reno-kernel/src/dispatcher.rs` (the excluded-only text, lines 828-845)
- Modify: `crates/reno-harness/src/drivers/pi/mod.rs` (`start`: the no-bot and `inherit_user_config` checks)
- Test: `crates/reno-kernel/src/tests/routing.rs` (`MockExecutor::with_descriptor` to give `reno` its real modes),
  `crates/reno-harness/tests/pi.rs`

**Reuse:** rung 2, in three places:
- the router's caps check (`crates/reno-kernel/src/router.rs:94-119`) gives 1.4 from the descriptor alone;
- the `unfit_effort` slot (`router.rs:74,226`; filled in `kernel.rs:1575-1597`) is the template for 1.5;
- `ExecError::InvalidSpec`, which maps to `StartRefusal::Invalid` with no fallback (`dispatcher.rs:639`), gives 1.6.

**Interfaces:**
- Produces: the `RouteRequest` field `unfit_config: Vec<(ExecutorId, String)>`, set at every construction site
  (`rg 'RouteRequest \{'`, ten sites); the exclusion text
  `"bot <name> sets inherit_user_config = true, which the Reno harness never loads"`.
- Consumes: `RENO_EXECUTOR_ID` (Task 1).

**Depends-on:** Task 1

- [ ] Write failing kernel tests:
  - A bot with `executors = ["reno", "codex"]` and `inherit_user_config = true` routes to `codex`, with `reno`
    rejected as `Excluded` naming the setting.
  - With `["reno"]` alone, the run fails with "no executor this bot lists can take the task (…inherit_user_config…)",
    with no "use a bot" advice.
  - An explicit `--executor reno` for that bot is excluded the same way.
  - A bot set to `auto_accept_edits`, or to a narrower sandbox, finds `reno` ineligible with today's reason.
  - The fallback for an account or install decline still moves to the next listed executor.
- [ ] Write failing driver tests in `tests/pi.rs`: a spec with no `RENO_BOT`, or with `inherit_user_config`, is
  refused with `InvalidSpec` and the "runs bots only" reason, before the not-armed refusal.
- [ ] Run `cargo test -p reno-kernel routing && cargo test -p reno-harness --test pi reno_`. Expect: the
  `inherit_user_config` bot is routed to `reno`, and the text is wrong.
- [ ] Implement `unfit_config` (router, every construction site, and `plan_route`), the dispatcher text for a run with
  a bot, and the two `InvalidSpec` checks at the top of the `Reno` branch of `start`.
- [ ] Run the same commands, and expect a pass.
- [ ] `git commit -m "feat(kernel): exclude the Reno harness for bots that inherit the owner's config, and say why"`

_Requirements: RENOHARNESS-1.4, RENOHARNESS-1.5, RENOHARNESS-1.6, RENOHARNESS-10.5_

---

### Task 3: Lay out a Reno attempt: flags, files and session dir

**Files:**
- Modify: `crates/reno-harness/src/wire/pi/launch.rs` (`Launch` near line 26 gains fields; `args` at 35-58, where
  `builtins` drops `--no-mcp`; existing test literals gain `..Default::default()`)
- Modify: `crates/reno-harness/src/drivers/pi/mod.rs`:
  - `launch_pi` (lines 623-707) gains `flavor: &Flavor` and builds the `.reno` layout, gate file, session dir and
    socket path for `Reno`;
  - the `Owner` probe's `Launch` at line 285 keeps the defaults;
  - add unit tests in its `#[cfg(test)] mod tests`.
- Create: `crates/reno-harness/src/drivers/pi/gate.mjs` (a stub that registers `reno-gate` and blocks every call;
  completed in Task 5)
- Modify: `crates/reno-harness/tests/fixtures/fake-pi.py` (log its argv and the paths it was given, as `FAKE_PI_LOG`
  already does)
- Test: unit tests in `crates/reno-harness/src/drivers/pi/mod.rs` that call `launch_pi` directly against
  `tests/fixtures/fake-pi.py`, and in `wire/pi/launch.rs`

**Reuse:** rung 2, in four places:
- `launch_pi` (`drivers/pi/mod.rs:623-707`);
- `ScratchDir` (`proc/scratch.rs:30-72`, 0700 and removed on drop);
- `mcp::write_private` (`mcp.rs:34-49`, 0600 and never overwritten) and `mcp::install` (`mcp.rs:52-70`);
- `child_env::restrict` with a new `DriverEnv` (`child_env.rs:46,359`), and the `launch::args` table
  (`wire/pi/launch.rs:35-58`).

**Interfaces:**
- Produces: the `Launch` fields `session_dir: Option<&Path>`, `builtins: bool`, `no_approve: bool` and
  `gate: Option<&Path>`. They default to `None`/`false`, so the `Owner` argv is unchanged.
- The `Reno` argv:
  `--mode rpc --no-themes --no-extensions --no-skills --no-prompt-templates --no-approve --session-dir <root>/<run_id> [--session-id] [--append-system-prompt] -e builtin:mcp -e builtin:codemode [--extension <bridge>] --extension <gate>`.
- The layout: `<scratch>/.reno/{gate.mjs, gate.sock, agent/, reno-mcp.mjs, mcp-servers.json, instructions.md}`. When
  that socket path is 104 bytes or more, the socket goes to `/tmp/reno-gate-<uuid>/.reno/gate.sock`.
- The env, set last: `RENO_GATE_SOCKET`, `RENO_GATE_NONCE` (uuid v4) and `PI_CODING_AGENT_DIR=<scratch>/.reno/agent`.
- Consumes: `Flavor::Reno { sessions_root }` (Task 1).

**Depends-on:** Task 1

- [ ] Write failing unit tests:
  - `args` for `Reno` is exactly the list above, with no `--no-mcp` and the gate the last `--extension`.
  - `args` for the default `Launch` is today's list, and the pinned `tests/pi.rs`
    `the_command_line_isolates_the_owners_tools_and_trusts_nothing` stays green.
  - `launch_pi(.., &Flavor::Reno{..})` against the fake writes every file under a `.reno` component, and makes the
    session dir `<sessions_root>/<run_id>` with mode 0700.
  - Two launches of the same run get a new scratch, gate file, nonce and socket path, but the same session dir.
  - With a 90-byte `TMPDIR` passed to the scratch builder, the socket path falls back to `/tmp/reno-gate-*`.
  - After the attempt is torn down (normally, and with the fake killed), its scratch is gone.
- [ ] Run `cargo test -p reno-harness --lib drivers::pi && cargo test -p reno-harness --lib wire::pi::launch`, and
  expect the new `Launch` fields and the `launch_pi` parameter to be unresolved.
- [ ] Implement the `Launch` fields and `args`, `launch_pi(.., flavor)` with the layout, session dir (0700) and
  socket path rule, and the `gate.mjs` stub shipped with `include_str!` and `mcp::write_private`.
- [ ] Run those commands, then `cargo test -p reno-harness --test pi`. Expect a pass, `pi` included.
- [ ] `git commit -m "feat(harness): lay out each Reno attempt under .reno with a durable session dir"`

_Requirements: RENOHARNESS-1.3, RENOHARNESS-5.4, RENOHARNESS-7.3, RENOHARNESS-7.5, RENOHARNESS-8.4, RENOHARNESS-10.1, RENOHARNESS-11.3, RENOHARNESS-11.5_

---

### Task 4: Give a Reno attempt only the bot's key

**Files:**
- Modify: `crates/reno-harness/src/child_env.rs` (add `RENO` beside `PI` at line 139)
- Modify: `crates/reno-harness/src/drivers/pi/mod.rs` (`launch_pi`: the env order for `Reno`; unit tests)
- Modify: `crates/reno-harness/tests/fixtures/fake-pi.py`:
  - log whether `GOOGLE_APPLICATION_CREDENTIALS`, `AWS_SHARED_CREDENTIALS_FILE`, `AWS_CONFIG_FILE`,
    `PI_CODING_AGENT_DIR` and each provider key are present, never their values;
  - print a given secret value to stderr in a `scenario:leak-stderr` mode.
- Test: unit tests in `crates/reno-harness/src/child_env.rs` and in `drivers/pi/mod.rs`

**Reuse:** rung 2, in four places:
- `launch_pi` (`drivers/pi/mod.rs:623-707`);
- `ScratchDir` (`proc/scratch.rs:30-72`, 0700 and removed on drop);
- `mcp::write_private` (`mcp.rs:34-49`, 0600 and never overwritten) and `mcp::install` (`mcp.rs:52-70`);
- `child_env::restrict` with a new `DriverEnv` (`child_env.rs:46,359`), and the `launch::args` table
  (`wire/pi/launch.rs:35-58`).

**Interfaces:**
- Produces: `pub const RENO: DriverEnv`, with BASE names only.
- Set on a `Reno` child, in this order:
  - `restrict(RENO)`;
  - `GOOGLE_APPLICATION_CREDENTIALS`, `AWS_SHARED_CREDENTIALS_FILE` and `AWS_CONFIG_FILE`, each set to a missing path
    under `<scratch>/.reno/agent/`;
  - `apply_run_env(spec.env)`;
  - then Task 3's three variables.
- Consumes: `launch_pi(.., flavor)` and the layout (Task 3).

**Depends-on:** Task 3

- [ ] Write failing unit tests:
  - `child_env::allowed_from(RENO, env)`, with a constructed daemon env holding `ANTHROPIC_API_KEY`,
    `ANTHROPIC_OAUTH_TOKEN`, `PI_CODING_AGENT_DIR` and `HOME`, keeps `HOME` and drops the rest. No process env is
    mutated.
  - `launch_pi` for `Reno`, with the bot secret `OPENAI_API_KEY=sk-test-123456` in `spec.env`, gives a child that
    logs only that key, its own agent dir and the three blanks.
  - A secret named `GOOGLE_APPLICATION_CREDENTIALS` wins over its blank, while a secret named `PI_CODING_AGENT_DIR`
    does not win.
  - The key is absent from the logged argv.
  - `scenario:leak-stderr` leaves the secret value scrubbed in the stderr tail.
  - The `Owner` child's environment is unchanged.
- [ ] Run `cargo test -p reno-harness --lib child_env && cargo test -p reno-harness --lib drivers::pi`. Expect `RENO`
  to be unresolved, and the `Reno` child to see today's `PI` list.
- [ ] Implement `RENO` and the `Reno` env order in `launch_pi`.
- [ ] Run, and expect a pass.
- [ ] `git commit -m "feat(harness): launch the Reno harness with the bot's key only"`

_Requirements: RENOHARNESS-2.1, RENOHARNESS-2.2, RENOHARNESS-2.3, RENOHARNESS-2.4, RENOHARNESS-2.6_

---

### Task 5: The gate extension, and how its calls reach the ladder

**Files:**
- Modify: `crates/reno-harness/src/drivers/pi/gate.mjs` (replace Task 3's stub)
- Create: `crates/reno-harness/tests/fixtures/pi-gate-test.mjs`
- Create: `crates/reno-harness/tests/misc/pi_gate.rs`
- Modify: `crates/reno-harness/tests/misc/main.rs` (add `mod pi_gate;`)
- Create: `crates/reno-harness/src/wire/pi/subject.rs`
- Modify: `crates/reno-harness/src/wire/pi/mod.rs` (declare `subject`)
- Modify: `crates/reno-harness/src/wire/pi/frames.rs` (beside the builders at lines 30-90)

**Reuse:** none, new code (rung 7). Pi offers the hook (`tool_call`, ADR-0002) but no host protocol. The shape follows
the spike's `gate.mjs` (`S1-pi-gate.md`, local): a persistent connection and fail-closed returns.

Also (`subject`):

**Reuse:** rung 2. `wire::pi::normalize::action_class` (`wire/pi/normalize.rs:48-55`) is exactly the 3.8 class table.
`wire/claude/permission.rs:75` `subject()` is the template for paths and `raw`.

**Interfaces:**
- Produces, for the gate:
  - env `RENO_GATE_SOCKET` and `RENO_GATE_NONCE`;
  - the command `reno-gate`, with an empty handler;
  - lines `{"t":"hello","nonce"}` (at `session_start` with reason `startup`) and
    `{"t":"call","id","toolName","toolCallId","parentToolCallId","input"}`;
  - replies `{"t":"decision","id","decision":"allow"|"deny","reason"}`;
  - `MAX_OUTSTANDING = 16`, `MAX_WAITING = 256`.
- Produces, for the subject and the reply parser:
  - `pub(crate) fn subject(tool: &str, call_id: &str, parent: Option<&str>, input: &serde_json::Value) -> ApprovalSubject`;
  - `pub(crate) fn get_commands() -> serde_json::Value`;
  - `pub(crate) fn lists_extension_command(data: &serde_json::Value, name: &str) -> bool`. It reads
    `data["commands"]`, the `data` that `Run::query` returns.
- Consumes: none.

**Depends-on:** Task 3

- [ ] Write the failing node fixture, modelled on `tests/fixtures/pi-mcp-test.mjs`, and its Rust runner, modelled on
  `tests/misc/pi_mcp.rs`. Like that runner, it reports and skips when node is absent. The fixture loads `gate.mjs`
  with a stub `pi` API, plays a scripted Unix socket, and asserts:
  - the hello carries the nonce;
  - every `tool_call` is sent, nested ones with their `parentToolCallId`;
  - allow returns `undefined` with `input` untouched;
  - deny returns `{block:true, reason}` with the driver's reason;
  - a closed socket, a malformed line, a handler throw and no connection each block;
  - there is no timer of its own: with a reply withheld across 1000 event-loop turns (`setImmediate`, no wall clock),
    the hook is still pending, and then allows;
  - with 16 outstanding, the 17th call waits;
  - with 16 outstanding and 256 waiting, the 273rd call is blocked with "too many calls are waiting for Reno".
- [ ] Write failing table-driven unit tests for `subject`, one row per tool:
  - `bash` and `powershell` give `Command` with `input.command`;
  - `read`, `ls`, `grep` and `find` give `Read` with `input.path`, and with no path when it is omitted;
  - `edit` and `write` give `Edit` with `input.path`;
  - `mcp__x__y` gives `McpTool`;
  - `extension_tool` gives `Other`;
  - `raw` equals `{toolName, toolCallId, parentToolCallId, input}` verbatim.
- [ ] Write failing unit tests for `lists_extension_command`: true only for a `reno-gate` entry with
  `source: "extension"`. Confirm the field names against Pi `dist/modes/rpc/rpc-mode.js:540` before writing it.
- [ ] Run `cargo test -p reno-harness --test misc pi_gate && cargo test -p reno-harness --lib wire::pi`. Expect the
  stub to block everything, and `subject` / `lists_extension_command` to be unresolved.
- [ ] Implement `gate.mjs`, `subject` and the frames functions, sans-IO.
- [ ] Run, and expect a pass.
- [ ] `git commit -m "feat(harness): the Reno gate extension and the subjects its calls become"`

_Requirements: RENOHARNESS-3.2, RENOHARNESS-3.3, RENOHARNESS-3.4, RENOHARNESS-3.5, RENOHARNESS-3.8, RENOHARNESS-6.1, RENOHARNESS-6.2_

---

### Task 6: Arm before the first prompt

**Files:**
- Create: `crates/reno-harness/src/drivers/pi/gate.rs` (`#[cfg(unix)]`)
- Modify: `crates/reno-harness/src/drivers/pi/mod.rs`:
  - `launch_pi` binds the `GateHost` before spawning Pi;
  - `Run` (line 721) owns it;
  - `Run::start_up` (775-835) arms with `Run::query` (837);
  - remove Task 1's not-armed refusal.
- Modify: `crates/reno-harness/tests/fixtures/fake-pi.py`:
  - when `RENO_GATE_SOCKET` is set, read `FAKE_PI_GATE` at start-up, before any prompt:
    `ok | no-hello | never-connect | wrong-nonce | no-command | second-client`;
  - answer `get_commands` with `reno-gate` (source `extension`) unless the mode is `no-command`.
- Test: `crates/reno-harness/tests/pi.rs`, unit tests in `gate.rs`, `crates/reno-kernel/src/tests/routing.rs`

**Reuse:** rung 2 and rung 5:
- the approval open/answer/deny-all pattern of the Claude driver (`drivers/claude/mod.rs:1070,1089-1225`) and
  `exec::OpenRequests` (`exec/requests.rs:50`);
- `Run::query` (`drivers/pi/mod.rs:837`) for `get_commands`;
- the `Player::play` select loop (`mod.rs:952-1022`) gains one arm;
- `tokio::net::UnixListener` and `UnixStream::peer_cred` from the installed `tokio` (rung 5);
- the peer-uid check modelled on `reno-rpc`'s `UnixServer` (`crates/reno-rpc/src/transport.rs:33-125`).

**Interfaces:**
- Produces:
  - `GateHost::bind(path: &Path, nonce: &str) -> std::io::Result<GateHost>`;
  - `async fn GateHost::arm(&mut self, budget: Duration) -> Result<(), ArmError>`, with
    `enum ArmError { NoHello, WrongNonce, Timeout }`;
  - `ARMING_BUDGET: Duration = 30 s`, overridable by `with_arming_budget` (Task 1).
  - The listener is dropped and the socket unlinked after the first valid hello.
- Consumes: `get_commands` and `lists_extension_command` (Task 5); the layout and env (Tasks 3, 4).

**Depends-on:** Task 4, Task 5

- [ ] Write failing tests through `start` (`with_arming_budget(Duration::from_millis(500))`, with the mode set by
  `FAKE_PI_GATE` in `spec.env` and `RENO_BOT` set):
  - `ok` arms and the prompt is sent.
  - `no-hello` (connects, then closes) gives `NoHello`; `never-connect` gives `Timeout`; `wrong-nonce` gives
    `WrongNonce`; `no-command` fails on the command list. Each ends `Done{stop: Error}` before any prompt frame is
    logged, naming the check.
  - `second-client` is refused, and after arming, a connect to the socket path fails.
  - A resumed run arms again, with a new nonce.
- [ ] Write a failing unit test in `gate.rs`: a peer of another uid is refused, tested by injecting the uid
  comparison.
- [ ] Write a failing kernel test in `tests/routing.rs`: with `executors = ["reno", "pi"]` and a `MockExecutor` for
  `reno` whose stream ends `Done{stop: Error}` before any prompt, the dispatcher records no fallback step and the run
  ends failed.
- [ ] Run `cargo test -p reno-harness --test pi reno_arm && cargo test -p reno-harness --lib drivers::pi::gate && cargo test -p reno-kernel routing`.
  Expect "not armed in this build", or `GateHost` unresolved.
- [ ] Implement `GateHost::bind` and `arm`, and wire arming into `Run::start_up` for `Reno` (failures become
  `Startup::Failed`). Remove the Task 1 refusal.
- [ ] Run `cargo test -p reno-harness --test pi`, and expect a pass.
- [ ] `git commit -m "feat(harness): arm the Reno gate before the first prompt, failing the run otherwise"`

_Requirements: RENOHARNESS-5.1, RENOHARNESS-5.2, RENOHARNESS-5.3, RENOHARNESS-5.5, RENOHARNESS-11.1_

---

### Task 7: Every call goes to the ladder before it runs

**Files:**
- Modify: `crates/reno-harness/src/drivers/pi/gate.rs` (`next`, `decide`, the writer task and the open-call bound)
- Modify: `crates/reno-harness/src/drivers/pi/mod.rs`:
  - `Player::play` (lines 952-1022) gains the gate arm;
  - the `OpenRequests` payload `GatePayload { id: u64 }`;
  - `dismiss_all` (1112) drains gate requests.
- Modify: `crates/reno-harness/tests/fixtures/fake-pi.py`. As real Pi does, for each scripted tool it emits
  `tool_execution_start`, then sends the gate call and waits; on `allow` it runs the tool, and on deny it emits an
  error `tool_execution_end` carrying the reason. Siblings go one at a time. Scenarios: `gate-coverage`, `gate-200`,
  and `gate-flood` (a broken gate that sends 17 calls without waiting).
- Test: `crates/reno-harness/tests/pi.rs`

**Reuse:** rung 2 and rung 5:
- the approval open/answer/deny-all pattern of the Claude driver (`drivers/claude/mod.rs:1070,1089-1225`) and
  `exec::OpenRequests` (`exec/requests.rs:50`);
- `Run::query` (`drivers/pi/mod.rs:837`) for `get_commands`;
- the `Player::play` select loop (`mod.rs:952-1022`) gains one arm;
- `tokio::net::UnixListener` and `UnixStream::peer_cred` from the installed `tokio` (rung 5);
- the peer-uid check modelled on `reno-rpc`'s `UnixServer` (`crates/reno-rpc/src/transport.rs:33-125`).

**Interfaces:**
- Produces:
  - `async fn GateHost::next(&mut self) -> Option<GateCall>`, where
    `GateCall { id: u64, tool: String, call_id: String, parent: Option<String>, input: serde_json::Value, over_bound: bool }`;
  - `fn GateHost::decide(&mut self, id: u64, decision: Decision, reason: Option<String>)`;
  - `MAX_OPEN_GATE_CALLS = 16`;
  - request ids `gate-<id>`.
- Consumes: `subject()` (Task 5); `GateHost::bind` and `arm` (Task 6).

**Depends-on:** Task 5, Task 6

- [ ] Write failing tests (fake Pi in `FAKE_PI_GATE=ok`, decisions sent through `ExecHandle::decide` /
  `decide_with_message`):
  - `gate-coverage` raises one `NeedsApproval` per call: read, grep, find, ls, bash, edit, write, MCP, three siblings
    one after another, and a nested codemode call. Each has its action class (Task 5's table) and keeps the tool, the
    call id, the parent id and the input verbatim. Each `NeedsApproval` comes after its `Tool` event and before its
    `ToolResult`.
  - A denied `bash` leaves its canary file, and its `ToolResult` is an error with the reason.
  - An allowed call runs with its arguments unchanged.
  - `gate-200` runs 200 calls and ends at `agent_settled`, with no cap.
  - Per-turn `Usage` is still emitted.
  - A cancel denies every open gate request.
  - `gate-flood` gets its 17th call denied, and the run ends.
- [ ] Run `cargo test -p reno-harness --test pi reno_calls`, and expect no `NeedsApproval` to be raised.
- [ ] Implement `next`, `decide` and the writer task: a bounded channel on the `PiExecutor`'s `TaskTracker`, owned by
  `Run` and aborted in `teardown`. Add the `Player::play` arm (`subject` → `OpenRequests::open_approval` →
  `emit(NeedsApproval)`; `answers` → `decide`) and the drain on cancel.
- [ ] Run, and expect a pass.
- [ ] `git commit -m "feat(harness): hold each Reno-harness tool call for the kernel's approval ladder"`

_Requirements: RENOHARNESS-3.1, RENOHARNESS-4.1, RENOHARNESS-4.4, RENOHARNESS-8.1, RENOHARNESS-8.2, RENOHARNESS-11.2_

---

### Task 8: End the run when Reno loses its watch or a credential appears

**Files:**
- Modify: `crates/reno-harness/src/drivers/pi/mod.rs`:
  - `Player::play`: `next() == None`, and the `auth.json` check before raising and at arming;
  - `launch_pi` sets the client flag for `Reno`.
- Modify: `crates/reno-harness/src/drivers/pi/rpc.rs` (`Client::send` at line 41: the per-run `refuse_host_bash` flag)
- Modify: `crates/reno-harness/tests/fixtures/fake-pi.py` (scenarios `gate-close`, `gate-auth-json`,
  `gate-auth-empty` and `gate-auth-garbled`)
- Test: `crates/reno-harness/tests/pi.rs`

**Reuse:** rung 2 and rung 5:
- the approval open/answer/deny-all pattern of the Claude driver (`drivers/claude/mod.rs:1070,1089-1225`) and
  `exec::OpenRequests` (`exec/requests.rs:50`);
- `Run::query` (`drivers/pi/mod.rs:837`) for `get_commands`;
- the `Player::play` select loop (`mod.rs:952-1022`) gains one arm;
- `tokio::net::UnixListener` and `UnixStream::peer_cred` from the installed `tokio` (rung 5);
- the peer-uid check modelled on `reno-rpc`'s `UnixServer` (`crates/reno-rpc/src/transport.rs:33-125`).

**Interfaces:**
- Produces: `Client::refusing_host_bash(self) -> Client`; the diagnostics
  `"Reno lost its watch over the run: the gate's connection closed"` and
  `"a credential file appeared in the run's Pi agent directory (auth.json)"`.
- Consumes: `GateHost::next` (Task 7).

**Depends-on:** Task 7

- [ ] Write failing tests:
  - `gate-close`: the gate's connection drops mid-run, and the run ends `Done{stop: Error}` with the "lost its watch"
    reason.
  - `gate-auth-json`: a non-empty `auth.json` written into the agent dir ends the run at the next call, denying it.
  - `gate-auth-empty`: Pi's own `{}` and a missing file do not end it.
  - `gate-auth-garbled`: an unparsable `auth.json` ends it.
  - A `reno` run's client refuses `{"type":"bash"}`.
  - The settings deny-list still refuses `set_auto_compaction` on both flavors, and a `pi` client is otherwise
    unchanged.
- [ ] Run `cargo test -p reno-harness --test pi reno_watch`, and expect the run to go on after the close.
- [ ] Implement the three endings and the client flag.
- [ ] Run, and expect a pass.
- [ ] `git commit -m "feat(harness): fail a Reno-harness run when its gate goes or a stored credential appears"`

_Requirements: RENOHARNESS-2.7, RENOHARNESS-6.6, RENOHARNESS-7.2, RENOHARNESS-10.2_

---

### Task 9: The ladder answers gated subjects as it answers every executor

**Files:**
- Test: `crates/reno-kernel/src/tests/approvals.rs`, `crates/reno-kernel/src/tests/protect.rs`,
  `crates/reno-kernel/src/tests/secrets.rs`
- No production file changes. If a test fails, that is a design defect to raise, not a fix to slip in.

**Reuse:** rung 2:
- the ladder (`approval.rs:276-383`) answers gated calls as it answers every executor: committed before the harness
  is told, then journaled (`approval.rs:62-66`), with expiry as a deny (`:37-38`);
- the built-in protection (`protect.rs:52,65-78,131-146`) denies edits and recognised writes to any path with a
  `.reno` component. The launch puts every run-owned file under one, which is 7.1;
- redaction (`supervisor.rs:395-415`, `redact/values.rs:164-201`) scrubs `subject.raw` with the rest of the event;
- Pi's `Tool`, `ToolResult` and `Usage` normalisation (`normalize.rs:144-145,244`) records the calls that run (4.4);
- the budget (`crates/reno-kernel/src/budget.rs`) applies by bot, whatever the executor (8.3). Nothing adds a cap
  (8.1).

**Interfaces:**
- Consumes: subjects shaped like Task 5's `subject()` output, built in the test with the existing `MockExecutor` /
  `MockStep` and the `command(line)` helper (`tests/approvals.rs:27`).
- Produces: none.

**Depends-on:** Task 5

- [ ] Write table-driven tests, at `full_access` and at `supervised`:
  - A policy deny, a standing deny and a repeat denial refuse.
  - A policy allow and a standing allow allow.
  - `mcp__reno__report` is allowed and recorded in the approval history with no journal pair.
  - An `ask` rule asks at `full_access`.
  - The rest is allowed at `full_access` and asked at `supervised`.
  - An unanswered ask expires as denied.
  - The decision is committed before the harness is told, then `ApprovalResolved` is journaled.
  - A subject's `raw` holding a secret value is journaled redacted.
  - An edit of `<scratch>/.reno/gate.mjs`, an `rm` or a `>` redirect onto `<data>/harness/.reno/pi-sessions/x/s.jsonl`,
    and an edit of `.git/config` are denied by the built-in protection in every mode, before any allow.
  - A bot's `max_wall_time` still stops a run.
  - A bot that lists `reno` and a secret that is not set is refused at start, naming the secret
    (`dispatcher.rs:444-492`).
- [ ] Run `cargo test -p reno-kernel approvals && cargo test -p reno-kernel protect && cargo test -p reno-kernel secrets`,
  and expect a pass (these are guards). Record any failure as a design finding.
- [ ] `git commit -m "test(kernel): pin how the ladder and the built-in protection answer Reno-harness calls"`

_Requirements: RENOHARNESS-2.5, RENOHARNESS-3.6, RENOHARNESS-3.7, RENOHARNESS-4.2, RENOHARNESS-4.3, RENOHARNESS-6.3, RENOHARNESS-7.1, RENOHARNESS-7.4, RENOHARNESS-8.3, RENOHARNESS-10.3_

---

### Task 10: Keep a run's Pi session for its resumes, and only that long

**Files:**
- Modify: `crates/reno-harness/src/exec/mod.rs` (`Executor` trait, lines 96-190)
- Modify: `crates/reno-harness/src/drivers/pi/mod.rs` (`run_closed` and `retain_runs` for `Reno`; a per-run handle on
  the attempt task, for `run_closed` to await)
- Modify: `crates/reno-harness/tests/fixtures/fake-pi.py` (resolve the session file under `--session-dir` when given)
- Modify: `crates/reno-kernel/src/workspaces.rs` (`run_ended` at line 179)
- Modify: `crates/reno-kernel/src/recovery.rs` (one awaited step after step 2 and before step 5)
- Test: `crates/reno-kernel/src/tests/workspaces.rs` and `crates/reno-kernel/src/tests/recovery.rs`, each with a
  test-local recording `impl Executor` (as `tests/secrets.rs` defines its own); `crates/reno-harness/tests/pi.rs`

**Reuse:** rung 2. It hooks the kernel's one run-end seam, `workspaces::run_ended` (`crates/reno-kernel/src/workspaces.rs:179`),
which every end path calls: a supervised run's finish (`supervisor.rs:1006`), a cancel of a queued or interrupted run
(`kernel.rs:641`), and a run that never got a supervisor (`dispatcher.rs:1059`). Its sibling `close_if_unattended`
already reads whether the run ended for good. The sweep follows recovery's per-run file clearing through a port
(`recovery.rs:16-17`, `RunLink`).

**Interfaces:**
- Produces: `async fn Executor::run_closed(&self, run: &RunId) {}` and
  `async fn Executor::retain_runs(&self, keep: &HashSet<RunId>) {}`, both default no-ops.
- Consumes: `Flavor::Reno { sessions_root }` (Task 1) and the session dir layout (Task 3).

**Depends-on:** Task 3, Task 7

- [ ] Write failing tests:
  - The kernel, with the recording executor, calls `run_closed` once for each run that ends `Succeeded` or
    `Cancelled`, through finish, a queued cancel and an interrupted cancel, and never for `Failed` or `Interrupted`.
  - Recovery awaits `retain_runs` with `keep = {Failed, Interrupted, Queued with session_ref}` before it re-queues
    and before any claim.
  - Through `start` against the fake Pi, `run_closed` removes `<sessions_root>/<run>` only after the attempt's Pi has
    exited.
  - `retain_runs` removes unknown and closed dirs, and keeps the kept ones.
  - A second attempt with `spec.resume` passes the same `--session-dir <sessions_root>/<run_id>` (asserted on the
    fake's logged argv), and finds its session there with no "did not find session" notice.
- [ ] Run `cargo test -p reno-kernel workspaces && cargo test -p reno-kernel recovery && cargo test -p reno-harness --test pi reno_session`,
  and expect unresolved trait methods.
- [ ] Implement the two defaults and the `Reno` overrides. `run_ended` calls `run_closed` on a `shared.spawn` task;
  recovery calls `retain_runs` and awaits it.
- [ ] Run, then `cargo check --workspace --all-targets`, and expect a pass, with every other `Executor` impl unedited.
- [ ] `git commit -m "feat(kernel): remove a Reno-harness run's Pi session once it can never resume"`

_Requirements: RENOHARNESS-8.5, RENOHARNESS-11.4, RENOHARNESS-11.6_

---

### Task 11: The Reno harness keeps the process-group and detector guards

**Files:**
- Test: `crates/reno-harness/tests/pi.rs`
- No production file changes.

**Reuse:** (no design section: the design lists 6.4, 6.5 and 10.4 as guards met by unchanged code) rung 2. These
are the existing process-group spawn and orphan sweep (`drivers/pi/mod.rs:296-298`, `recovery.rs:5-8`) and the
after-the-fact detectors (`supervisor/protect.rs:1-30`). The kernel-side sweep and detector behavior is already
covered by the existing tests in `crates/reno-kernel/src/tests/recovery.rs` and `tests/protect.rs`; this task adds
only what is specific to `reno`.

**Interfaces:**
- Consumes: the `reno` and `pi` flavors (Tasks 1 and 6).
- Produces: none.

**Depends-on:** Task 6

- [ ] Write guard tests:
  - A `reno` run's Pi is spawned as its own process group, and that group is reported to `ExecControls::processes`
    at spawn, so the kernel's shutdown and the orphan sweep can end it.
  - Dropping the executor's run ends the group.
- [ ] Run `cargo test -p reno-harness --test pi reno_group`, and expect a pass. Record any failure as a design
  finding.
- [ ] `git commit -m "test(harness): pin the process-group guards for the Reno harness"`

_Requirements: RENOHARNESS-6.4, RENOHARNESS-6.5, RENOHARNESS-10.4_

---

### Task 12: Conformance and the live Pi run

**Files:**
- Modify: `crates/reno-harness/tests/conformance/targets.rs` (add `reno_target()` beside `pi_target()` at line 608)
- Modify: `crates/reno-harness/tests/conformance/main.rs`:
  - register `reno_target()` in `the_mock_and_every_driver_pass_the_suite`;
  - pin `labels("reno")` with `user-config` as `skip`;
  - keep the `labels("pi")` block at lines 111-122.
- Modify: `crates/reno-harness/tests/fixtures/fake-pi.py` (`scenario:approval-subjects`, reporting each tool's
  decision in the result text, as the Claude fake does)
- Modify: `crates/reno-harness/tests/fixtures/pi-rpc-probe.ts` (a scripted-tool mode: read, grep, find, ls, edit,
  write, bash, an MCP tool, and a codemode script with nested calls)
- Modify: `crates/reno-harness/tests/pi.rs` (an `#[ignore]` live test beside
  `a_real_pi_plays_a_run_against_its_mock_provider`, near line 1050)

**Reuse:** (no design section: this extends the design's Seams for testing rows for conformance and live Pi) rung 2.
`pi_target()` and the Claude target's `ApprovalPlan` (`targets.rs:608`; `scenario:approval-subjects`, targets.rs:289-300).
The existing live test's mock provider (`tests/fixtures/pi-rpc-probe.ts`) is loaded with `-e`, not through
`inherit_user_config`.

**Interfaces:**
- Consumes: the whole driver (Tasks 1-10).
- Produces: none.

**Depends-on:** Task 8, Task 10

- [ ] Write `reno_target()`:
  - every scenario sets `RENO_BOT` in its env (`Scenario::env`, targets.rs:321) and `FAKE_PI_GATE=ok`;
  - `user_config: None`;
  - `approval: Some(ApprovalPlan{ scenario: "scenario:approval-subjects" at AccessMode::Supervised, on_allow, on_deny, on_deny_message })`;
  - the exhaustive mode runs cover `Supervised` and `FullAccess` at `Unrestricted`.
- [ ] Write the live test (`RENO_PI_LIVE=1`, `-- --ignored`). Its wrapper exports only `PI_OFFLINE` and
  `PI_TELEMETRY` and passes `-e <pi-rpc-probe.ts>` before `"$@"`, so the gate stays the last `--extension`, and it
  never exports `PI_CODING_AGENT_DIR`. The spec sets `RENO_BOT`. It asserts:
  - a real Pi arms with its own `{}` `auth.json` present;
  - every scripted tool kind reaches a decision, codemode nested calls included;
  - the gate is the last `tool_call` handler loaded;
  - a planted non-empty `auth.json` ends the run;
  - killing Pi's process (its pid from the process record) and starting again with `spec.resume` continues the same
    session.
  - It reports, as a measurement with no bound, the time from each `NeedsApproval` it receives to that call's
    `ToolResult`, over 200 calls it allows at once.
- [ ] Run `cargo test -p reno-harness --test conformance the_mock_and_every_driver_pass_the_suite`, and expect a pass,
  with `reno` in the pinned labels. The owner runs the live test per `docs/KEEPING-UP.md`.
- [ ] `git commit -m "test(harness): conformance and live checks for the Reno harness"`

_Requirements: RENOHARNESS-3.2, RENOHARNESS-5.1, RENOHARNESS-5.4, RENOHARNESS-8.5, RENOHARNESS-2.7_

---

### Task 13: Say what Reno now does

**Files:**
- Modify: `README.md` (line 58)
- Modify: `apps/renod/README.md` (the executor list at line 119)
- Modify: `crates/reno-harness/src/drivers/pi/mod.rs` (docs at lines 37-38, 192-199 and 376)
- Modify: `crates/reno-kernel/src/supervisor/protect.rs` (docs at lines 3-9)
- Modify: `crates/reno-proto/src/defs/bot.rs` (docs at lines 97-100)
- Modify: `docs/KEEPING-UP.md` (the Pi section: a bump re-runs the gate matrix and the live test, and re-checks the
  reload paths and the ambient credential paths named in `design.md`)
- Test: `crates/reno-proto/src/defs/tests.rs` (a docs test reading `README.md` with `include_str!`)

**Reuse:** rung 2, by editing the existing text in place:
- `README.md:58`;
- `drivers/pi/mod.rs:37-38,192-199,376`;
- `supervisor/protect.rs:3-9`;
- `crates/reno-proto/src/defs/bot.rs:97-100`.

**Interfaces:** none.

**Depends-on:** Task 12

- [ ] Write a failing test: `README.md` does not contain "It never runs a model itself", and does contain "a harness
  of its own for bots" and "API keys".
- [ ] Run `cargo test -p reno-proto readme`, and expect it to fail on the old line.
- [ ] Rewrite:
  - the README line;
  - the renod executor list;
  - the Pi driver docs: Pi can block each call through an extension; Reno gates it only as `reno`; `pi` still asks
    nothing;
  - the `protect.rs` header: `pi` stays among the harnesses that cannot ask; `reno` asks before every call;
  - the `inherit_user_config` docs: which executors isolate, as their descriptors declare it, Pi and `reno` included;
  - the `KEEPING-UP.md` checklist.
- [ ] Check by reading that the Pi driver docs and the `protect.rs` header say what RENOHARNESS-9.2 says, and the
  `inherit_user_config` docs what 9.3 says (manual acceptance step).
- [ ] Run the test, and expect a pass. Then run the close: the full verify list in `docs/agents/project.md`.
- [ ] `git commit -m "docs: Reno has a harness of its own for bots, on your API keys"`

_Requirements: RENOHARNESS-9.1, RENOHARNESS-9.2, RENOHARNESS-9.3_
