# Reno

Reno is a local-first, Rust-native daemon for macOS that coordinates existing coding harnesses (Claude Code, Codex, Pi, opencode, Cursor, any ACP agent). It does not run a model loop of its own: it decides who does what, when, in which workspace, and whether the result was good enough. The pieces are the daemon `renod`, the CLI `reno` and the GPUI app `reno-app`.

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

## Relationships

- A **Bot** has many **Triggers** and lists the **Executors** it may run on
- A **Run** executes on one **Executor** and may raise many **Approvals**
- A **Thread** owns one worktree and holds many **Runs**

## Flagged ambiguities

- "driver" vs "harness": harness is the layer, a driver is one executor's module inside it
