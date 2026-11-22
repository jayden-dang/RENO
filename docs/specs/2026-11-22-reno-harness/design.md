# Design: Bots run on Reno's own harness, with every tool call seen by Reno

Feature code: RENOHARNESS
Status: Approved
Approval: Approved by the operator (Jayden Đặng), 2026-11-22, after reading the spec sheet at 02afd14: "approve"
Date: 2026-11-22
Requirements: ./requirements.md

## Context

Reno drives other agent CLIs, and today the Pi executor `pi` runs the owner's own Pi at full access only.
- It refuses every other mode before it spawns (`crates/reno-harness/src/drivers/pi/mod.rs:376-378`, `:472-473`).
- It loads the owner's credentials through a daemon-environment allowlist (`crates/reno-harness/src/child_env.rs:139-148`).
- It never raises an approval.

The kernel's approval ladder (`crates/reno-kernel/src/supervisor/approval.rs:276-383`) answers every other driver.
Each driver raises `ExecEvent::NeedsApproval` and reads the answer from `ExecControls::answers`
(`crates/reno-harness/src/exec/mod.rs:231-262`). ADR-0002 chose Pi as the Reno harness, because a Pi extension on
its `tool_call` hook reached Reno for every call in the spike and failed closed.

The binding constraint is L11 (`docs/architecture/runtime.md:96`): every call goes to the kernel's own policy,
inbox and journal, the same ones every run uses. The project's posture adds a second: the wire protocol and the
store carry the owner's state, so a change to either needs a bridge (`docs/agents/project.md:24`). Together they
rule out two alternatives:
- a gate that judges calls itself in JS, because policy would then live outside the kernel;
- a new `agent.*` wire method for the gate, because that is a wire addition and a bridge for nothing the driver
  cannot already do.

So the gate talks only to the Pi driver, over a private per-run Unix socket. The driver turns each call into the
`NeedsApproval` it would raise for any harness that asks.

The fresh retrieval (`load-subgraph`, recipe fsubr-1.2, `neighbors(RENOHARNESS)`) found no neighbor. OWNS coverage
is 0/10, and no CODE has a `tasks.md`. The observations are four unowned clusters over `crates/reno-kernel/src/`,
`crates/reno-harness/src/drivers/`, `reno-harness/wire` and `reno-harness/tests`. So no `Reuse:` below comes from
the catalog; each comes from the design scan (`.skills/_pending-reno-harness/auto/design-scan.md`, outside the
repo). Spine invariants relied on: ARCH-6, ARCH-7, ARCH-8, ARCH-9, ARCH-10, ARCH-11, ARCH-13, ARCH-15, ARCH-16 and
ARCH-18.

## Decisions

1. The Reno harness is a second executor, `reno` [agent-decided id], served by the Pi driver module with a
   `Flavor::Reno` setting. `pi` stays `Flavor::Owner`, unchanged [Weigh W1]. Recorded in ADR-0003.
2. The gate is a JS extension shipped inside the binary (`include_str!`), like the MCP bridge. It talks to the
   driver over a per-run Unix socket with a one-line JSON protocol. No wire method and no `ExecEvent` variant is
   added.
