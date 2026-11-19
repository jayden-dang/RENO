# UI brief: following a run — plan, background tasks, subagents (ROAD-35)

Status: Locked
Date: 2026-11-19
Screenshot: [2026-11-19-run-activity-ui-brief.png](./2026-11-19-run-activity-ui-brief.png)
Inherits: [2026-11-19-app-shell-ui-brief.md](./2026-11-19-app-shell-ui-brief.md) (tokens, type, status bar, transcript, composer, side panel are unchanged)

## Decision

Hybrid **"Trays + Session"**: direction A's trays above the composer, plus direction B's Plan / Running / Agents sections in the side panel's Session tab. Direction C (a floating activity island) was dropped.
The owner's words: "Tôi đồng ý với đề xuất, kết hợp A + B", then "lock và implement nó."

## Grounding

Token source of record: the `Theme` set in the app-shell brief (no new tokens). Typefaces: Geist for UI text, Geist Mono for commands, output and times. Icons: Phosphor. Spacing: `panel::layout::GUTTER`.

## Job (worked-in screen)

- **Question:** What is this run doing, and has it left anything running?
- **Finding:** one sentence, read first, in the plan tray: "Step 3 of 5 · Running the test suite — 1 subagent researching the ACP v2 spec — 1 background dev server on :5173 for 12 min · +1 more".
- **Primary job:** stop a background task, or open a subagent's transcript; each is one click from where the finding is read.

## Surfaces

### Plan tray (above the composer)

- **Layout:** a rounded card attached above the composer, inside the conversation column. Collapsed, it shows a header line "Plan · <done>/<total> · <current step>" (the current step in accent) with a disclosure chevron, and the finding sentence beneath it. Expanded, it adds the checklist below a hairline.
- **Components:**
  - rung 1 — `ui/kit.rs` `icon_button`, `focus_ring`, `spinner`.
  - new (rung 7) — the tray card and the checklist rows (done ✓ muted, in progress spinner + accent, pending ○), because no kit piece is a collapsible list attached to the composer.
- **States:**
  - The run has no plan, tasks or agents: no tray.
  - The plan advances live.
  - Collapsed or expanded; the choice is kept per run while the app is open.
- **Type & color:** Geist 13 header and rows; finding in `muted`; current step `accent`.
- **A11y:** the header is a disclosure button with a label naming progress; rows are read as a list.

### Background chip and task tray

- **Layout:**
  - A pill "● N running" right-aligned on its own row between the plan tray and the composer.
  - Clicking it opens the task tray above the chip: one row per task. Each row shows the command (mono), a port tag when there is one, the uptime, "Output ›" and Stop, with the last output line beneath.
- **Components:**
  - rung 1 — `kit::tag` (the port), `kit::icon_button` (Stop), `kit::tip`.
  - new (rung 7) — the pill and the task rows, because no kit piece lists processes.
- **States:**
  - Task: running; Stopping… (on press); Stopped (with "ran N min" and the final line).
  - A stop that fails says why inline under the task ("Could not stop npm run dev: it already ended." / "…: it is no longer listed.") until the next press.
  - The chip counts running tasks only and disappears at 0.
- **Interaction:**
  - Stop takes effect on press: the row reads Stopping… at once and Stopped when the daemon confirms. Tray and Session always show the same state.
  - "Output ›" opens the side panel on Session (opening the panel if hidden) and never takes the composer's focus.
- **Type & color:** command and output in Geist Mono 12.5; running dot `accent`, stopped dot `faint`.
- **A11y:** Stop and Output are labelled buttons naming their task.

### Subagent and side-chat chips (in the transcript)

- **Layout:** a pill under the agent message after which the agent was spawned: "↳ ● <name> · <state> · N steps" (subagent) or "↳ ● Side chat · <topic> · answered". Clicking it opens that agent's transcript as a side-panel tab.
- **Components:**
  - rung 2 — over the transcript's tool rows.
  - new (rung 7) — the chip, because no transcript row opens another conversation.
