# Roadmap: Reno

Status: Approved
Date: 2026-11-22

<!--
The program layer: milestone INTENT, above any single feature and below the product
vision. `plan-milestones` authors it; `refresh-roadmap-status` derives progress from it
without writing anything.

This file owns intent only: outcomes, priority, ordering, membership, dependencies,
commitments, blockers, deferrals, goal dispositions. It never records how far a feature has got —
that lives once, as `Status:` in the feature's own requirements.md, mirrored into its
docs/specs/INDEX.md row. Every heading below is a REQUIRED slot — fill it or write `None`.

Structural rules S1–S10, Focus rules and ID rules: see the `plan-milestones` template
(`templates/roadmap-INDEX.md`). In short: MILE-N and ROAD-N are defined once and never
renumbered or reused; priority lives only in Focus; order lives in After:; a ROAD-N is a
slot, bound to a feature code only by `specify-behavior` in docs/specs/INDEX.md.
-->

## Focus

**Now:** ROAD-33, ROAD-56, ROAD-61, ROAD-65
**Next:** ROAD-55, ROAD-53, MILE-1, MILE-11

| ID | Milestone | Depends-on | Commitment |
|---|---|---|---|
| MILE-10 | Workflow design on a mocked daemon | none | Planned |
| MILE-1 | Harness parity | MILE-10 | Planned |
| MILE-2 | Choose where and how a task runs | MILE-10 | Planned |
| MILE-3 | Harness depth | MILE-1 | Planned |
| MILE-4 | Agents hand work to each other | MILE-1, MILE-3 | Planned |
| MILE-5 | Notices and terminal status | MILE-10 | Planned |
| MILE-6 | The app on the real daemon | MILE-10, MILE-2, MILE-3, MILE-4, MILE-5 | Planned |
| MILE-7 | Bots made by chat | MILE-4 | Planned |
| MILE-8 | Teams on one goal | MILE-4 | Planned |
| MILE-9 | Checked work and gated improvement | MILE-8 | Planned |
| MILE-11 | Bots on Reno's own harness | MILE-10 | Planned |
| MILE-12 | A computer for each bot | MILE-11 | Planned |

## MILE-10 — Workflow design on a mocked daemon

**Outcome:** The owner clicks through every main workflow in the new design on mocked data (start a task in a project, branch or worktree; answer questions and approvals from one inbox; follow a run with its subagents, background tasks and plan; hand work to other harnesses; make a bot by chat; manage executors and settings) and grades each one good enough or not.
**Goals:** GOAL-1, GOAL-11, GOAL-7, GOAL-8, GOAL-9
**Members:**
- **ROAD-30** ui-direction — After: none — Surfaces: None — the new design is drafted with `draft-ui` first
- **ROAD-29** app-budgets-and-profiling — After: none — Surfaces: `xtask/src/bench.rs`, `docs/BENCHMARKS.md`, `apps/reno-app/`
- **ROAD-52** mocked-contracts — After: none — Surfaces: `crates/reno-client/src/fake/`, `crates/reno-proto/`
- **ROAD-31** composer — After: ROAD-30, ROAD-52 — Surfaces: `apps/reno-app/`
- **ROAD-32** sidebar-and-sessions — After: ROAD-30, ROAD-52 — Surfaces: `apps/reno-app/`
- **ROAD-33** transcript — After: ROAD-30, ROAD-29 — Surfaces: `apps/reno-app/`, `crates/reno-markdown/`, `crates/reno-client/` — Notes: [ROAD-33](roads/ROAD-33.notes.md) — Card: [ROAD-33](roads/ROAD-33.md)
- **ROAD-34** palette-and-keymap — After: ROAD-30 — Surfaces: `apps/reno-app/`
- **ROAD-35** panel-surfaces — After: ROAD-30, ROAD-52 — Surfaces: `apps/reno-app/src/panel/`
- **ROAD-37** settings-and-design-system — After: ROAD-30 — Surfaces: `apps/reno-app/src/ui/`
- **ROAD-38** first-run-and-shell — After: ROAD-30 — Surfaces: `apps/reno-app/`
- **ROAD-55** app-crate-split — After: ROAD-29, ROAD-32 — Surfaces: `apps/reno-app/`, `xtask/src/main.rs`, `docs/architecture/system.md` — Notes: [ROAD-55](roads/ROAD-55.notes.md)
- **ROAD-53** workflow-review-and-contract-drafts — After: ROAD-31, ROAD-32, ROAD-33, ROAD-34, ROAD-35, ROAD-37, ROAD-38 — Surfaces: `docs/specs/`
**Depends-on:** none
**Commitment:** Planned
**Closed:** None
**Deferred:** None
**Blockers:** None