3. Arming happens inside the driver's start-up, before the first prompt. Pi must list the gate's command, and the
   socket's one connection must say hello with the attempt's nonce. A failure is the attempt's own
   `Done{stop: Error}` (today's launch-failure path, `drivers/pi/mod.rs:517-529`), which the dispatcher never falls
   back from [Weigh S1].
4. Run-owned files live under a `.reno` path component, so the existing built-in protection
   (`crates/reno-proto/src/defs/protect.rs:53,131-146`) denies edits and recognised writes to them before any allow,
   in every mode. No proto change is needed [Weigh S3].
5. Pi's session lives in `<data_dir>/harness/.reno/pi-sessions/<run_id>/` (0700), passed with `--session-dir`. It
   is removed when the run is `succeeded` or `cancelled`, and swept at start [Weigh W6-W8].
6. Two default-no-op methods on the `Executor` trait let the kernel tell an executor that a run is closed, and at
   start which runs to keep. Only `reno` overrides them, so the kernel stays free of I/O (ARCH-7).
7. `inherit_user_config = true` excludes `reno` at routing with the existing `RejectReason::Excluded { why }`. A
   bot run that the exclusion leaves with no executor fails with a message naming the setting [Weigh W5].
8. The ladder, the policy, standing rules and the stores are unchanged.

## Architecture

Vocabulary for every section below: **module**, **interface**, **implementation**, **seam** (a public surface
where behavior is observable and substitutable). Prefer deep modules, whose interface is much simpler than what it
hides.

```
 bot run ──► router (eligibility) ──► dispatcher ──► PiExecutor{Flavor::Reno}.start(spec)
                                                        │ launch: scratch/.reno/{gate.mjs, agent/, bridge, gate.sock}
                                                        │ pi --mode rpc … --session-dir <data>/harness/.reno/pi-sessions/<run>
                                                        │        -e builtin:mcp -e builtin:codemode --extension bridge --extension gate
                                                        ▼
                     Pi ── tool_call hook ── gate.mjs ══ unix socket (1 conn, nonce) ══ GateHost (driver)
                                                                                         │ subject() (wire/pi, sans-IO)
                                                                                         ▼
                                       ExecEvent::NeedsApproval ──► supervisor ladder (unchanged) ──► ExecAnswer
                                                                                         │
                     Pi ◄── allow / {block, reason} ◄── gate.mjs ◄══ decision line ═══════┘
```

### Executor identity and registration

Satisfies: RENOHARNESS-1.1, RENOHARNESS-1.2, RENOHARNESS-1.7, RENOHARNESS-10.6, RENOHARNESS-10.8
Reuse: rung 2. `PiExecutor` (`drivers/pi/mod.rs:200-269`) gains a `Flavor`. Registration is one more entry in
`discover` (`apps/renod/src/executors.rs:249-274`). `BUILTIN_EXECUTORS` (`crates/reno-proto/src/defs/mod.rs:120-133`)
gains `"reno"`.
Respects: ARCH-15
Surface:
- `PiExecutor::new()` and its callers in `executors.rs`, tests and conformance: **replace**. It keeps returning the
  `Owner` flavor, so callers need no edit.
- The persisted `runs.executor` value and the routing JSON: **frozen**. The design builds around them: `reno` is a
  new value in a string column that already holds any executor id, and old readers show the id as written.
- Bot definitions that list `reno`: **frozen**. They are read only by a daemon that has `reno` in
  `BUILTIN_EXECUTORS`. An older daemon reports an unknown executor, as it does today for any id it lacks.
- `discover(hold_mock, stream_mock, harnesses)` and its callers in `apps/renod/src/daemon.rs` (the call site changes)
  and its tests:
  **replace**. It gains `data_dir: &Path`; every call site in the repo is updated in this change.
- `BUILTIN_EXECUTORS` readers (`ValidationContext::with_builtin`/`offline`, `defs/mod.rs:338-353`): **replace**.
  The list gains one id, and the readers are unchanged.
- `executor list` output, wire `ExecutorDescriptor` and the app's executor lists: **replace**. One more descriptor
  arrives, of a shape that already exists, so no wire change. The app names it "Reno" by its generic rule
  (`apps/reno-app/src/summary.rs:272-287`).

Interface:
```
pub const RENO_EXECUTOR_ID: &str = "reno";
impl PiExecutor { pub fn reno(sessions_root: PathBuf) -> Self }
// descriptor(): id "reno", caps.access_modes [Supervised, FullAccess], sandbox_levels [Unrestricted],
//               resume true, isolates_user_config true; cli_verified as pi
// probe(): installed + version as pi; signed_in Unknown; models curated [agent-decided] (never the owner's credentials)
// maintenance(): None (the CLI's update and sign-in rows stay on `pi`)
```
Depth: n/a. This extends `PiExecutor`.
Locality: extend `drivers/pi/mod.rs` (the flavor branches in `descriptor`, `probe`, `start` and `maintenance`),
`apps/renod/src/executors.rs` (one entry and the data dir) and `defs/mod.rs` (one id). Nothing else moves.

`Flavor::Owner` keeps every line of today's behavior. `Flavor::Reno` branches only where this design says.

### Eligibility and refusals before spawn

Satisfies: RENOHARNESS-1.4, RENOHARNESS-1.5, RENOHARNESS-1.6, RENOHARNESS-10.5
Reuse: rung 2, in three places:
- the router's caps check (`crates/reno-kernel/src/router.rs:94-119`) gives 1.4 from the descriptor alone;
- the `unfit_effort` slot (`router.rs:74,226`; filled in `kernel.rs:1575-1597`) is the template for 1.5;
- `ExecError::InvalidSpec`, which maps to `StartRefusal::Invalid` with no fallback (`dispatcher.rs:639`), gives 1.6.
Respects: ARCH-15, ARCH-13
Surface:
- `RouteRequest` (`router.rs:59-76`) and every one of its construction sites (`plan_route` at `kernel.rs:1586-1597`,
  and the router and kernel tests, `router.rs:345` and others; the list is a `rg 'RouteRequest \{'` at build time):
  **replace**. It gains `unfit_config: Vec<(ExecutorId, String)>`, and every site sets it.
- The excluded-only failure text (`dispatcher.rs:828-845`): **replace**. For a run with a bot, it reads
  `no executor this bot lists can take the task (<explain_verbose>)`. The "name an executor or use a bot" advice
  stays only for a run with no bot.
- The persisted `run_routing.decision` JSON and the wire `RoutingDecision`: **frozen**. The design builds around
  them: only the existing `Excluded { why }` variant is written, so old readers read it.

Interface:
```
// plan_route: if def.inherit_user_config { unfit_config.push((reno, "bot <name> sets inherit_user_config = true, which the Reno harness never loads")) }
// router: unfit_config hit -> RejectReason::Excluded { why }, for a listed executor and for an explicit `--executor reno`
//         alike; the failure text then names the setting
// PiExecutor{Reno}.start: no RENO_BOT in spec.env -> ExecError::InvalidSpec("the Reno harness runs bots only: name a bot")
//                         spec.inherit_user_config -> ExecError::InvalidSpec(...)   // for resumed runs, which skip routing
```
Depth: n/a. This extends the router and `start`.
Locality: extend `router.rs`, `kernel.rs` (`plan_route`), `dispatcher.rs` (one message) and `drivers/pi/mod.rs`
(`start`). The router tests extend.

The second `InvalidSpec` check covers a resumed run. Such a run skips routing (`resume.rs`), and its pinned bot
version was routed once already, so the check only defends against a definition that changed under a pin.

### Launching an attempt

Satisfies: RENOHARNESS-1.3, RENOHARNESS-2.1, RENOHARNESS-2.2, RENOHARNESS-2.3, RENOHARNESS-2.4, RENOHARNESS-2.5,
RENOHARNESS-2.6, RENOHARNESS-5.4, RENOHARNESS-5.5, RENOHARNESS-7.3, RENOHARNESS-7.5, RENOHARNESS-8.4,
RENOHARNESS-10.1, RENOHARNESS-11.3
Reuse: rung 2, in four places:
- `launch_pi` (`drivers/pi/mod.rs:623-707`);
- `ScratchDir` (`proc/scratch.rs:30-72`, 0700 and removed on drop);
- `mcp::write_private` (`mcp.rs:34-49`, 0600 and never overwritten) and `mcp::install` (`mcp.rs:52-70`);
- `child_env::restrict` with a new `DriverEnv` (`child_env.rs:46,359`), and the `launch::args` table
  (`wire/pi/launch.rs:35-58`).
Respects: ARCH-6, ARCH-15
Surface:
- `Launch` construction sites, `launch_pi` and the `Owner` probe (`drivers/pi/mod.rs:285`): **replace**, with the
  new fields left at their defaults.
- `Launch` and `args` (`wire/pi/launch.rs`) and the pinned flag test (`tests/pi.rs`
  `the_command_line_isolates_the_owners_tools_and_trusts_nothing`): **replace**. `Launch` gains
  `session_dir: Option<&Path>`, `builtins: bool` and `gate: Option<&Path>`. With `Owner` they are `None`/`false`,
  so the arguments are byte-identical and the test still pins them.
- `child_env` lists: **replace**. A new `RENO` list is added, and `PI` is untouched.

Interface:
```
// layout, per attempt:  <scratch>/.reno/{gate.mjs, gate.sock, agent/, reno-mcp.mjs, mcp-servers.json, instructions.md}
// argv (Reno):  --mode rpc --no-themes --no-extensions --no-skills --no-prompt-templates --no-approve
//               --session-dir <sessions_root>/<run_id> [--session-id <id>] [--append-system-prompt <file>]
//               -e builtin:mcp -e builtin:codemode --tools +codemode [--extension <bridge>] --extension <gate>   (gate last)
// env (Reno):   restrict(RENO)  — BASE only: no PI_* and no provider variable from the daemon
//               then the ambient-credential blanks: GOOGLE_APPLICATION_CREDENTIALS, AWS_SHARED_CREDENTIALS_FILE and
//               AWS_CONFIG_FILE, each pointed at a missing file in <scratch>/.reno/agent/ (a bot secret of the
//               same name, set next, wins)
//               then apply_run_env(spec.env)  — the bot's secrets, RENO_RUN_ID, RENO_BOT
//               then PI_CODING_AGENT_DIR=<scratch>/.reno/agent, RENO_GATE_SOCKET, RENO_GATE_NONCE (set last, so a
//               secret cannot override them)
```
Depth: n/a. This extends `launch_pi`.
Locality: extend `drivers/pi/mod.rs` (`launch_pi`), `wire/pi/launch.rs` (three fields) and `child_env.rs` (one
list). `mcp.rs` is unchanged; it writes into whatever directory it is given.

Details:
- **Fresh files per attempt (7.5).** Each attempt is a fresh `start`, so it gets a fresh `ScratchDir`, gate file,
  agent dir, socket and nonce (a uuid v4). The 5.5 re-arm comes for free.
- **Session dir (8.4).** It is created by the launch if absent, with mode 0700. The run id is the same across
  attempts (`dispatcher.rs:393`), so a resume's `--session-id` finds the session.
- **Project trust.** Even without the flag, Pi treats a project as untrusted in RPC mode, and built-in MCP reads
  config only at `session_start` (Pi `extensions/mcp/index.js:971`). `--no-approve` makes Pi ignore the working
  folder's `.pi/` resources (Pi `cli/args.js:232,335`;
  `docs/security.md:37-42`). The explicit `--session-dir` outranks a project `sessionDir` setting, which Pi reads before
  trust (`docs/security.md:31`). `AGENTS.md` context files still load, as they do for `pi`: `--no-approve` sets `projectTrustOverride = false` (Pi
  `cli/args.js:232-233`), and Pi's trust-protected list does not include context files (`docs/security.md:37-42`).