- **States:** running (accent dot); finished or answered (faint dot). Chips are not shown while an approval panel replaces the composer.
- **A11y:** each chip is a button with a label naming the agent and its state.

### Session tab: Plan, Running, Agents

- **Layout:** three sections above the existing Run metadata.
  - **Plan N/M:** the checklist; the current step is highlighted.
  - **Running N:** each task with command, port, uptime, Output (toggles a mono output block of the last lines) and Stop.
  - **Agents N:** one lane per subagent or side chat, with a status stripe on its left edge, the name and what it is doing, steps, state, and its last line. Clicking a lane opens its tab.
- The Session tab does **not** repeat the finding sentence; the tray says it.
- **Components:**
  - rung 1 — the existing panel views and section headings.
  - rung 2 — lanes over the All runs lane pieces (status stripe, tail).
- **States:**
  - Each section is absent when empty.
  - Stopped tasks stay listed with "Stopped".
  - Agent state: Running in accent, Finished or Answered muted.
- **Type & color:** Geist 13 titles; Geist Mono 12 output, lines and steps; tabular numbers.

### Agent tab

- **Layout:** a side-panel tab titled with the agent's name (side chats read "Side chat"). The header is the name and what it is doing, plus a state tag; below it is the agent's transcript (prompt as a bubble, tool lines muted, agent text bare), ending in "Working" while it runs.
- **Components:** rung 1 — the panel tab strip and `Surface::SideChat`; rung 2 — transcript row styles.

### Real daemon vs. not-yet controls

There is no daemon support yet (ROAD-52), so `Capabilities::REAL.run_activity` is false. With the real daemon none of these surfaces show. They show on the fake daemon (`--fake`).

## Contract

`reno_client::activity`, mocked by the fake daemon; ROAD-52 builds the real implementation to this shape.

- `ActivityDesk::activity(run) -> RunActivity`: the run's plan, background tasks and agents. It is empty for a run with none. No error is defined.
- `ActivityDesk::stop_task(run, task) -> Result<(), StopError>`: the task reads `Stopping` at once and `Stopped` when it has ended. The screen handles two errors:
  - `TaskNotRunning`: already ended or already stopping.
  - `UnknownTask`: no such task in the run.
- `RunActivity { plan: [PlanItem { text, doing, status: Done|InProgress|Pending }], tasks: [BackgroundTask { id, command, port?, uptime_ms, state: Running|Stopping|Stopped, tail: [line] }], agents: [AgentLane { id, kind: Subagent|SideChat, name, doing, state: Running|Finished, lines: [AgentLine { role: Prompt|Agent|Tool, text }], finding?, spawned_after, expected_steps }] }`
  - `spawned_after` is the index of the lead's agent message the agent was spawned after (it places the chip).
- App entry points: `Store::run_activity(run) -> Option<RunActivity>` (`None` when the capability is off or the run has nothing); `Store::stop_task(run, task)`; `Store::stop_error(run, task)`.

## Signature

What a run left running is answered beside the composer, one click from stopping it, and the full picture sits in the Session tab without repeating it.

## Amendments (owner, during review)

1. A + B hybrid chosen; C (activity island) dropped.
2. Agent-flagged and applied before lock, consistent with the app-shell brief's "no repeated run status":
   - the finding sentence appears only in the tray, not in Session;
   - B's status-bar count chip is dropped;
   - each task row in the tray has "Output ›" to the Session output, without taking focus.

## Code and tests

- Code:
  - `crates/reno-client/src/activity.rs` (contract);
  - `crates/reno-client/src/fake/activity.rs` (fake);
  - `apps/reno-app/src/vm/activity.rs`;
  - `apps/reno-app/src/screens/run/activity.rs`;
  - `apps/reno-app/src/screens/run/session.rs`.

  The fake is switched by the app's `--fake` mode (`FakeConfig::activity_runs`, set in `start_fake`), and the controls are gated by `Capabilities::run_activity`.
- Lock tests: `apps/reno-app/src/activity_tests.rs`, each seen failing once against a broken guard.
