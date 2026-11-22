# Requirements: Bots run on Reno's own harness, with every tool call seen by Reno

Feature code: RENOHARNESS
Status: Draft
Date: 2026-11-22
Roadmap item: ROAD-62 (claim: `.scratch/reno-harness/issues/01-reno-harness.md`)

<!--
Each criterion ends with its source:
- [Lock …]: a decided line (vision, architecture, ADR, roadmap, ticket).
- [Weigh …]: a pick made in shaping. It is evidence, not the owner's word.
- [agent-decided]: chosen in shaping as the option that leaves every documented contract as it is.
Weigh and agent-decided values are not the owner's choice until the owner confirms them.
-->

The Reno harness is the executor Reno runs for bots: Pi, driven with the owner's API key and gated so
that every tool call passes Reno's policy before it runs (ADR-0001, ADR-0002, ARCH-18). The executor
`pi`, the owner's own Pi on their own login, stays as it is. In this file, "a run on `reno`" means a
run whose executor is the Reno harness.

## 1. Point a bot at the Reno harness

**Story:** As the owner, I want to list the Reno harness in a bot's `executors`, so that the bot's runs go to the harness Reno gates.

- **RENOHARNESS-1.1** THE SYSTEM SHALL register an executor with the id `reno` [agent-decided] that runs the installed Pi CLI, distinct from the executor `pi`, and list it in `reno executor list`. [Weigh W1; Lock CONTEXT.md:33]
- **RENOHARNESS-1.2** THE SYSTEM SHALL declare `supervised` and `full_access` as the access modes of `reno`, and `unrestricted` as its only sandbox level. [Weigh W3; Lock L10, L15]
- **RENOHARNESS-1.3** WHEN a run of a bot that lists `reno` is routed to `reno` THE SYSTEM SHALL start Pi in RPC mode with Reno's gate extension loaded for that run. [Lock L11, L12]
- **RENOHARNESS-1.4** IF a bot asks for `auto_accept_edits`, or for a sandbox narrower than `unrestricted`, THEN THE SYSTEM SHALL treat `reno` as ineligible for its runs, with the reason the router gives for any executor that does not declare the mode. [Lock L10; Weigh W3]
- **RENOHARNESS-1.5** IF a bot sets `inherit_user_config = true` THEN THE SYSTEM SHALL treat `reno` as ineligible for its runs, with a reason that names the setting, carried by the existing `RejectReason::Excluded { why }` (shown in the verbose routing explanation) and no new variant. [Lock L15, L19; Weigh: variant pick; agent-decided: no new variant]
- **RENOHARNESS-1.6** IF a run with no bot is to start on `reno` THEN THE SYSTEM SHALL refuse to start it, with a reason saying `reno` runs bots only, and without falling back to another executor, carried as an invalid start (the existing `StartRefusal::Invalid`) and no new variant. [agent-decided; CONTEXT.md:33; Weigh: variant pick]
- **RENOHARNESS-1.7** WHEN a bot definition lists `reno` THE SYSTEM SHALL accept the id wherever executor ids are validated, offline validation included. [Weigh W1]

## 2. The run uses the owner's Reno key, never Pi's own login

**Story:** As the owner, I want a Reno-harness run to call the model with the API key I stored in Reno, so that a bot never runs on my subscription login.

- **RENOHARNESS-2.1** WHEN a run starts on `reno` THE SYSTEM SHALL give Pi provider credentials only from the secrets the bot lists in `secrets`, read from Reno's own Keychain items. [Lock L7]
- **RENOHARNESS-2.2** WHEN a run starts on `reno` THE SYSTEM SHALL point Pi's agent directory at a directory that belongs to the run, so that Pi's own settings and stored login are never read. [Lock L7; `docs/standards/INDEX.md:24`]
- **RENOHARNESS-2.3** WHEN a run starts on `reno` THE SYSTEM SHALL pass to Pi none of the daemon environment's provider credential variables or Pi directory variables. [Lock L7]
- **RENOHARNESS-2.4** THE SYSTEM SHALL pass a run's provider key to Pi through the environment only, never on Pi's command line. [agent-decided]
- **RENOHARNESS-2.5** (guard) IF a secret that a bot lists is not set THEN THE SYSTEM SHALL CONTINUE TO refuse to start the run, naming the secret, on `reno` as on every executor.
- **RENOHARNESS-2.6** (guard) WHEN a run on `reno` prints to stderr or reaches the journal THE SYSTEM SHALL CONTINUE TO replace the run's secret values (exact match, 8 bytes or longer) with their `[secret:NAME]` mark in what is logged, kept or journaled.

