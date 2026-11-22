# Product Vision: Reno

Status: Approved
Date: 2026-11-22

## Problem

Coding harnesses (Claude Code, Codex, Pi, opencode, Cursor, the ACP agents) are each strong alone, but each runs alone, in its own terminal with its own sessions. Nothing hands a task to the harness best suited for it, lets one agent break work down and give the parts to others, runs automation bots on a schedule or an event, or gathers what every agent needs from the owner into one place. Tools that try usually wrap the harnesses in a new agent loop and permission layer, and lose the harness's own settings, allow-lists, MCP servers and features. Platforms that run their own agents for recurring work give each bot a model and a computer, but little say over what each bot may do, spend or change.

## Users

A developer on their own Mac who already uses several agent CLIs and wants them to work as one team, and who wants bots that automate recurring work of any kind, not only code, created by chatting with Reno. The owner is the first user.

## Goals

- **GOAL-1** Each task goes to the executor that suits it, with the reason shown before it is sent, and falls back to the next when one is signed out or missing.
- ~~**GOAL-2**~~ superseded by GOAL-11 on 2026-11-22: Reno now has a harness of its own, whose permission system is Reno's policy. (Was: "The owner answers, from one inbox, everything a harness would have asked them, for every run including unattended ones. Reno adds no permission layer of its own.")
- **GOAL-3** Several bots work one goal in parallel, each in its own worktree, coordinating through boards and messages, and a flow survives a daemon restart.
- **GOAL-4** When a bot or flow asks for it, work is checked before it is called done: tests first, then a verifier on a different harness, then the owner's verdict.
- **GOAL-5** Definitions improve only under a gate: a proposal is replay-tested, needs the owner's approval, and reverts itself if it does worse.
- **GOAL-6** Everything runs on the owner's machine with the logins they already have: no Reno account and no cloud service. Reno never writes or refreshes a harness's auth or Keychain (it may read a login, read-only, to show plan usage), and writes a harness setting only for an explicit owner action: an allow rule the owner clicks, or switching a CLI's own updater off and back on.
- **GOAL-7** A lead agent breaks work down and hands the parts to the right running harnesses through Reno's MCP or ACP, so one workflow combines several harnesses. The lead can be any harness, including the owner's own terminal session.
- **GOAL-8** A run the owner starts behaves as the same harness started in a terminal in the same directory: same settings, allow-list, hooks, MCP servers and features. A conformance check proves it for each driver.
- **GOAL-9** The owner creates or changes a bot by chatting with Reno. The change lands as a diff in the definitions repo and takes effect only after the owner accepts it.
- **GOAL-10** A bot can run on Reno's own harness, with the owner's API keys and a computer of its own (a browser first, in a VM on the Mac), and the owner controls each such bot in depth: which tools and domains it may use, every tool call checked by Reno's policy before it runs, and a full record of what it did. The harness decides when a run is done; nothing caps a run by default.
- **GOAL-11** The owner answers, from one inbox, everything a harness would have asked them, for every run including unattended ones. On a harness Reno drives, Reno adds no permission layer of its own; on Reno's own harness, Reno's policy is that harness's permission system.

## Non-goals

- Reno never wraps or overrides a harness it drives: its loop, tools and permission system stay its own. Reno adds one harness of its own beside them.
- No multi-device sync, cloud execution, mobile clients, voice, or plugin marketplace.
- No Windows or Linux packaging.
- ~~No browser or computer-use tooling of Reno's own for agents~~ retired 2026-11-22: a bot on Reno's own harness gets a computer (GOAL-10).

## Scope boundaries

- Local-first on macOS. The kernel keeps no macOS-only code outside `renod` lifecycle and the Keychain, so Linux stays reachable.
- Owner-started runs, and the tasks a lead the owner started hands out, load the harness's own config like a terminal. Unattended runs (triggers and schedules with no owner session) are isolated from it.
- Whose permissions decide: a run the owner starts on a harness Reno drives follows that harness's own permission system; a bot on Reno's own harness follows Reno's policy, and every tool call passes it before it runs; an unattended bot on another harness is isolated and trusted no further than its executor declares. On every harness Reno guards its own definitions repo and data dir (including its store), so definitions change only through GOAL-5 and GOAL-9.
- Reno never writes or refreshes a harness's auth or Keychain, and writes a harness setting only for an explicit owner action (an allow rule, the updater switch).
- Reno's own harness calls model providers with the owner's API keys, kept in the Keychain as Reno's own items; it never uses a subscription login. Unattended use of other harnesses under their subscription terms is still an open question; API-key auth is the documented path there.
- A bot's computer runs on the owner's Mac. It reaches the network only through `renod`, which applies that bot's domain allowlist. Usage and cost are shown, never capped by default.
- Reno's own harness is an existing agent harness that Reno drives and gates, not a loop written from scratch; a spike picks it between Pi and dsh. A GOAL-7 lead may be a bot on it.