- **Built-in MCP.** `--no-mcp` is not passed on `Reno`. With an agent dir that holds no `mcp.json`, Pi's built-in
  MCP loads no server, and Reno's bridge is the only source of MCP tools.
- **Codemode.** Pi 1.5.0 registers the codemode tool inactive by default (Pi `extensions/codemode/index.js:28`,
  `defaultActive: false`), so the launch passes `--tools +codemode` (Pi `cli/args.js:398-399`) [agent-decided] to give
  the model the tool. Its nested calls are gated one by one.
- **Removal (11.3).** `ScratchDir`'s drop removes the per-attempt files at the attempt's end, however it ends,
  including when Pi is killed. A daemon crash leaves them for the OS temp cleaner, and they hold no secret: the key
  is only in Pi's environment.
- **Socket path.** It is about 82 bytes on this machine (design-scan §5), against the macOS `sun_path` limit of
  104 bytes (`char sun_path[104]`, the macOS SDK's `sys/un.h:79`). When `<scratch>/.reno/gate.sock` would be 104 bytes or more (a long `TMPDIR`), the socket goes to a short
  owner-only directory, `/tmp/reno-gate-<uuid>/.reno/gate.sock` (0700, removed with the attempt). If even that is
  too long, the attempt fails. The path is never truncated.
- **Ambient cloud credentials (2.1).** `BASE` keeps `HOME`, and Pi's providers find file-based credentials under it:
  Vertex through `GOOGLE_APPLICATION_CREDENTIALS`, else `~/.config/gcloud/application_default_credentials.json`
  (pi-ai `env-api-keys.js:56-67`), and Bedrock through the AWS shared files under `~/.aws`. The blanks above point
  each at a missing file, so Pi finds none, and a bot that wants one lists it as a secret. A model-run `aws` or
  `gcloud` command sees the same blanks. Other ambient paths in a future Pi are re-checked when `cli_verified`
  moves.
- **Extension order (5.4).** Pi loads `builtin:` extensions first, whatever their place on the command line
  (Pi `core/package-manager.js:776-780`), and then each `--extension` in argument order. So the gate, the last
  `--extension`, is the last loaded. The live test asserts the order.
- **`rg` and `fd`.** Pi's `grep` and `find` use the system `rg` and `fd` when they are on `PATH` (Pi
  `utils/tools-manager.js:63-79`), and `PATH` passes through `BASE`. Only when they are missing does Pi download them
  into the attempt's fresh agent dir, once per attempt.