## 3. Every tool call passes Reno's policy before it runs

**Story:** As the owner, I want every tool call a Reno-harness bot makes to be decided by Reno's policy before it runs, so that a call I forbid never happens.

- **RENOHARNESS-3.1** WHEN Pi is about to run a tool call on `reno` THE SYSTEM SHALL hold the call until Reno's approval ladder answers it, raised as `NeedsApproval` to the same policy, standing rules, inbox and journal that every run uses. [Lock L11, L17]
- **RENOHARNESS-3.2** THE SYSTEM SHALL hold, as RENOHARNESS-3.1 says, every call Pi runs, whatever its tool: for example its built-in tools, MCP tools, each of several calls in one assistant message, each nested call of a codemode script, and a tool an extension adds. [Lock L9, L12 (ADR-0002), L15, L19]
- **RENOHARNESS-3.3** WHEN the ladder denies a call on `reno` THE SYSTEM SHALL block it in Pi, so that it runs no part of its effect. [Lock L1, L9]
- **RENOHARNESS-3.4** WHEN the ladder denies a call on `reno` THE SYSTEM SHALL give the bot the ladder's reason as that call's result. [Lock L21]
- **RENOHARNESS-3.5** WHEN the ladder allows a call on `reno` THE SYSTEM SHALL let Pi run it with exactly the arguments it was judged on. [agent-decided]
- **RENOHARNESS-3.6** WHILE a run on `reno` is `full_access` THE SYSTEM SHALL answer its calls as the ladder does for every executor at full access: a policy deny, a standing deny or a repeat denial refuses; a policy or standing allow allows; a call to a tool of Reno's own MCP server is allowed (even when an `ask` rule names it); a call a policy rule names with the effect `ask` is asked of the owner; and the rest is allowed without asking. [Weigh W3]
- **RENOHARNESS-3.7** WHILE a run on `reno` is `supervised` THE SYSTEM SHALL ask the owner through the inbox about every call that the policy and the standing rules neither allow nor deny, that this run has not already refused, and that is not a call to a tool of Reno's own MCP server. [Weigh W3]
- **RENOHARNESS-3.8** WHEN the gate raises a call on `reno` THE SYSTEM SHALL describe it to the ladder with the action class, command and paths that the policy and the built-in protection read: `write` and `edit` as edits with their paths; `bash` and `powershell` as commands with their command line; `read`, `grep`, `find` and `ls` as reads with their paths; MCP tools as MCP tool calls with their tool name; and any other tool as class `other` with its tool name, judged like any call and never let through unraised. [Lock L11; L15 ("the gate is not a sandbox")]

## 4. Every tool call is on record, with its arguments, before it runs

**Story:** As the owner, I want to read in a run's journal each call its bot made, with its arguments and Reno's decision, so that I can see everything the bot did.

- **RENOHARNESS-4.1** WHEN a call on `reno` is raised to the ladder THE SYSTEM SHALL journal it as `NeedsApproval` before the call runs, with a subject that keeps the gate's request verbatim: tool name, call id, parent call id and arguments, redacted of the run's secret values. [Lock L1, L19]
- **RENOHARNESS-4.2** WHEN the ladder answers a call on `reno` THE SYSTEM SHALL commit the decision, what made it and its reason to the approval history before Pi is told, and then journal it as `ApprovalResolved`, as the ladder does for every executor. [Lock L19]
- **RENOHARNESS-4.3** (guard) WHERE a call is to a tool of Reno's own MCP server THE SYSTEM SHALL CONTINUE TO record it in the approval history (its approvals row and that row's event) rather than as a journal pair.
- **RENOHARNESS-4.4** (guard) WHILE a Pi run goes on THE SYSTEM SHALL CONTINUE TO journal Pi's tool executions, their results and its per-turn token usage, on `reno` as on `pi`.

## 5. A run never starts unless Reno is watching

**Story:** As the owner, I want a Reno-harness run to refuse to start when Reno's gate is not confirmed live inside Pi, so that no bot ever runs ungated.

