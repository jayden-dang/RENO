# ROAD-73 terminal-in-thread-chat

**Outcome:** Jayden uses a terminal as part of thread chat and brings its output into an agent request, following the Delta interaction direction.

**Examples:**
1. Jayden opens a terminal from the chat, runs a command and includes its output with a question for the agent.
2. Jayden returns to that terminal while the earlier conversation still shows the output that was sent.

**Must keep:** the distinction between the person's terminal and the harness's own commands and permissions.
**Off limits:** implementing before ROAD-33 completes; expanding ROAD-33.
**Decided:** terminal interaction is part of the thread-chat direction to frame after ROAD-33.
**Delegated:** study and try Delta's terminal interactions.
**Ask me:** terminal-before-first-message, output capture and background-process scope during the thread-chat frame.
**UI:** reference
**Reference:** [Delta Terminals](https://delta.dev/docs/agents/terminals), including output capture and shell reuse observed on 2026-11-23.