## MILE-1 — Harness parity

**Outcome:** A run the owner starts from Reno behaves as the same harness started in a terminal in that folder (its own settings, allow-list, hooks and MCP servers decide), the owner answers its questions from Reno, an "Always allow" given in Reno also holds in the terminal, and every daemon feature is reachable from the CLI.
**Goals:** GOAL-11, GOAL-6, GOAL-8
**Members:**
- **ROAD-1** answer-harness-questions — After: none — Surfaces: `crates/reno-proto/src/protocol.rs`, `crates/reno-kernel/src/supervisor.rs`, `crates/reno-harness/src/drivers/`, `apps/reno/` — Card: [ROAD-1](roads/ROAD-1.md)
- **ROAD-2** owner-started-vs-unattended-runs — After: none — Surfaces: `crates/reno-proto/src/exec.rs`, `crates/reno-kernel/`
- **ROAD-3** terminal-parity-config — After: ROAD-2 — Surfaces: `crates/reno-harness/src/drivers/`, `crates/reno-harness/src/child_env.rs`, `crates/reno-harness/src/conformance/`
- **ROAD-4** harness-decides-permissions — After: ROAD-2 — Surfaces: `crates/reno-kernel/src/supervisor/approval.rs`, `crates/reno-proto/src/defs/`, `crates/reno-harness/src/wire/` — Notes: [ROAD-4](roads/ROAD-4.notes.md)
- **ROAD-5** always-allow-into-harness-settings — After: ROAD-4 — Surfaces: `crates/reno-harness/src/drivers/`, `apps/renod/`, `apps/reno/`
- **ROAD-6** cli-reaches-every-method — After: none — Surfaces: `apps/reno/`
**Depends-on:** MILE-10
**Commitment:** Planned
**Closed:** None
**Deferred:** None
**Blockers:** None

## MILE-2 — Choose where and how a task runs

**Outcome:** From the CLI the owner registers, clones or creates a project, picks a branch and whether a task runs in the current checkout or a worktree, names, pins and archives threads, queues messages behind a running turn, interrupts a turn without ending the run, and asks for plan mode.
**Goals:** GOAL-1, GOAL-8
**Members:**
- **ROAD-7** projects — After: none — Surfaces: `crates/reno-store/`, `apps/renod/`, `apps/reno/`
- **ROAD-8** branch-and-checkout-choice — After: ROAD-7 — Surfaces: `crates/reno-proto/src/protocol.rs`, `crates/reno-harness/src/proc/git_workspace.rs`, `crates/reno-kernel/`
- **ROAD-9** thread-names-pins-archive — After: none — Surfaces: `crates/reno-proto/src/thread.rs`, `crates/reno-store/`, `apps/reno/`
- **ROAD-10** run-control — After: ROAD-4 — Surfaces: `crates/reno-kernel/src/supervisor.rs`, `crates/reno-store/`, `crates/reno-harness/src/drivers/`
- **ROAD-11** workspace-and-git-views — After: none — Surfaces: `crates/reno-proto/src/workspace.rs`, `crates/reno-proto/src/files.rs`, `apps/renod/`
**Depends-on:** MILE-10
**Commitment:** Planned
**Closed:** None
**Deferred:** None
**Blockers:** None

## MILE-3 — Harness depth