- **RENOHARNESS-5.1** WHEN Pi has started for a run on `reno` THE SYSTEM SHALL send the run's first prompt only after Pi lists the gate's command as an extension command and the gate has sent the run's own nonce on the run's socket. [Lock L11, L15]
- **RENOHARNESS-5.2** IF either check of RENOHARNESS-5.1 fails, or both have not passed within a bounded time, THEN THE SYSTEM SHALL fail the run without sending a prompt, with a reason that names the check. [Lock L1, L11]
- **RENOHARNESS-5.3** IF a run fails as RENOHARNESS-5.2 says THEN THE SYSTEM SHALL end the run failed, without falling back to another executor. [Weigh S1]
- **RENOHARNESS-5.4** THE SYSTEM SHALL have Pi load the gate after every other extension of the run. [Lock L15]
- **RENOHARNESS-5.5** WHEN a run on `reno` is resumed THE SYSTEM SHALL start a new Pi with a new socket and a new nonce, and arm it as RENOHARNESS-5.1 says before the resumed prompt. [Lock L11]

## 6. A call Reno cannot judge does not run

**Story:** As the owner, I want a call to be blocked whenever Reno cannot answer it, so that a crash or a silence never becomes a yes.

- **RENOHARNESS-6.1** IF the gate cannot reach the run's socket, the socket closes, or the gate's handler fails, THEN THE SYSTEM SHALL block every call from then on, with a reason saying Reno could not judge it. [Lock L9, L11]
- **RENOHARNESS-6.2** WHILE a call on `reno` waits for the ladder THE SYSTEM SHALL keep the gate waiting at least until the ladder's deadline for that call, so that the gate never blocks a call the ladder has not yet answered. [Lock L19]
- **RENOHARNESS-6.3** (guard) IF no person answers a call by its deadline THEN THE SYSTEM SHALL CONTINUE TO deny it as expired, on `reno` as on every executor.
- **RENOHARNESS-6.4** (guard) WHEN the daemon shuts down while a Pi run goes on THE SYSTEM SHALL CONTINUE TO stop that run's Pi process group with it.
- **RENOHARNESS-6.5** (guard) WHEN the daemon starts after a crash THE SYSTEM SHALL CONTINUE TO end the Pi process groups the crash left running, by the orphan sweep from the stored process records, before any run resumes. Until then, the gate of such a Pi has lost its socket, and RENOHARNESS-6.1 blocks its calls.

## 7. Nothing in the run can change or get around its gate

**Story:** As the owner, I want a bot's own calls to be unable to change or get around the gate that watches it, so that a run cannot switch Reno's watch off.

- **RENOHARNESS-7.1** IF a call on `reno` would write, move or delete the gate extension, the run's Pi agent directory, or another file Reno loads into Pi for the run, THEN THE SYSTEM SHALL deny it, whatever the policy and the access mode allow. [Weigh S3; Lock L19]
- **RENOHARNESS-7.2** THE SYSTEM SHALL refuse, in the driver's writer, to send Pi's host RPC `bash` command on a run on `reno`. [Lock L15]
- **RENOHARNESS-7.3** WHEN Pi starts for a run on `reno` THE SYSTEM SHALL load none of the owner's Pi extensions, skills, prompt templates or MCP servers, and name each extension it loads explicitly: Reno's MCP bridge, Pi's built-in MCP and codemode, and the gate. [Lock L15]
- **RENOHARNESS-7.4** (guard) WHEN a call on `reno` is an edit of a path in `.git`, `.reno` or Reno's data directory, or a command that writes there in a form the built-in protection recognises, THE SYSTEM SHALL CONTINUE TO deny it in every access mode, before any allow, as the built-in protection does for every run.

## 8. The run goes on until the harness says it is done

**Story:** As the owner, I want a Reno-harness run to end when Pi itself settles, so that Reno adds no limit of its own to a long task.

- **RENOHARNESS-8.1** THE SYSTEM SHALL add no limit of its own on the turns or tool calls of a run on `reno`. [Lock L1, L13]
- **RENOHARNESS-8.2** WHEN Pi reports `agent_settled` for a run on `reno` THE SYSTEM SHALL end the run, with the stop reason that Pi's last assistant message gives, as the `pi` driver does. [Lock L1, L21]
- **RENOHARNESS-8.3** (guard) WHERE a bot's budget sets `max_wall_time`, `max_concurrent_runs` or `daily_token_ceiling` THE SYSTEM SHALL CONTINUE TO apply them to its runs on `reno` as on any executor, defaults included.

