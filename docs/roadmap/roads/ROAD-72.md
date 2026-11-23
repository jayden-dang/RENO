# ROAD-72 thread-context-and-invocations

**Outcome:** Jayden chooses relevant context and harness actions from the thread chat using Delta's mention, command and skill interactions as the reference.

**Examples:**
1. Jayden uses an @ mention to point a request at a file or another thread and can review what context the request will include.
2. Jayden discovers a command or skill through / or $, sees which harness it belongs to, and chooses when to send the request.

**Must keep:** each harness's supported commands, skills and configuration; unsupported capabilities remain distinguishable.
**Off limits:** implementing before ROAD-33 completes; silently replacing a harness's execution loop.
**Decided:** mentions, commands and skills belong in the new thread-chat design informed by the Delta research.
**Delegated:** research the interactions and candidate features.
**Ask me:** context inclusion boundaries, trigger behavior and delivery scope during the thread-chat frame.
**UI:** reference
**Reference:** [Delta Threads](https://delta.dev/docs/agents/threads) and [Skills](https://delta.dev/docs/agents/skills).
