# Reno

Reno is a local-first, Rust-native daemon for macOS that coordinates existing coding harnesses (Claude Code, Codex, Pi, opencode, Cursor, any ACP agent). It decides who does what, when, in which workspace, and whether the result was good enough. The only model loop it runs is the **Reno harness**, an existing harness it drives and gates for bots. The pieces are the daemon `renod`, the CLI `reno` and the GPUI app `reno-app`.

## Language

**Executor**:
An agent CLI (or SDK shim) that Reno drives, as listed by `reno executor list` with what the daemon found out about each.
_Avoid_: provider, backend, model

**Harness**:
The layer that speaks one executor's wire format (`reno-harness`).
_Avoid_: driver (a driver is one harness's implementation module)

**Bot**:
A named, versioned definition (TOML in the defs repo) of what to run, on which executors, under which limits. A run is pinned to the bot version in force when it started.

**Run**:
One execution of a prompt on an executor, with a transcript of events that survives daemon restarts.

**Thread**:
A conversation that owns a worktree; a follow-up task continues the thread's branch.

**Trigger**:
A schedule or event that starts a run for a bot; held while paused and caught up on resume.

**Approval**:
A pending action a run is waiting on. Only the owner approves or denies; a standing **rule** can answer the same action again.

**Policy**:
The definition that decides what a run may do without asking.

**Reno harness**:
The executor Reno runs for bots: an existing agent harness that Reno drives with the owner's API keys and gates, so every tool call passes the **Policy** before it runs.
_Avoid_: own loop, native agent, built-in agent

**Gate**:
Reno's extension inside the **Reno harness** that holds every tool call until the **Policy** has answered it, and
blocks the call when Reno cannot answer.
_Avoid_: hook, guard, sandbox

**Arming**:
Confirming, before a run's first prompt, that its **Gate** is loaded in the harness and connected to Reno for that run.
A run that cannot be armed does not start.
_Avoid_: handshake, gate check

**Bot computer**:
The Linux VM a bot on the **Reno harness** works in: a browser first, only its project mounted, network only through `renod`'s allowlist for that bot.
_Avoid_: sandbox, desktop, VM (alone)

## Relationships

- A **Bot** has many **Triggers** and lists the **Executors** it may run on
- A **Run** executes on one **Executor** and may raise many **Approvals**
- A **Thread** owns one worktree and holds many **Runs**

## Flagged ambiguities

- "driver" vs "harness": harness is the layer, a driver is one executor's module inside it
- "harness" also names an agent CLI in the vision and roadmap ("the harness's own settings"); the glossary's **Harness** is the layer. Unresolved; the **Reno harness** follows the vision's sense