### The gate extension

Satisfies: RENOHARNESS-3.2, RENOHARNESS-3.3, RENOHARNESS-3.4, RENOHARNESS-3.5, RENOHARNESS-6.1, RENOHARNESS-6.2
Reuse: none, new code (rung 7). Pi offers the hook (`tool_call`, ADR-0002) but no host protocol. The shape follows
the spike's `gate.mjs` (`S1-pi-gate.md`, local): a persistent connection and fail-closed returns.
Interface:
```
env:      RENO_GATE_SOCKET, RENO_GATE_NONCE
command:  registers "reno-gate" (so get_commands lists it with source "extension")
gate → driver, one JSON object per line:
  {"t":"hello","nonce":<str>}                                   at session_start
  {"t":"call","id":<n>,"toolName":<str>,"toolCallId":<str>,"parentToolCallId":<str|null>,"input":<object>}
driver → gate:
  {"t":"decision","id":<n>,"decision":"allow"|"deny","reason":<str|null>}
hook returns: allow -> undefined (input untouched);  anything else -> {block: true, reason}
```
Depth: deletion test. If the gate vanished, a caller would still need to know only the three line shapes above and
"anything but allow blocks". The implementation hides the connection, the waits, the outstanding cap and the
failure paths.
Respects: ARCH-11, ARCH-18
Locality: new file `crates/reno-harness/src/drivers/pi/gate.mjs`, installed like `mcp.mjs`. Nothing existing
moves.

Rules the file implements:
- Every `tool_call` event is sent, whatever the tool (3.2). The hook never edits `event.input` (3.5). Under codemode,
  nested calls arrive through the same hook with their `parentToolCallId`.
