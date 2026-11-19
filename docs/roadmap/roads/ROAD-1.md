# ROAD-1 answer-harness-questions

<!--
The operator's intent for one road, written at /idea while they are at the keyboard.
`/shape --auto` reads every slot as their standing word; agents decide everything else.
At most 30 non-blank lines: a file path, a table name or an API shape belongs in the spec,
not here.
-->

**Outcome:** The owner answers, from one inbox, every question a harness asks in any Reno run, attended or unattended; the harness gets exactly that answer and the run goes on, and a question that can no longer be answered says why.

**Examples:**
1. Jayden runs a review bot on Codex at 23:00. Codex asks "Bump the lockfile too? (yes / no)". Next morning Jayden types `reno inbox`, sees it, types `reno answer q_7 yes`, and Codex carries on and updates the lockfile.
2. Jayden answers a question from Grok (ACP) with `reno answer q_9 --text "dùng staging"`. Reno says it takes only one of `Allow once`, `Always`, `Reject`, and sends nothing to Grok.

**Must keep:** approvals and their time limit work as today; old journals still replay; a run's own token cannot answer; each driver's own answer tests still pass; the journal append budget holds; the app keeps working as it does now.
**Off limits:** the app's answering UI; a lead answering for its child; changing approvals; not counting wait time against the run's time budget; letting Cursor ask questions.
**Decided:** one inbox for every question, unattended runs included (P2); no time limit of its own: a question waits until answered, the run ends, the harness gives up, or the run's time budget runs out; questions are durable records like approvals, closed with a reason after a restart; a question family of its own beside approvals, with one CLI view of both; Reno checks each answer against what that harness can take and refuses it plainly, never silently.
**Delegated:** None
**Ask me:** None
**UI:** none