## 9. Reno's docs say what it now does

**Story:** As someone reading Reno's README, I want it to say Reno has a harness of its own for bots that uses my API keys, so that it does not tell me Reno never runs a model.

- **RENOHARNESS-9.1** THE SYSTEM SHALL replace the README line "It never runs a model itself." with one that says Reno runs your installed CLIs and has a harness of its own for bots that uses your API keys. [Lock L16]
- **RENOHARNESS-9.2** THE SYSTEM SHALL describe Pi, in the Pi driver's docs and in the protected-path detectors' docs, as able to block each call through an extension, as gated by Reno only as `reno`, and as asking nothing as `pi`. [Lock L18]
- **RENOHARNESS-9.3** THE SYSTEM SHALL describe, in the bot definition's docs for `inherit_user_config`, which executors isolate the owner's configuration as their descriptors declare it, Pi and the Reno harness included. [Lock L18]

## 10. Everything else works as before

**Story:** As the owner, I want my `pi` bots and every other executor to run exactly as before, so that adding the Reno harness breaks nothing I use.

Files the change touches, and what each guards:

| File | Guard |
|---|---|
| `crates/reno-harness/src/drivers/pi/mod.rs` | RENOHARNESS-10.1 |
| `crates/reno-harness/src/drivers/pi/rpc.rs` | RENOHARNESS-10.2 |
| `crates/reno-harness/src/wire/pi/launch.rs` | RENOHARNESS-10.1 |
| `crates/reno-harness/src/wire/pi/normalize.rs` | RENOHARNESS-4.4 |
| `crates/reno-harness/src/child_env.rs` | RENOHARNESS-10.1 |
| `crates/reno-harness/src/drivers/pi/` new gate extension and its socket | no behavior to guard (new) |
| `crates/reno-kernel/src/supervisor/approval.rs` | RENOHARNESS-10.3, RENOHARNESS-4.3, RENOHARNESS-6.3 |
| `crates/reno-kernel/src/supervisor/protect.rs` (docs: Pi "asks nothing" is true only of `pi`) | RENOHARNESS-10.4 |
| `crates/reno-kernel/src/supervisor.rs` (read, not changed) | RENOHARNESS-4.4, RENOHARNESS-2.6 |
| `crates/reno-kernel/src/budget.rs` (read, not changed) | RENOHARNESS-8.3 |
| `crates/reno-proto/src/defs/mod.rs` (`BUILTIN_EXECUTORS`) | RENOHARNESS-10.8 |
| `crates/reno-proto/src/defs/eval.rs`, `protect.rs` (read; changed only if design puts RENOHARNESS-7.1 there) | RENOHARNESS-7.4 |
| `apps/renod/src/daemon.rs` (read, not changed) | RENOHARNESS-10.6, RENOHARNESS-6.5 |
| `crates/reno-kernel/src/dispatcher.rs` and `router.rs` | RENOHARNESS-10.5 |
| `apps/renod/src/executors.rs` | RENOHARNESS-10.6 |
| `crates/reno-proto/src/defs/bot.rs` (docs only) | RENOHARNESS-10.7 |
| `crates/reno-kernel/src/dispatcher.rs` secret resolution, `crates/reno-proto/src/secrets.rs` (read, not changed) | RENOHARNESS-2.5 |
| `README.md` | no behavior to guard (text) |
| `crates/reno-proto/src/protocol.rs`, `crates/reno-store/src/migrations.rs` | not touched: no wire method, no migration |

