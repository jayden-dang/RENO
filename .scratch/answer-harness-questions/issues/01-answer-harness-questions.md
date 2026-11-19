# ROAD-1 The owner answers a harness's questions from Reno

Assignee: Jayden Đặng
Status: enhancement, status:shaping
Priority: high
Estimate: 3
Blocked by: none

## Problem

The requester's words: "Road 1 sẽ làm gì?" and "viết card cho nó với /idea road-1".

From the approved vision (GOAL-2): "The owner answers, from one inbox, everything a harness would have asked them, for every run including unattended ones."

Today a harness that asks the owner a question in a Reno run waits, then times out and goes on without the owner's answer, because nothing in Reno can send the answer back.

## Outcome

The owner answers, from one inbox, every question a harness asks in any Reno run, attended or unattended; the harness gets exactly that answer and the run goes on, and a question that can no longer be answered says why.

## Acceptance

- [ ] A question a harness asks shows in the owner's inbox with its choices, and the owner's answer reaches that harness and the run continues
- [ ] A second answer to the same question is refused
- [ ] After a daemon restart, a question that was waiting no longer shows as open, and its history says it was closed by the restart
- [ ] An answer the harness cannot take (free text where only choices are allowed, too long, too many picks) is refused with what is allowed, and nothing reaches the harness
- [ ] A question from an unattended run waits for the owner instead of expiring

## Out of scope

- Answering from the app
- A lead agent answering for its child
- Any change to approvals or their time limit
- Not counting wait time against the run's time budget
- Letting Cursor ask questions

Roadmap: ROAD-1 (MILE-1 Harness parity). Card: docs/roadmap/roads/ROAD-1.md

## Comments

### 2026-11-19 — frame

Tier 2: a multi-task feature (a store migration, new protocol types and a feature flag, the answer path through the kernel and run supervisor, six drivers reporting what each question accepts, CLI commands, tests at every layer).

Neighbours: none in the feature catalog (it has no entries yet). The nearest shipped code is the approval flow (list, decide, history, watch, approve/deny), which this follows as its pattern; it is not catalogued yet (`/map-features` can register it).

Open questions for the spec (none blocks shaping):
1. Store each question set and answer as whole documents, or in columns.
2. The full list of reasons a question closes without an answer.
3. The CLI syntax for answering a set of several questions at once.