**Outcome:** The owner sees what a harness's subagents, background tasks and plan list are doing and can stop a background task, sends images as images, uses the harness's own slash commands and skills, picks its model options, and enables, signs in and checks usage of each harness from Reno.
**Goals:** GOAL-1, GOAL-8
**Members:**
- **ROAD-12** subagent-and-child-events — After: none — Surfaces: `crates/reno-proto/src/exec.rs`, `crates/reno-harness/src/wire/`
- **ROAD-13** background-tasks — After: ROAD-3 — Surfaces: `crates/reno-harness/src/drivers/claude/`, `crates/reno-proto/src/exec.rs`, `apps/renod/`
- **ROAD-14** plan-and-todo-events — After: none — Surfaces: `crates/reno-proto/src/exec.rs`, `crates/reno-harness/src/wire/`
- **ROAD-15** image-input — After: none — Surfaces: `crates/reno-proto/src/exec.rs`, `crates/reno-harness/src/drivers/`
- **ROAD-16** model-options-and-effort — After: none — Surfaces: `crates/reno-harness/src/drivers/claude/`, `crates/reno-harness/src/drivers/codex/`, `crates/reno-proto/`
- **ROAD-17** commands-and-skills-discovery — After: ROAD-3 — Surfaces: `crates/reno-harness/`, `crates/reno-proto/src/protocol.rs`
- **ROAD-18** executor-catalog-depth — After: none — Surfaces: `crates/reno-harness/src/wire/acp/`, `crates/reno-harness/src/drivers/opencode/`
- **ROAD-19** executor-enable-signin-usage — After: none — Surfaces: `crates/reno-harness/src/usage/`, `crates/reno-harness/src/exec/`, `apps/renod/`
**Depends-on:** MILE-1
**Commitment:** Planned
**Closed:** None
**Deferred:** None
**Blockers:** None

## MILE-4 — Agents hand work to each other

