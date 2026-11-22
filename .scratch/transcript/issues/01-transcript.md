
# ROAD-33 [TRANSCRIPT] Reading, finding and copying in a long conversation stays fast

Assignee: Jayden Đặng
Status: enhancement, ready-for-agent
Priority: high
Estimate: 8
Blocked by: none

## Problem

The requester's words: "Bây giờ cho tôi biết road-33 sẽ làm gì - xây dựng idea card cho nó."
Then: "In that long conversation, i think [a reference editor's] repo already solve it great solution - can you cehck it for figure out that first and should be learn from it."

A long conversation scrolls at p95 31.4 ms a frame against a signed-off target of 16.7 ms. The owner cannot select text across messages, search the conversation, or come back to where they were reading. Math shows as raw source. Diagrams, images and the visuals an agent posts do not show at all.

## Outcome

The owner reads, finds, selects and copies anything in a conversation of any length without a dropped frame:
- Replies show typeset math, drawn diagrams, images, and the images and diagrams an agent posts.
- ⌘F finds anything in the thread, closed folds included. Going to a match opens what hides it, and closing the search
  closes again only what it opened.
- A drag across messages copies what is on screen, in order.
- A thread opens where the owner left it, after a restart too.
- The reading look is the one the owner picked: a quiet find strip, result ticks on the prompt rail, and a round "jump
  to latest" button.

## Acceptance

- [ ] On 2,000 messages with every fold open, scrolling stays at or under 16.7 ms a frame at p95 on the app bench. One very long reply, and a search held open while a run streams, meet the same target.
- [ ] Typing a query in ⌘F shows "1 of 7" on the seeded thread. Enter opens turn 3's fold at the `cargo test` step. Esc closes the search, and closes again only the folds the search opened.
- [ ] Match case, whole word and regex change the count; an invalid pattern says why and shows no count.
- [ ] A drag from the middle of a prompt into the reply below, then ⌘C, pastes that text in order. Double, triple and quadruple click select a word, a line and a block. "Copy as Markdown" gives the source.
- [ ] Returning to a thread, in the same session or after a restart, shows the place where the owner left it.
- [ ] Page and line keys, a scrollbar, and "scroll to user message" move through the conversation. Opening content stops following the tail, and "jump to latest" follows it again.
- [ ] A code block's Copy copies only its code, and wrap toggles long lines. A file link opens that file at its line in the side panel. A right-click menu offers Open, Copy Link, Copy, Copy as Markdown and Copy Message, and works from the keyboard.
- [ ] Math is typeset, with a Typeset math switch and a size setting. Flowchart, sequence, class, state, ER, gantt and pie diagrams draw, with Preview and Code tabs and ⌘-scroll zoom.
- [ ] Worktree and inline (`data:`) images show, SVG included. An image from the web shows as a link card, and the app makes no network request for it.
- [ ] On the mocked daemon, an image or diagram an agent posts, and an image in a tool result, show in the conversation. On the real daemon, neither shows, and no placeholder shows either.
- [ ] Reasoning shows open while it streams. Edits show their file and diff, commands their status, and file reads their line numbers. An error turn shows a callout with "Copy error".
- [ ] Callouts, wide tables, footnotes, front matter and task boxes draw.
- [ ] The owner has seen the additions made after their pick in the running app (the find toggles, the scrollbar, the turn-end actions, the menu, the diagram tabs and zoom, the tool cards and the settings rows) and said go, before this lands.
- [ ] Every transcript behaviour that worked before keeps working, with its tests unchanged.

## Out of scope

- Scripts that an agent writes and Reno runs (a road of their own).
- The real daemon side of the images and diagrams an agent posts (after the mocked-daemon milestone).
- Any change to how the daemon keeps run journals, or to the daemon's wire protocol.
- Editing a past prompt and regenerating from it (a road of its own).
- Comments anchored to passages (a road of its own, after this one).
- Left open by the owner's approval, so the defaults stand:
  - Loading images from the web: they stay a link card, and nothing is fetched.
  - Stopping one tool call without stopping the run: not built; it would need a protocol change.
  - How it is built (step by step or in waves): chosen when the build starts.