- Blocking happens on a deny (3.3, with the driver's `reason` as the block reason, 3.4), and also when:
  - there is no connection, or the socket errors or closes;
  - a line is malformed;
  - the handler throws (6.1).
- No timer of its own (6.2). The kernel always answers: an expiry is a deny at the TTL, and a cancel denies
  everything open (`approval.rs:702,797-832`). So the gate waits until the decision arrives or the socket closes.
- At most `MAX_OUTSTANDING` (16) [agent-decided] calls are sent unanswered. Further calls wait inside the gate, so
  the driver's bound (11.2) is never reached by a working gate. That wait queue is bounded too, at `MAX_WAITING`
  (256) [agent-decided]. A call beyond it is blocked at once, with the reason "too many calls are waiting for Reno",
  so the gate holds a bounded amount of state whatever the model asks for (ARCH-11).
- No reconnect. After the socket is lost, every call blocks until Pi exits.

### The gate host (in the driver)

Satisfies: RENOHARNESS-2.7, RENOHARNESS-2.8, RENOHARNESS-2.9, RENOHARNESS-3.1, RENOHARNESS-5.1, RENOHARNESS-5.2, RENOHARNESS-5.3, RENOHARNESS-6.6, RENOHARNESS-7.2,
RENOHARNESS-8.2, RENOHARNESS-11.1, RENOHARNESS-11.2
Reuse: rung 2 and rung 5:
- the approval open/answer/deny-all pattern of the Claude driver (`drivers/claude/mod.rs:1070,1089-1225`) and
  `exec::OpenRequests` (`exec/requests.rs:50`);
- `Run::query` (`drivers/pi/mod.rs:837`) for `get_commands`;
- the `Player::play` select loop (`mod.rs:952-1022`) gains one arm;
- `tokio::net::UnixListener` and `UnixStream::peer_cred` from the installed `tokio` (rung 5);
- the peer-uid check modelled on `reno-rpc`'s `UnixServer` (`crates/reno-rpc/src/transport.rs:33-125`).
Respects: ARCH-6, ARCH-9, ARCH-10, ARCH-11, ARCH-18
Surface:
- `rpc::Client::send` (`drivers/pi/rpc.rs:41`) and the `no_run_ever_sends_a_command_that_writes_pis_settings` test:
  **replace**. The client gains a per-run `refuse_host_bash` flag, set only for `Reno` (7.2). The settings deny-list
  itself is unchanged (10.2), and `Owner` runs send exactly what they send today.
- `wire/pi/frames.rs`: **replace**. It gains a `get_commands()` builder and a reply parser. No existing builder
  changes.

Interface (inside the driver):
```
GateHost::bind(path, nonce) -> GateHost             // 0600 socket in the 0700 .reno dir
GateHost::arm(&mut self, budget) -> Result<(), ArmError::{NoHello, WrongNonce, Timeout}>
                                                    // accepts exactly one connection (peer uid = owner);
                                                    // others are closed, giving no decision
GateHost::next(&mut self) -> Option<GateCall>       // None = connection gone
GateHost::decide(&mut self, id, Decision, Option<String>)
```
Depth: deletion test. If the gate host vanished, the driver would still need only "bind, arm, next call, decide".
The host hides the socket, the uid check, the nonce, the listener's one-time accept, the open-call bound and the line
framing.
Locality: new module `crates/reno-harness/src/drivers/pi/gate.rs`, owned by `Run` (`mod.rs:721`). The extensions
to `Run::start_up` and `Player::play` are listed below.

Flow:
- **Arming, in `Run::start_up`** (`mod.rs:775-835`), for `Reno` only.
  - After Pi is up and before the first `prompt`, `GateHost::arm` waits for the one connection and its hello. In
    the same window, `Run::query(get_commands)` must list `reno-gate` with source `extension` (5.1). Both are
    awaited as events, never polled (ARCH-10), within `ARMING_BUDGET` (30 s [agent-decided], inside `STARTUP_BUDGET` of 120 s, `drivers/pi/mod.rs:108`).
  - A failure is `Startup::Failed("Reno's gate did not arm: <check>")`, which becomes `Done{stop: Error}` (5.2). The
    dispatcher sees a stream, not an `ExecError`, so it never falls back (5.3).
  - The gate says hello once, at `session_start` with reason `startup`. The listener accepts until the first
    connection with a valid hello. Then it is dropped and the socket file unlinked, before the first prompt, so no
    later connection exists to accept. A connection before that with a wrong or missing nonce is closed, and arming
    fails (11.1).
- **Calls, in `Player::play`.**
  - A new select arm reads `GateHost::next()`. Each call becomes `subject()` (next section), then
    `OpenRequests::open_approval`, then `emit(NeedsApproval{request_id: "gate-<id>", subject})` (3.1).
  - The existing `answers.recv()` arm resolves `Decision(Allow|Deny, message)`, then
    `GateHost::decide(id, .., message)`.
  - A cancel, or the end of the run, denies all open gate requests (`drain`) before Pi's process group goes, as
    `dismiss_all` does for dialogs.
- **No usable model (2.8, 2.9).** In `Run::set_up`, for `Reno` only, when the spec names no model, the host sends one
  `get_available_models`. Pi answers with its snapshot of the models that have auth (Pi `rpc-mode.js:381-383`). An
  empty list fails the run before the first prompt with `"Pi has no model from this bot's secrets (<secret names>);
  Pi reads provider keys such as <names from child_env::PI>"`. Values are never printed (2.6), and the variable
  list is hint text only. A named model is already resolved against that list, and fails, by the existing
  `set_up` (`drivers/pi/mod.rs:812-818`) (2.9).
- **Credential check (2.7).** At arming, and before each call is raised, the host reads `<agent dir>/auth.json`.
  A missing file, or one that parses to an empty JSON object, is clean. Pi writes `{}` itself when the file is missing
  (Pi `core/auth-storage.js:40-43,62-64,314-322`). Anything else, an unparsable file included (fail closed), is a
  stored credential. Then it denies the waiting call, drains every open request, and ends the run
  with `Done{stop: Error, diagnostic: "a credential file appeared in the run's Pi agent directory (auth.json)"}`. The
  check is in the driver rather than in `gate.mjs`, because the driver is out of the run's reach. This keeps the
  semantics of the interpret pick (Weigh) and moves only where it runs.
- **Bound (11.2).** `GateHost` counts open calls. More than `MAX_OPEN_GATE_CALLS` (16) [agent-decided] means a broken gate:
  `next()` returns the excess call already denied, and the host closes the connection, which is 6.6. `decide` never
  blocks: it queues onto a bounded channel of `MAX_OPEN_GATE_CALLS` lines, drained by one writer task. That task is
  spawned on the `PiExecutor`'s `TaskTracker` (`drivers/pi/mod.rs:200-214`), owned by the attempt's `Run`, and
  aborted in `teardown` (ARCH-11). `next()` is cancel-safe, because it sits in `Player::play`'s `select!`.
- **Lost connection (6.6).** `GateHost::next() == None` while the run goes on ends it as
  `Done{stop: Error, diagnostic: "Reno lost its watch over the run: the gate's connection closed"}`. A person may
  resume it, and resume re-arms.
- **Run end (8.2).** Unchanged: `agent_settled` ends the run, and the stop reason comes from the last assistant
  message (`mod.rs:21,1193`).
- **The nonce is readable.** It is in Pi's environment, so a model-run command can read it. What makes that safe is
  the single-connection rule: the gate's connection is accepted before any prompt is sent, and no later connection
  is accepted. A process that learns the nonce can reach only a listener that no longer accepts.

### Reload paths in RPC mode (checked)

This section is not a module. It records the check that RENOHARNESS's Out of Scope asks design to make, by a read
of the installed Pi 1.5.0 source (`/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist`):
- **What reloads, and how it is reached.** Settings (`settingsManager.reload()`) and extensions (`_resourceLoader.reload()`)
  reload only through `AgentSession.reload()` (`core/agent-session.js:3016-3048`). In RPC mode that is reachable only
  as `ctx.reload()` from an extension command's context (`modes/rpc/rpc-mode.js:252-253`).
- **Which commands a prompt can run.** In RPC mode a prompt that starts with `/` runs only extension commands
  (`core/agent-session.js:1658,1716-1727`). The built-in `/reload` is handled by interactive mode only
  (`BUILTIN_SLASH_COMMANDS` is read only by `modes/interactive/interactive-mode.js`).
- **The commands of the extensions a Reno attempt loads.**
  - Built-in MCP's `/mcp` logs in, logs out or reconnects, and never calls `ctx.reload()`
    (`extensions/mcp/index.js:1121-1198`).
  - Codemode registers no command.
  - Reno's bridge registers none. The gate's `reno-gate` command has an empty handler.
- **Who sends prompts.** A model cannot send a prompt. Prompts and steers come from Reno: the task, the resume
  prompt and the owner's steering.
- **Result:** no reload of settings or extensions is reachable by a run within an attempt. So a file written into
  the agent dir cannot displace the gate before the attempt ends, and RENOHARNESS-7.5 covers the next attempt.
- **What a run can still affect.** Pi re-reads `auth.json` when its file revision changes
  (`core/auth-storage.js:283-284`), and a stored `auth.json` outranks the environment key (Pi `docs/models.md:22`).
  A run that writes one into its agent dir by a means the protection does not recognise, for example a copy of the
  owner's `~/.pi/agent/auth.json`, would put the attempt on that credential, which can be a subscription login
  (against L7). The gate host's `auth.json` check (RENOHARNESS-2.7) ends the run at the next call. One model turn may
  already have used the credential; that residual is in the Out of Scope gap that ROAD-63 and MILE-12 close.
- **Re-checked when `cli_verified` moves.** The check is repeated whenever `VERIFIED_CLI_VERSION` is raised, beside
  the gate matrix.

### Subjects from Pi's calls

Satisfies: RENOHARNESS-3.8, RENOHARNESS-4.1
Reuse: rung 2. `wire::pi::normalize::action_class` (`wire/pi/normalize.rs:48-55`) is exactly the 3.8 class table.
`wire/claude/permission.rs:75` `subject()` is the template for paths and `raw`.
Respects: ARCH-6
Interface:
```
pub(crate) fn subject(tool: &str, call_id: &str, parent: Option<&str>, input: &Value) -> ApprovalSubject
  action  = action_class(tool)
  command = input.command          for Command
  paths   = input.path when present, for Edit and Read (Pi's tools name it `path`; `grep`, `find` and `ls` may omit it,
            meaning the working dir, so paths is empty). Paths are kept as written, like every driver. `find`'s
            `pattern` and `grep`'s `glob` are not paths and stay only in `raw` (part of the lexical-path residual).
  raw     = {toolName, toolCallId, parentToolCallId, input}   (verbatim; the supervisor's redactor scrubs it)
```
Depth: n/a. This extends `wire/pi`.
Locality: new function in `wire/pi/` (sans-IO, beside `normalize.rs`). No neighbor changes.

### The ladder, protection and recording (unchanged code, relied on)

Satisfies: RENOHARNESS-3.6, RENOHARNESS-3.7, RENOHARNESS-4.2, RENOHARNESS-4.3, RENOHARNESS-4.4, RENOHARNESS-6.3,
RENOHARNESS-7.1, RENOHARNESS-7.4, RENOHARNESS-8.1, RENOHARNESS-8.3, RENOHARNESS-10.3, RENOHARNESS-10.4
Reuse: rung 2:
- the ladder (`approval.rs:276-383`) answers gated calls as it answers every executor: committed before the harness
  is told, then journaled (`approval.rs:70-72`), with expiry as a deny (`:41-42`, `expire_due` at `:703`);
- the built-in protection (`protect.rs:53,65-78,131-146`) denies edits and recognised writes to any path with a
  `.reno` component. The launch puts every run-owned file under one, which is 7.1;
- redaction (`supervisor.rs:395-415`, `redact/values.rs:164-201`) scrubs `subject.raw` with the rest of the event;
- Pi's `Tool`, `ToolResult` and `Usage` normalisation (`normalize.rs:144-145,244`) records the calls that run (4.4);
- the budget (`crates/reno-kernel/src/budget.rs`) applies by bot, whatever the executor (8.3). Nothing adds a cap
  (8.1).
Respects: ARCH-8, ARCH-9, ARCH-18
Interface: none new.
Depth: n/a. This reuses the shipped ladder.
Locality: leave. No edit lands in these files. `supervisor/protect.rs` gets a doc change only (9.2).

Notes:
- **Reno's own MCP tools.** Step 6 records them in the approval history, not as a journal pair (`approval.rs:348-359`),
  which is RENOHARNESS-4.3's exception to 4.1.
- **Inherited behavior.** When the journal write after a decision fails, the run is cancelled, but an allow already
  committed to the approval history may reach the gate (`approval.rs:526-535`). The decision is on record in the
  store; this road does not change it.
- **Paths are judged lexically.** The built-in protection judges a path as written, with no symlink resolution and
  no `~` expansion (`crates/reno-proto/src/defs/eval.rs:25,48`). A symlink into the run's `.reno` directory, or a
  relative path that reaches it without naming `.reno`, is part of the unrecognised-writes residual in Out of Scope.
  The spike asked for paths to be resolved (`docs/roadmap/roads/ROAD-61.notes.md:19`, "resolve paths"); that belongs
  with ROAD-63's command parsing.
- **Event order.** Pi emits `tool_execution_start` before its hook runs (Pi `agent-loop.js:385-426`), so a call's
  `Tool` event is journaled just before its `NeedsApproval`. A blocked call then gets an error `ToolResult` with the
  gate's reason. The call still has its decision on record before it runs (4.1).

### Session directories

Satisfies: RENOHARNESS-8.5, RENOHARNESS-10.9, RENOHARNESS-10.10, RENOHARNESS-11.4, RENOHARNESS-11.5, RENOHARNESS-11.6
Reuse: rung 2. It hooks the kernel's one run-end seam, `workspaces::run_ended` (`crates/reno-kernel/src/workspaces.rs:179`),
which every end path calls: a supervised run's finish (`supervisor.rs:1006`), a cancel of a queued or interrupted run
(`kernel.rs:641`), and a run that never got a supervisor (`dispatcher.rs:1059`). Its sibling `close_if_unattended`
already reads whether the run ended for good. The sweep follows recovery's per-run file clearing through a port
(`recovery.rs:16-17`, `RunLink`).
Respects: ARCH-7, ARCH-8
Surface:
- `Executor` trait (`crates/reno-harness/src/exec/mod.rs:96-190`) and every implementation (`claude`, `codex`, `pi`,
  `muse`, `opencode`, `acp`, `cursor`, `mock`, and the test executors in `crates/reno-kernel/src/tests/`):
  **replace**. Two methods are added with default no-op bodies, so no implementation is edited and only `PiExecutor`
  overrides them.

Interface:
```
trait Executor {
  async fn run_closed(&self, run: &RunId) {}                 // the kernel calls it when the run is succeeded or cancelled
  async fn retain_runs(&self, keep: &HashSet<RunId>) {}       // the kernel awaits it once in recovery, before any claim
}
// PiExecutor{Reno}: run_closed first awaits the end of that run's attempt task (its teardown, so Pi is gone and the
//                   ScratchDir is dropped), then removes <sessions_root>/<run>;
//                   retain_runs removes every <sessions_root>/<dir> whose name is not in keep
// kernel: workspaces::run_ended() -> if status ∈ {Succeeded, Cancelled} { executor_of(run).run_closed(run) }, on a task
//         spawned with shared.spawn, as run_ended already spawns its idle-clock task (workspaces.rs:179-185), so the
//         kernel's task set owns it and shutdown awaits it (ARCH-11)
//         recovery (after step 2, before step 5's re-queue and before the dispatcher claims)
//                  -> keep = runs with status ∈ {Failed, Interrupted} ∪ {Queued with a session_ref}
//                  -> each executor.retain_runs(&keep).await
```
Depth: deletion test. If this vanished, the kernel would still need only "tell the executor when a run is closed,
and at start which runs to keep". Where the directories live and how they are removed stay inside the driver.
Locality: extend `exec/mod.rs` (two defaults), `workspaces.rs` (`run_ended`), `recovery.rs` (one awaited step) and
`drivers/pi/mod.rs` (a per-run handle on the attempt task). Other drivers: leave.

Keep set:
- It holds every run that can still be resumed: `failed` and `interrupted` (a person may resume either,
  `resume.rs:163`), and `queued` runs with a session (one recovery re-queued for auto-resume). Everything else is
  removed, including a directory whose run is unknown.
- Ordering. Recovery awaits `retain_runs` before it re-queues and before the dispatcher claims anything
  (`recovery.rs:1-2`). So no run created after the sweep can lose its directory to it. The step is one directory
  listing and removal per `reno` session dir, inside the boot that `docs/BENCHMARKS.md:28` holds to 300 ms "after
  recovery"; the bench's boot scenario covers it.
- Guards. `run_ended` keeps its idle-clock task and `close_if_unattended` exactly as they are, with `run_closed`
  added beside them (10.9). Recovery's existing steps keep their order (10.10).
- A person's resume of a run whose directory is gone falls back to Pi's existing "did not find session" notice
  (`drivers/pi/mod.rs:785-803`).

### Docs

Satisfies: RENOHARNESS-9.1, RENOHARNESS-9.2, RENOHARNESS-9.3
Reuse: rung 2, by editing the existing text in place:
- `README.md:58`;
- `drivers/pi/mod.rs:37-38,192-199,376`;
- `supervisor/protect.rs:3-9`;
- `crates/reno-proto/src/defs/bot.rs:97-100`.
Interface: none.
Depth: n/a. These are text edits.
Locality: leave. Text only.

## Seams for testing

| Seam | Kind | Covers |
|---|---|---|
| `PiExecutor` driven against the fake Pi (`crates/reno-harness/tests/pi.rs` + `tests/fixtures/fake-pi.py`; the fake plays the gate over `RENO_GATE_SOCKET`) | integration | RENOHARNESS-1.3, 2.1, 2.2, 2.3, 2.4, 2.6, 2.7, 2.8, 2.9, 7.1 (files laid out under `.reno`), 11.4 (driver-level `run_closed` removal), 3.1, 3.2, 3.3, 3.4, 3.5, 3.8, 4.1, 4.4, 5.1, 5.2, 5.4, 5.5, 6.1, 6.6, 7.2, 7.3, 7.5, 8.1, 8.2, 8.4, 10.1, 10.2, 11.1, 11.2, 11.3, 11.5 |
| `gate.mjs` under node, against a scripted socket (new, beside `pi-mcp-test.mjs`) | unit | RENOHARNESS-3.2, 3.3, 3.4, 3.5, 6.1, 6.2 |
| Kernel with `MockExecutor` (`crates/reno-kernel/src/tests/{approvals,protect,routing,resume}.rs` and recovery) | integration | RENOHARNESS-10.9, 10.10, 1.4, 1.5, 1.6, 2.5, 3.6, 3.7, 4.2, 4.3, 5.3, 6.3, 6.4, 6.5, 7.1, 7.4, 8.3, 8.5, 10.3, 10.4, 10.5, 11.4, 11.6 |
| Proto definitions (`crates/reno-proto/src/defs/tests.rs`) | unit | RENOHARNESS-1.7, 10.7, 10.8 |
| `apps/renod/src/executors.rs` tests (`built_in_ids_are_valid` and discover) | unit | RENOHARNESS-1.1, 1.2, 10.6 |
| Conformance `reno_target()` (`crates/reno-harness/tests/conformance/targets.rs`) with an approval plan | integration | RENOHARNESS-3.1, 3.3, 3.4 |
| Live Pi, `#[ignore]` + `RENO_PI_LIVE=1`, mock provider by `-e` (no key, no network) | e2e | RENOHARNESS-3.2, 5.1, 8.5 and 2.7 (a real Pi arms with its own `{}` `auth.json` present, and a planted credential ends the run), and the latency report |
| Docs check (a test asserting the retired README line is gone and the new one present) | unit | RENOHARNESS-9.1, 9.2, 9.3 |

New seams: two. They are the node test for `gate.mjs`, because the fake Pi cannot run JS, and the
`Executor::run_closed`/`retain_runs` pair, exercised through the kernel's `MockExecutor`. Every other row extends
an existing seam. ARCH-16 holds throughout: the socket lives in a tempdir, time is paused, and the live test is
`--ignored`.

## Coverage check

Every requirement ID appears in exactly one `Satisfies:` line above, except the four listed as unmapped below. The
mapping by section:

| Section | IDs |
|---|---|
| Identity | 1.1, 1.2, 1.7, 10.6, 10.8 |
| Eligibility | 1.4, 1.5, 1.6, 10.5 |
| Launch | 1.3, 2.1-2.6, 5.4, 5.5, 7.3, 7.5, 8.4, 10.1, 11.3 |
| Gate | 3.2-3.5, 6.1, 6.2 |
| Host | 2.7, 2.8, 2.9, 3.1, 5.1-5.3, 6.6, 7.2, 8.2, 11.1, 11.2 |
| Subjects | 3.8, 4.1 |
| Ladder | 3.6, 3.7, 4.2-4.4, 6.3, 7.1, 7.4, 8.1, 8.3, 10.3, 10.4 |
| Sessions | 8.5, 10.9, 10.10, 11.4-11.6 |
| Docs | 9.1-9.3 |

Unmapped on purpose:
- **6.4 and 6.5** are guards met by the existing process-group spawn and orphan sweep (`drivers/pi/mod.rs:296-298`,
  `recovery.rs:5-8`). They are tested in the kernel seam, but no section changes them.
- **10.2** is met by the unchanged settings deny-list in `rpc::Client::send`, beside 7.2's new flag.
- **10.7** is met by the unchanged `BotDef` (`deny_unknown_fields`, `bot.rs:53`).