**Outcome:** A lead (a Reno run or the owner's own terminal session) lists harnesses and models, starts child runs on any harness, messages them, waits for them, reads and stops them without giving any child more access than it has, and the owner forks or opens a side chat from any run.
**Goals:** GOAL-3, GOAL-7
**Members:**
- **ROAD-20** owner-session-as-lead — After: MILE-1 — Surfaces: `apps/reno/src/mcp.rs`, `crates/reno-rpc/src/transport.rs`
- **ROAD-21** delegation-tools — After: ROAD-20, ROAD-12 — Surfaces: `apps/reno/src/mcp.rs`, `crates/reno-kernel/`
- **ROAD-22** unforgeable-agent-messages — After: ROAD-21 — Surfaces: `apps/renod/`, `crates/reno-proto/src/agent.rs`
- **ROAD-23** lead-answers-child-questions — After: ROAD-1, ROAD-21 — Surfaces: `apps/reno/src/mcp.rs`
- **ROAD-24** side-chat-and-fork — After: none — Surfaces: `crates/reno-proto/src/protocol.rs`, `crates/reno-kernel/`, `crates/reno-harness/src/drivers/`
- ~~**ROAD-25**~~ lead-build-spike — retired 2026-11-22: ADR-0001 settles it; a lead may be a bot on the Reno harness, which ROAD-61 picks
**Depends-on:** MILE-1, MILE-3
**Commitment:** Planned
**Closed:** None
**Deferred:** None
**Blockers:** None

## MILE-5 — Notices and terminal status

**Outcome:** The owner gets a desktop banner when a run finishes, fails or needs them even with the app closed, and programs in Reno's terminals report working, needs-input and done, with search and clickable links in the terminal.
**Goals:** GOAL-11
**Members:**
- **ROAD-26** banners-from-the-daemon — After: none — Surfaces: `apps/renod/`, `crates/reno-notify/`
- **ROAD-27** terminal-program-status — After: none — Surfaces: `crates/reno-term/`, `apps/renod/`
- **ROAD-28** terminal-search-and-links — After: none — Surfaces: `crates/reno-term/`, `apps/reno-app/src/terminal.rs`
**Depends-on:** MILE-10
**Commitment:** Planned
**Closed:** None
**Deferred:** None
**Blockers:** None

## MILE-6 — The app on the real daemon

**Outcome:** Every workflow designed in MILE-10 runs on the real daemon instead of mocks, and the app stays inside its written idle, streaming and cold-start budgets.
**Goals:** GOAL-1, GOAL-11, GOAL-7, GOAL-8
**Members:**
- **ROAD-54** replace-mocks-with-real-contracts — After: MILE-10 — Surfaces: `apps/reno-app/`, `crates/reno-client/` — Notes: [ROAD-54](roads/ROAD-54.notes.md)
- **ROAD-36** project-actions — After: ROAD-7, MILE-10 — Surfaces: `crates/reno-store/`, `apps/renod/`, `apps/reno-app/`
- **ROAD-39** browser-tab-for-a-person — After: ROAD-35 — Surfaces: None — needs a new boundary crate for the web view, decided when it starts
- **ROAD-56** bounded-queues-enforced — After: none — Surfaces: `clippy.toml`, `apps/reno-app/`, `crates/reno-harness/src/drivers/opencode/`, `docs/architecture/runtime.md` — Notes: [ROAD-56](roads/ROAD-56.notes.md)
- **ROAD-70** change-provenance — After: ROAD-54 — Surfaces: `crates/reno-kernel/`, `crates/reno-store/`, `apps/reno-app/src/panel/` — Notes: [ROAD-70](roads/ROAD-70.notes.md)
- **ROAD-57** thread-links-open-the-app — After: none — Surfaces: `xtask/src/bundle.rs`, `apps/reno-app/src/lifecycle.rs` — Notes: [ROAD-57](roads/ROAD-57.notes.md)
**Depends-on:** MILE-10, MILE-2, MILE-3, MILE-4, MILE-5
**Commitment:** Planned
**Closed:** None
**Deferred:**
- **ROAD-40** pull-requests → MILE-6 (2026-11-19, large; over the owner's own git credentials, after the app exists)
- **ROAD-41** app-and-daemon-self-update → MILE-6 (2026-11-19, needs a release pipeline, signing and notarization first)
- **ROAD-42** edit-files-in-the-panel → MILE-6 (2026-11-19, editing was left for later; touches run protection)
- **ROAD-43** split-chats → MILE-6 (2026-11-19, after the new shell exists)
- ~~**ROAD-44**~~ agent-published-visuals → MILE-6 (2026-11-19) — retired 2026-11-22: static visuals an agent posts moved into ROAD-33, scripted ones into ROAD-58
- **ROAD-58** agent-scripted-visuals → MILE-6 (2026-11-22, reverses the earlier ruling that Reno renders harness output, not agent-written UI; needs a vision edit and an ADR first) — Notes: [ROAD-58](roads/ROAD-58.notes.md)
- **ROAD-45** window-capture-into-composer → MILE-6 (2026-11-19, a person's tool in its own boundary crate, after the redesign)
**Blockers:** None

## MILE-7 — Bots made by chat

**Outcome:** The owner creates or changes a bot by chatting with Reno, sees the change as a diff in the definitions repo, and it takes effect only after they accept it.
**Goals:** GOAL-9
**Members:**
- **ROAD-46** bot-by-chat — After: MILE-4 — Surfaces: `crates/reno-proto/src/defs/`, `crates/reno-kernel/src/defs.rs`, `apps/renod/`
**Depends-on:** MILE-4
**Commitment:** Planned
**Closed:** None
**Deferred:** None
**Blockers:** None

## MILE-8 — Teams on one goal

**Outcome:** Several bots work one goal in parallel, each in its own worktree, coordinating through a shared board and messages, and a flow survives a daemon restart.
**Goals:** GOAL-3
**Members:**
- **ROAD-47** shared-board — After: MILE-4 — Surfaces: `crates/reno-store/`, `crates/reno-kernel/`, `apps/reno/src/mcp.rs`
- **ROAD-48** mailbox — After: ROAD-22 — Surfaces: `crates/reno-kernel/`, `crates/reno-store/`
- **ROAD-49** journaled-flows — After: ROAD-47, ROAD-48 — Surfaces: `crates/reno-kernel/`, `crates/reno-store/`
**Depends-on:** MILE-4
**Commitment:** Planned
**Closed:** None
**Deferred:** None
**Blockers:** None

## MILE-9 — Checked work and gated improvement

**Outcome:** A bot or flow that asks for it has its work checked by tests, then a verifier on a different harness, then the owner's verdict, and a definition change is replay-tested, approved by the owner and reverted if it does worse.
**Goals:** GOAL-4, GOAL-5
**Members:**
- **ROAD-50** evaluations — After: MILE-8 — Surfaces: `crates/reno-kernel/`, `crates/reno-store/`
- **ROAD-51** gated-definition-improvement — After: ROAD-50 — Surfaces: `crates/reno-kernel/`, `crates/reno-proto/src/defs/`
**Depends-on:** MILE-8
**Commitment:** Planned
**Closed:** None
**Deferred:** None
**Blockers:** None

## MILE-11 — Bots on Reno's own harness

**Outcome:** The owner gives a bot Reno's own harness: it runs on the owner's API key, every tool call it makes is checked by that bot's policy before it runs and a denied call never runs, the owner sees each call with its usage, and a run goes on until the harness says it is done.
**Goals:** GOAL-1, GOAL-10, GOAL-11
**Members:**
- **ROAD-61** reno-harness-spike — After: none — Surfaces: None — a throwaway spike picks Pi or dsh — Notes: [ROAD-61](roads/ROAD-61.notes.md)
- **ROAD-62** reno-harness-driver-and-gate — After: ROAD-61 — Surfaces: `crates/reno-harness/src/drivers/`, `crates/reno-kernel/src/supervisor/approval.rs`, `crates/reno-proto/src/defs/bot.rs`, `README.md`
- **ROAD-63** per-bot-tool-and-domain-policy — After: ROAD-62 — Surfaces: `crates/reno-proto/src/defs/`, `crates/reno-kernel/`
- **ROAD-64** runs-uncapped-by-default — After: none — Surfaces: `crates/reno-proto/src/defs/bot.rs`, `crates/reno-kernel/src/budget.rs`, `docs/ROADMAP.md`
**Depends-on:** MILE-10
**Commitment:** Planned
**Closed:** None
**Deferred:** None
**Blockers:** None

## MILE-12 — A computer for each bot

**Outcome:** A bot on Reno's harness works in a computer of its own on the owner's Mac: a browser first, only its project mounted, only the domains its definition allows reachable, and the owner watches it live and can take over.
**Goals:** GOAL-10
**Members:**
- **ROAD-65** bot-computer-spike — After: none — Surfaces: None — a throwaway spike on a Linux VM per bot — Notes: [ROAD-65](roads/ROAD-65.notes.md)
- **ROAD-66** bot-computer-vm — After: ROAD-65 — Surfaces: `apps/renod/`, `crates/reno-kernel/`
- **ROAD-67** egress-allowlist-proxy — After: ROAD-66 — Surfaces: `apps/renod/`, `crates/reno-proto/src/defs/bot.rs`
- **ROAD-68** browser-tools-and-live-view — After: ROAD-66, ROAD-62 — Surfaces: `crates/reno-harness/`, `apps/reno-app/`
- **ROAD-69** computer-setup-scripts — After: ROAD-66 — Surfaces: `apps/renod/`
**Depends-on:** MILE-11
**Commitment:** Planned
**Closed:** None
**Deferred:** None
**Blockers:** None

## Goal dispositions

Every live `GOAL-N` in `docs/product/vision.md` that no milestone cites belongs here, so
that a goal is never silently dropped (S6).

| Goal | Disposition | Date | Reason |
|---|---|---|---|
| None | | | Every live goal (GOAL-1, GOAL-3 to GOAL-11) is cited by a milestone |