Requirements covered: TRANSCRIPT-1.1, TRANSCRIPT-1.2, TRANSCRIPT-1.3, TRANSCRIPT-1.4, TRANSCRIPT-1.5, TRANSCRIPT-1.6, TRANSCRIPT-1.7, TRANSCRIPT-1.8, TRANSCRIPT-1.9, TRANSCRIPT-1.10, TRANSCRIPT-1.11, TRANSCRIPT-2.1, TRANSCRIPT-2.2, TRANSCRIPT-2.3, TRANSCRIPT-2.4, TRANSCRIPT-2.5, TRANSCRIPT-2.6, TRANSCRIPT-2.7, TRANSCRIPT-2.8, TRANSCRIPT-2.9, TRANSCRIPT-2.10, TRANSCRIPT-2.11, TRANSCRIPT-2.12, TRANSCRIPT-2.13, TRANSCRIPT-3.1, TRANSCRIPT-3.2, TRANSCRIPT-3.3, TRANSCRIPT-3.4, TRANSCRIPT-3.5, TRANSCRIPT-3.6, TRANSCRIPT-3.7, TRANSCRIPT-3.8, TRANSCRIPT-3.9, TRANSCRIPT-3.10, TRANSCRIPT-3.11, TRANSCRIPT-3.12, TRANSCRIPT-3.13, TRANSCRIPT-4.1, TRANSCRIPT-4.2, TRANSCRIPT-4.3, TRANSCRIPT-4.4, TRANSCRIPT-4.5, TRANSCRIPT-5.1, TRANSCRIPT-5.2, TRANSCRIPT-5.3, TRANSCRIPT-5.4, TRANSCRIPT-5.5, TRANSCRIPT-6.1, TRANSCRIPT-6.2, TRANSCRIPT-7.1, TRANSCRIPT-7.2, TRANSCRIPT-8.1, TRANSCRIPT-8.2, TRANSCRIPT-8.3, TRANSCRIPT-8.4, TRANSCRIPT-8.5, TRANSCRIPT-8.6, TRANSCRIPT-9.1, TRANSCRIPT-9.2, TRANSCRIPT-9.3, TRANSCRIPT-9.4, TRANSCRIPT-9.5, TRANSCRIPT-9.6, TRANSCRIPT-10.1, TRANSCRIPT-10.2, TRANSCRIPT-10.3, TRANSCRIPT-10.4, TRANSCRIPT-10.5, TRANSCRIPT-10.6, TRANSCRIPT-11.1, TRANSCRIPT-11.2, TRANSCRIPT-11.3, TRANSCRIPT-11.4, TRANSCRIPT-11.5, TRANSCRIPT-12.1, TRANSCRIPT-12.2, TRANSCRIPT-12.3, TRANSCRIPT-12.4, TRANSCRIPT-12.5, TRANSCRIPT-13.1, TRANSCRIPT-13.2, TRANSCRIPT-13.3, TRANSCRIPT-13.4, TRANSCRIPT-13.5, TRANSCRIPT-13.6, TRANSCRIPT-13.7, TRANSCRIPT-13.8, TRANSCRIPT-13.9, TRANSCRIPT-13.10, TRANSCRIPT-13.11, TRANSCRIPT-13.12, TRANSCRIPT-13.13, TRANSCRIPT-13.14, TRANSCRIPT-13.15, TRANSCRIPT-13.16, TRANSCRIPT-13.17, TRANSCRIPT-13.18, TRANSCRIPT-14.1, TRANSCRIPT-14.2, TRANSCRIPT-14.3, TRANSCRIPT-14.4, TRANSCRIPT-15.1, TRANSCRIPT-15.2, TRANSCRIPT-15.3, TRANSCRIPT-15.4, TRANSCRIPT-16.1, TRANSCRIPT-16.2, TRANSCRIPT-16.3, TRANSCRIPT-16.4, TRANSCRIPT-16.5, TRANSCRIPT-16.6, TRANSCRIPT-16.7, TRANSCRIPT-16.8, TRANSCRIPT-16.9, TRANSCRIPT-16.10, TRANSCRIPT-16.11, TRANSCRIPT-16.12, TRANSCRIPT-16.13
Plan: docs/specs/2026-11-22-transcript/tasks.md
Roadmap: ROAD-33

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

### 2026-11-22 — screen

The screen waits on Jayden's pick. Three variants of the transcript's reading tools run in the app on the mocked daemon:
- A: a quiet find strip;
- B: match ticks on the rail;
- C: a Find results tab.

The requirements are written and reviewed, and they wait for the sheet.

### 2026-11-22 — spec closed, waiting on Jayden

The spec is written and reviewed, and it waits for Jayden's approval. Nothing is published, and every spec file stays Draft.

What Jayden needs to decide:
1. Approve the roadmap change on the branch: ROAD-59 (edit a past prompt and regenerate) is deferred, ROAD-60 (comments on the transcript) is added to MILE-10, and ROAD-33 is bound to code TRANSCRIPT. The roadmap stays Draft until then.
2. Whether remote images in replies should load. Today they show as a link card and nothing is fetched.
3. Whether stopping one tool call without stopping the run is a road of its own. It needs a protocol change.
4. Whether to keep paging keys, a scrollbar and "Scroll to user message", which the "everything the reference ships" scope requires but nothing else does.
5. The build mode for the 29-task plan, and whether to keep the second UI stop before landing.