- **RENOHARNESS-10.1** (guard) WHEN a bot or the owner runs on `pi` THE SYSTEM SHALL CONTINUE TO run the owner's Pi at full access and unrestricted only, with the flags, environment allowlist, login and owner config handling it uses today, and no gate.
- **RENOHARNESS-10.2** (guard) WHEN the driver writes to any Pi process THE SYSTEM SHALL CONTINUE TO refuse Pi's settings-writing RPC commands.
- **RENOHARNESS-10.3** (guard) WHEN any executor raises `NeedsApproval` THE SYSTEM SHALL CONTINUE TO judge it by the ladder's steps in their shipped order, with the same outcomes.
- **RENOHARNESS-10.4** (guard) WHILE a run on `pi` goes on THE SYSTEM SHALL CONTINUE TO run the after-the-fact protected-path detectors on it.
- **RENOHARNESS-10.5** (guard) WHEN the chosen executor declines a run for an account or install reason THE SYSTEM SHALL CONTINUE TO fall back to the next eligible executor the bot lists.
- **RENOHARNESS-10.6** (guard) WHEN the daemon starts THE SYSTEM SHALL CONTINUE TO register every executor it registers today, `pi` included (every harness executor unless harnesses are switched off), and the kernel SHALL CONTINUE TO probe each once in the background.
- **RENOHARNESS-10.7** (guard) WHEN a bot definition that does not list `reno` is loaded THE SYSTEM SHALL CONTINUE TO parse and validate it exactly as today, with no new field accepted or required.
- **RENOHARNESS-10.8** (guard) WHEN a bot definition lists a built-in executor id other than `reno` THE SYSTEM SHALL CONTINUE TO accept it in offline validation as today.

## 11. Quality attributes

**Section-kind:** nfr

**Story:** As a stakeholder, I want measurable quality targets for this feature, so that how-well is not left implicit.

System-doc consult: `docs/product/metrics.md`, `docs/ops/reliability.md` and `docs/security/threat-model.md` are absent, so the consult is a no-op. The targets below are domain judgment with no greppable ID.

- **Performance:** None. There is no standing target: no metrics or reliability doc defines one, and no number for the real path (one `synchronous=FULL` commit plus a journal write per call) has been measured. The spike's figure was for a path without either. The `--ignored` live Pi test reports gated-call latency over 200 policy-allowed calls as a measurement, with no pass or fail bound. A target, if one is wanted, is set from that first measurement.
- **Security:** **RENOHARNESS-11.1** THE SYSTEM SHALL make each run's gate socket reachable only by the owner's user, and accept on it only the one connection whose hello carries that run's nonce, closing any other with no decision given, verified by a unit test that connects a second client. [agent-decided]
- **Reliability:** **RENOHARNESS-11.2** THE SYSTEM SHALL hold at most a bounded number of unanswered gate requests per run, and deny each request beyond the bound, verified by a test that floods the socket. [agent-decided; ARCH-11]
- **Reliability:** **RENOHARNESS-11.3** WHEN a run on `reno` ends, however it ends, THE SYSTEM SHALL remove its socket and the files Reno wrote for it, verified by a test that ends runs as done, failed, cancelled and with Pi killed. [agent-decided]
- **Accessibility:** None — this feature adds no screen. The app names the executor by its generic rule (`apps/reno-app/src/summary.rs:272-287`).

## Out of Scope

- What the policy allows, asks or refuses, and per-bot tool and domain lists: ROAD-63. With the shipped defaults (`supervised`, policy `ask`), a `reno` bot asks about every call that the shipped policy rules, the built-in protection, the repeat-denial rule and Reno's own MCP tools do not answer, until ROAD-63 sets its bounds.
- How finely the ladder tells calls apart. A subject's digest leaves out its raw arguments, so two calls with the same tool, command and paths count as one for repeat denial and standing rules. For an MCP tool, every call to that tool in a run counts as one. This feature keeps the ladder as shipped; finer subjects are ROAD-63's.
- `max_wall_time` defaults and cost display: ROAD-64. With the shipped default `max_wall_time = 45m` (`crates/reno-proto/src/defs/bot.rs:165-166`), a run on `reno` is still stopped at 45 minutes until ROAD-64 changes it. GOAL-10's "nothing caps a run by default" is not met by this feature alone.
- Showing usage per call. Pi reports usage per assistant turn, and this feature records it per turn. MILE-11's "the owner sees each call with its usage" is not met by this feature alone; its display is ROAD-64's, and the MILE-11 assessment checks it.
- The bot's computer, VM, egress allowlist and browser: MILE-12.
- Runs the owner starts on other harnesses deciding their own permissions (ROAD-4), and harness questions in general (ROAD-1).
- Argument rewrite by the gate, `auto_accept_edits` on `reno`, a new wire method, a migration or new persisted format, a new variant of a persisted or wire enum (`RejectReason`, `StartRefusal`, `ExecEvent`), and any new screen.
- Exfiltration of the provider key. A model-run command can read the key from the run's environment, because secrets are environment variables of the run. This is an accepted risk handed to ROAD-63.

## Open Questions

None.
