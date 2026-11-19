# Product Vision: Reno

Status: Approved
Date: 2026-11-19

## Problem

Coding harnesses (Claude Code, Codex, Pi, opencode, Cursor, the ACP agents) are each strong alone, but each runs alone, in its own terminal with its own sessions. Nothing hands a task to the harness best suited for it, lets one agent break work down and give the parts to others, runs automation bots on a schedule or an event, or gathers what every agent needs from the owner into one place. Tools that try usually wrap the harnesses in a new agent loop and permission layer, and lose the harness's own settings, allow-lists, MCP servers and features.

## Users

A developer on their own Mac who already uses several agent CLIs and wants them to work as one team, and who wants bots that automate recurring work of any kind, not only code, created by chatting with Reno. The owner is the first user.

## Goals

- **GOAL-1** Each task goes to the executor that suits it, with the reason shown before it is sent, and falls back to the next when one is signed out or missing.
- **GOAL-2** The owner answers, from one inbox, everything a harness would have asked them, for every run including unattended ones. Reno adds no permission layer of its own.
- **GOAL-3** Several bots work one goal in parallel, each in its own worktree, coordinating through boards and messages, and a flow survives a daemon restart.
- **GOAL-4** When a bot or flow asks for it, work is checked before it is called done: tests first, then a verifier on a different harness, then the owner's verdict.
- **GOAL-5** Definitions improve only under a gate: a proposal is replay-tested, needs the owner's approval, and reverts itself if it does worse.
- **GOAL-6** Everything runs on the owner's machine with the logins they already have: no Reno account and no cloud service. Reno never touches a harness's auth or Keychain, and writes a harness allow rule only when the owner clicks.
- **GOAL-7** A lead agent breaks work down and hands the parts to the right running harnesses through Reno's MCP or ACP, so one workflow combines several harnesses. The lead can be any harness, including the owner's own terminal session.
- **GOAL-8** A run the owner starts behaves as the same harness started in a terminal in the same directory: same settings, allow-list, hooks, MCP servers and features. A conformance check proves it for each driver.
- **GOAL-9** The owner creates or changes a bot by chatting with Reno. The change lands as a diff in the definitions repo and takes effect only after the owner accepts it.

## Non-goals

- Reno does not replace a harness's own loop, tools or permission system.
- No multi-device sync, cloud execution, mobile clients, voice, or plugin marketplace.
- No Windows or Linux packaging.
- No browser or computer-use tooling of Reno's own for agents; a harness's own works as it does in a terminal.

## Scope boundaries

- Local-first on macOS. The kernel keeps no macOS-only code outside `renod` lifecycle and the Keychain, so Linux stays reachable.
- Owner-started runs, and the tasks a lead the owner started hands out, load the harness's own config like a terminal. Unattended runs (triggers and schedules with no owner session) are isolated from it.
- Reno adds no permission layer, and guards only its own definitions repo and data dir (including its store), so definitions change only through GOAL-5 and GOAL-9.
- Reno never touches a harness's auth or Keychain and writes an allow rule only on the owner's click.
- Unattended use under each harness's subscription terms is an open question; API-key auth is the documented path for unattended runs.
- How the GOAL-7 lead is built (a lead bot on an existing harness, or a loop borrowed into Reno) stays open until a spike.
