# ROAD-33 Reading, finding and copying in a long conversation stays fast

Assignee: Jayden Đặng
Status: enhancement, status:shaping
Priority: high
Estimate: 8
Blocked by: none

## Problem

The requester's words: "Bây giờ cho tôi biết road-33 sẽ làm gì - xây dựng idea card cho nó."
Then: "In that long conversation, i think [a reference editor's] repo already solve it great solution - can you cehck it for figure out that first and should be learn from it."

A long conversation scrolls at p95 31.4 ms a frame against a signed-off target of 16.7 ms. The owner cannot select text across messages, search the conversation, or come back to where they were reading. Math shows as raw source. Diagrams, images and the visuals an agent posts do not show at all.

## Outcome

The owner reads, finds, selects and copies anything in a conversation of any length without a dropped frame. Replies show typeset math, diagrams, images and the visuals an agent posts. All of it is designed on the mocked daemon and kept as real code.

## Acceptance

- [ ] On 2,000 messages with every fold open, scrolling stays at or under 16.7 ms per frame at p95. One very long reply meets the same target.
- [ ] A drag from one message into the next selects across both, and ⌘C copies that text in order.
- [ ] ⌘F counts matches across the whole conversation, including inside closed folds. Going to a match opens its fold. Closing the search closes again the folds it opened.
- [ ] Returning to a thread puts the view where the owner left it.
- [ ] A code block has its own Copy, and a file link opens that file in the side panel.
- [ ] Math is typeset, Mermaid blocks show as diagrams, and images show in replies. A visual an agent posts shows in the conversation on the mocked daemon, and does not show on the real daemon.
- [ ] The owner has picked one of three variants, and the pick is kept with lock tests.

## Out of scope

- Scripts that an agent writes and Reno runs (ROAD-58)
- The real daemon side of agent-posted visuals (after MILE-10)
- Any change to how the daemon keeps run journals

Roadmap: ROAD-33 (MILE-10 Workflow design on a mocked daemon). Card: docs/roadmap/roads/ROAD-33.md

## Comments

### 2026-11-22 — frame

Tier 2: a multi-task feature. It covers the list's row model and events, selection, search, scroll, links, rich blocks (math, diagrams, images), agent-posted visuals behind a mocked contract, and a draft-ui pass with three variants.

Neighbours in the catalog:
- APPSHELL (the conversation canvas, ROAD-30);
- RUNACTIVITY (subagent chips, ROAD-35);
- APPBENCH (the frame-time target, ROAD-29).

Nothing in the catalog owns the transcript itself. Agent-posted visuals were ROAD-44. That item is now retired: its static half moved here, and its scripted half is ROAD-58.

Open questions for the spec (none blocks shaping):
1. Is making each open fold's children their own list items enough to reach 16.7 ms? The cause was read from the code, not profiled. Measure it, and add a bench scenario for one very long reply. Split rows per markdown block only if that scenario is slow.
2. The math renderer. There are two permissive candidates (research note in docs/internal, 2026-11-22). Pick in design.
3. Does the per-frame text registration that cross-message selection needs fit the frame budget?
4. The shape of the mocked contract for agent-posted visuals, and its capability flag.
5. Can the scope be graded in one draft-ui pass, or does it need two (reading tools, then rich content)?
