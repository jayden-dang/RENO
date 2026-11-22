# Requirements: Reading a conversation (transcript)

Feature code: TRANSCRIPT
Status: Draft
Date: 2026-11-22

Roadmap item ROAD-33 (MILE-10, designed on the mocked daemon and kept as real code). Frame: the operator's confirmed frame
and the close package approved under the run's charter. "The reference agent panel" is the shipped product this feature
follows; its pieces and the ones not built are listed in Out of Scope. Labels: a criterion marked `(agent-decided)` or
`(Weigh)` carries a choice the operator delegated or that the evidence chose. No label means the operator's own words (the
ROAD-33 card) or an existing behaviour.

## 1. Scroll a long conversation with every fold open

**Story:** As the owner, I want to open every "Worked for" fold of a 2,000-message thread and scroll it top to bottom
without a stutter, so that reading never waits on the app.

- **TRANSCRIPT-1.1** THE SYSTEM SHALL draw the transcript as one list item per transcript entry (a prompt, a reply, a
  reasoning block, a tool group, a fold head, a card), and, WHILE a "Worked for" fold is open, as one list item per entry
  inside it, so that a frame lays out only the items in the viewport and its overdraw band. (agent-decided: row split)
- **TRANSCRIPT-1.2** WHEN a daemon event adds, grows or changes one entry THE SYSTEM SHALL lay out again only the list
  items of that entry, keeping the item under the scroll anchor where it is on screen. (agent-decided)
- **TRANSCRIPT-1.3** WHEN a turn finishes and its interim entries fold under a new "Worked for" head THE SYSTEM SHALL keep
  the entry the owner is reading at the same place on screen.
- **TRANSCRIPT-1.4** WHEN a restored thread opens THE SYSTEM SHALL parse a reply's markdown no earlier than its first draw,
  and SHALL hold an entry's measured height until its pending render lands, so that the list does not jump. (Weigh)
- **TRANSCRIPT-1.5** WHEN the owner opens or closes a fold THE SYSTEM SHALL keep the fold's head where it was on screen.
- **TRANSCRIPT-1.6** (guard) WHEN a test reads the run screen's rows THE SYSTEM SHALL CONTINUE TO return the nested row
  model (top-level rows, with a finished turn's interim rows nested in its "Worked for" row), so that every existing
  transcript lock test passes unchanged.
- **TRANSCRIPT-1.7** (guard) WHEN text deltas arrive in a row THE SYSTEM SHALL CONTINUE TO join them into one reply keyed by
  its first event, and to fold a finished turn's interim steps under one head ("Worked for <duration>" when the turn is one timed
  stretch, else "Worked"), leaving the final message bare.
- **TRANSCRIPT-1.8** (guard) WHEN the owner clicks a "Worked for" head THE SYSTEM SHALL CONTINUE TO open the fold in place.
- **TRANSCRIPT-1.9** (guard) WHILE a run streams and the view is at the tail THE SYSTEM SHALL CONTINUE TO follow the tail.
- **TRANSCRIPT-1.10** (guard) WHEN a thread's older runs have not changed THE SYSTEM SHALL CONTINUE TO not build their rows
  again on every change.
- **TRANSCRIPT-1.11** (guard) WHEN the bench runs THE SYSTEM SHALL CONTINUE TO honour `RENO_BENCH`, `RENO_BENCH_SCROLL`,
  `RENO_OPEN` and `RENO_OPEN_FOLDS`, and `renod --stream-mock=burst` SHALL CONTINUE TO emit 2,000 messages each followed by
  a tool step.

## 2. Select across messages and copy

**Story:** As the owner, I want to drag from the middle of my prompt to the middle of the reply below it and press ⌘C, so
that I paste exactly that text in order.

- **TRANSCRIPT-2.1** WHEN the owner drags across the transcript THE SYSTEM SHALL select text continuously across entries,
  messages and runs of the thread, in reading order.
- **TRANSCRIPT-2.2** WHILE a selection's start or end is scrolled out of view THE SYSTEM SHALL keep the selection and
  autoscroll the list while the drag is held past the list's edge.
- **TRANSCRIPT-2.3** WHEN the owner double-clicks a word, triple-clicks a line, or shift-clicks after a selection THE
  SYSTEM SHALL select that word, that line, or extend the selection to the click.
- **TRANSCRIPT-2.4** WHEN the owner presses ⌘C with a selection THE SYSTEM SHALL put on the clipboard the text the owner
  sees in the selected range, in order, with one blank line between entries. (Weigh)
- **TRANSCRIPT-2.5** WHEN the selected range covers a closed fold, collapsed reasoning, a collapsed user message or a cut
  tool output THE SYSTEM SHALL copy only the lines shown for it (for a closed fold, its head line such as
  "Worked for 3m 12s"). (Weigh)
- **TRANSCRIPT-2.6** WHEN the selected range covers typeset math, a diagram or an image THE SYSTEM SHALL copy its text
  equivalent: math as its TeX between `$…$` (inline) or `$$…$$` (display) whatever delimiter the source used, a diagram as
  its fenced source, and an image as its alt text in square brackets. (Weigh; the image alt text is agent-decided)
- **TRANSCRIPT-2.7** WHEN the selected range covers a file link drawn with a short label (new with 6.3; the parser's
  unused `file_label` field anticipates it) THE SYSTEM SHALL copy the link's path, not its label.
- **TRANSCRIPT-2.8** THE SYSTEM SHALL never select or copy control text: buttons and their labels, "Show N more lines",
  subagent chips, the "Thinking" line, hover times and the prompt tick rail. (Weigh)
- **TRANSCRIPT-2.9** WHEN the owner chooses "Copy as Markdown" on a selection THE SYSTEM SHALL copy the markdown source
  bytes of the selected range, original math delimiters included. (Weigh)
- **TRANSCRIPT-2.10** WHEN the owner clicks in the transcript outside the selection without dragging THE SYSTEM SHALL
  clear the selection.
- **TRANSCRIPT-2.11** (guard) WHEN a turn has finished THE SYSTEM SHALL CONTINUE TO offer its final message's quiet Copy,
  which copies that message's raw text.
- **TRANSCRIPT-2.12** (guard) WHEN the owner hovers a user message THE SYSTEM SHALL CONTINUE TO show its Copy icon (and,
  on the prompt, its time), a long one SHALL CONTINUE TO collapse with Show more / Show less, and the bubble SHALL
  CONTINUE TO show plain text.
  (agent-decided: the frame default of markdown in the bubble is set aside)

## 3. Find anything in the conversation

**Story:** As the owner, I want to press ⌘F, type `supervisor.rs`, see "1 of 7" and press Enter to land on turn 3's
`cargo test` step, so that I find anything without opening folds by hand.

- **TRANSCRIPT-3.1** WHEN the owner presses ⌘F on the run screen THE SYSTEM SHALL open a find bar above the transcript with
  its query field focused, or focus the query field when the bar is already open.
- **TRANSCRIPT-3.2** WHILE the find bar holds a query THE SYSTEM SHALL count matches across the whole thread, in prompts,
  replies, interim text, reasoning, commands and all tool output the app holds, inside closed folds too, and show
  "<current> of <total>", such as "1 of 7".
- **TRANSCRIPT-3.3** WHEN the owner presses Enter or ⌘G (next), or ⇧Enter or ⇧⌘G (previous), THE SYSTEM SHALL make the
  next or previous match current, wrapping at the ends, and scroll it into view.
- **TRANSCRIPT-3.4** WHEN the current match sits inside a closed fold, collapsed reasoning, a closed tool output or the
  cut part of a shown output THE SYSTEM SHALL open what hides it and scroll to the match.
- **TRANSCRIPT-3.5** THE SYSTEM SHALL highlight every visible match, and the current match with a distinct highlight.
- **TRANSCRIPT-3.6** WHEN the owner presses Esc in the find bar or clicks its close button THE SYSTEM SHALL close the bar,
  clear the highlights, and close again exactly the folds, reasoning and outputs that search opened. A fold the owner
  opened stays open.
- **TRANSCRIPT-3.7** WHEN the owner opens or closes a row that search had opened THE SYSTEM SHALL treat that row as the
  owner's from then on.
- **TRANSCRIPT-3.8** WHILE the find bar is open THE SYSTEM SHALL send Esc only to the find bar, never to stopping the run
  or to denying a held approval.
- **TRANSCRIPT-3.9** WHEN the owner turns on match case, whole word or regex (buttons, or ⌥⌘C, ⌥⌘W, ⌥⌘X) THE SYSTEM SHALL
  recount with that option. (Weigh: the reference's toggles)
- **TRANSCRIPT-3.10** IF the regex query does not parse THEN THE SYSTEM SHALL show the field in an error state with the
  reason, and show no count.
- **TRANSCRIPT-3.11** WHEN new events arrive while the find bar holds a query THE SYSTEM SHALL update the count without
  moving the current match, at most once per 150 ms. (Weigh)
- **TRANSCRIPT-3.12** WHILE a reasoning row is open only because it is streaming THE SYSTEM SHALL leave it open when search
  closes, and SHALL not close a row search opened when its stream ends while the find bar is open.
- **TRANSCRIPT-3.13** (guard) WHILE the find bar is closed THE SYSTEM SHALL CONTINUE TO deny the held approval on Esc when
  the approval panel has focus, and, where "Escape stops the agent" is on, to stop the run on Esc from the composer while
  Stop shows.

## 4. Come back to where I was reading

**Story:** As the owner, I want to leave a thread and come back to it in the same session and find the view where I left
it, so that I keep my place in a long conversation.

- **TRANSCRIPT-4.1** WHEN the owner leaves a thread's run screen THE SYSTEM SHALL remember, for that thread, the entry at
  the top of the view and the offset into it, in memory only. (Weigh; agent-decided: not on disk)
- **TRANSCRIPT-4.2** WHEN the owner returns to that thread while the app is still running THE SYSTEM SHALL show the
  remembered entry at the remembered offset.
- **TRANSCRIPT-4.3** IF the remembered entry is gone, or now sits inside a closed fold, THEN THE SYSTEM SHALL restore to
  that fold's head, else to the next entry after it in the thread.
- **TRANSCRIPT-4.4** WHEN a run's view moves from the run's key to its thread's key because the daemon named the thread
  THE SYSTEM SHALL carry the remembered place over.
- **TRANSCRIPT-4.5** WHEN the owner opens a thread with no remembered place THE SYSTEM SHALL open it at the tail,
  following new events.

## 5. Move through the conversation

**Story:** As the owner, I want to page through a conversation by keyboard and jump back to the latest message in one
click, so that I can reach any part of it without dragging the scrollbar.

- **TRANSCRIPT-5.1** WHILE the transcript has focus THE SYSTEM SHALL scroll by 90 % of the viewport on Page Up / Page Down,
  by three lines on ↑ / ↓, and to the start or end on Home / End.
- **TRANSCRIPT-5.2** WHEN the owner presses ⇧Page Up or ⇧Page Down THE SYSTEM SHALL scroll to the previous or next user
  message.
- **TRANSCRIPT-5.3** WHILE the view is not following the tail THE SYSTEM SHALL show a "jump to latest" control, and WHEN
  the owner uses it THE SYSTEM SHALL scroll to the end and follow the tail again. (Weigh; its look is agent-decided)
- **TRANSCRIPT-5.4** WHEN the owner opens a fold, a reasoning block, an output or a diagram zoom while following the tail
  THE SYSTEM SHALL stop following, so that what grew stays in view.
- **TRANSCRIPT-5.5** THE SYSTEM SHALL show a vertical scrollbar for the transcript beside the prompt tick rail.
- **TRANSCRIPT-5.6** WHEN the owner hovers the end of a finished turn THE SYSTEM SHALL offer "Scroll to user message" (that
  turn's prompt) and, on the last turn, "Scroll to top".
- **TRANSCRIPT-5.7** (guard) WHILE a transcript is taller than its view THE SYSTEM SHALL CONTINUE TO show the prompt tick
  rail, one tick per user message with the current one marked, and a click SHALL CONTINUE TO scroll to that message; a
  transcript that fits SHALL CONTINUE TO show no rail.

## 6. Copy code and follow links

**Story:** As the owner, I want to press a code block's Copy and click a `src/supervisor.rs:42` link in a reply, so that I
get just the code and land on that line in the Files tab.

- **TRANSCRIPT-6.1** WHEN the owner hovers a code block THE SYSTEM SHALL show its Copy and wrap buttons, and WHEN the owner
  presses Copy THE SYSTEM SHALL copy only the block's code.
- **TRANSCRIPT-6.2** WHEN the owner presses a code block's wrap button THE SYSTEM SHALL wrap or unwrap its long lines;
  unwrapped lines scroll sideways.
- **TRANSCRIPT-6.3** WHEN the owner clicks a link or an inline-code path (`path`, `path:line`, `path#Lline`) that resolves
  to a file in the run's worktree THE SYSTEM SHALL open that file in the side panel's Files tab at that line.
- **TRANSCRIPT-6.4** WHEN the owner clicks a web link (http or https) in the transcript THE SYSTEM SHALL open it only
  through the app's link opener, and IF the target is neither a web link nor a worktree file (a `mailto:` link included,
  which today opens directly) THEN THE SYSTEM SHALL leave the text unlinked. (default: the opener opens only web links
  from what a daemon says, apps/reno-app/src/links.rs:27)
- **TRANSCRIPT-6.5** WHEN the owner right-clicks in the transcript THE SYSTEM SHALL show a menu with, as they apply:
  Copy Link (on a link), Copy and Copy as Markdown (with a selection), Copy Message (on a reply), and Scroll to top or
  Scroll to bottom.
- **TRANSCRIPT-6.6** WHEN the owner clicks a footnote reference THE SYSTEM SHALL scroll to its footnote.
- **TRANSCRIPT-6.7** (guard) WHEN the Files tab previews a markdown file THE SYSTEM SHALL CONTINUE TO draw it with the
  transcript's markdown renderer, and an image file SHALL CONTINUE TO show in the Files tab.
- **TRANSCRIPT-6.8** (guard) WHEN a test runs THE SYSTEM SHALL CONTINUE TO forbid the link opener from reaching a real
  browser.

## 7. Read typeset math

**Story:** As the owner, I want `$$\int_0^1 x^2\,dx = \tfrac13$$` in a reply to show as typeset math, so that I read
formulas as formulas.

- **TRANSCRIPT-7.1** WHEN a reply holds display math (`$$…$$`, `\[…\]`) THE SYSTEM SHALL draw it typeset on its own line.
- **TRANSCRIPT-7.2** WHEN a reply holds inline math (`$…$`, `\(…\)`) THE SYSTEM SHALL draw it typeset in the line, aligned
  on the text's baseline.
- **TRANSCRIPT-7.3** IF a formula does not typeset THEN THE SYSTEM SHALL show its source as code, with no error dialog.
- **TRANSCRIPT-7.4** THE SYSTEM SHALL size math to the surrounding text and colour it from the theme. (agent-decided: no
  setting; see Open Questions S4)

## 8. See diagrams

**Story:** As the owner, I want a ```mermaid flowchart in a reply to show as a drawn diagram I can zoom, so that I read the
structure the agent described.

- **TRANSCRIPT-8.1** WHEN a reply holds a `mermaid` code block of a supported diagram type (at least flowchart, sequence, class, state,
  ER, gantt and pie) THE SYSTEM SHALL draw the diagram
  in its place, with Preview and Code tabs that switch between the drawing and its source.
- **TRANSCRIPT-8.2** WHILE a diagram is being drawn THE SYSTEM SHALL show its source with a "Rendering…" note, and keep the
  previous drawing while a changed source redraws.
- **TRANSCRIPT-8.3** IF a diagram does not parse or its type is not supported THEN THE SYSTEM SHALL show it as a plain code
  block.
- **TRANSCRIPT-8.4** WHEN the owner ⌘-scrolls over a diagram THE SYSTEM SHALL zoom it between fit-to-width and 200 %, show
  the zoom level with a reset button while zoomed, and scroll it sideways when it is wider than the column.
- **TRANSCRIPT-8.5** WHEN the theme changes THE SYSTEM SHALL draw diagrams again in the new theme's colours.
- **TRANSCRIPT-8.6** WHEN the owner presses a diagram's Copy THE SYSTEM SHALL copy its source.

## 9. See images in replies

**Story:** As the owner, I want `![build graph](docs/graph.png)` in a reply to show the picture from the run's worktree, so
that I see what the agent points at.

- **TRANSCRIPT-9.1** WHEN a reply holds an image whose source is a path in the run's worktree THE SYSTEM SHALL load it
  through the daemon's file read and show it scaled down to the column width.
- **TRANSCRIPT-9.2** WHEN a reply holds a `data:` image THE SYSTEM SHALL decode it in the app and show it the same way.
- **TRANSCRIPT-9.3** IF an image cannot be loaded (missing, unreadable, an unsupported type, or the thread's worktree is
  gone) THEN THE SYSTEM SHALL show "Image not loaded: <alt or path>" in its place.
- **TRANSCRIPT-9.4** WHEN a reply holds an http or https image THE SYSTEM SHALL show a link card with its alt text and host
  that opens in the browser through the app's link opener, and SHALL make no network request for it. (Weigh; see Open
  Questions S2)
- **TRANSCRIPT-9.5** WHEN the owner clicks an image wrapped in a link THE SYSTEM SHALL follow the link as in 6.3 and 6.4.

## 10. See what an agent posts

**Story:** As the owner, I want an image or diagram that an agent posts to show in the conversation where it was posted,
so that I see it without opening another tool. (On the mocked daemon during MILE-10.)

- **TRANSCRIPT-10.1** WHERE the connection offers agent-posted visuals THE SYSTEM SHALL show each visual a run posts, an
  image or a diagram, in the transcript after the entry it was posted with, titled with its caption.
- **TRANSCRIPT-10.2** WHERE the connection offers images in tool results THE SYSTEM SHALL show a tool result's image under
  its call, scaled down to at most 384 × 384 px, with "Go to file" when the call names a file.
- **TRANSCRIPT-10.3** WHERE the connection does not offer agent-posted visuals or tool-result images (the real daemon
  today) THE SYSTEM SHALL show none and no placeholder for them.
- **TRANSCRIPT-10.4** IF a posted visual fails to load or to draw THEN THE SYSTEM SHALL show its caption with
  "Visual not loaded".
- **TRANSCRIPT-10.5** (guard) WHEN the app connects to the real daemon THE SYSTEM SHALL CONTINUE TO turn every mocked
  capability off.

## 11. Read thinking and tool calls as they stream

**Story:** As the owner, I want to watch a run's reasoning and tool calls appear smoothly and then fold away, so that I
follow a live run without it jumping.

- **TRANSCRIPT-11.1** WHEN reply text streams in THE SYSTEM SHALL reveal it in steps of 16 ms, catching up with the
  received text within 200 ms of the last delta, and SHALL stop the reveal timer once caught up. (agent-decided: streaming pace)
- **TRANSCRIPT-11.2** WHILE reduce motion is on THE SYSTEM SHALL show received text at once.
- **TRANSCRIPT-11.3** WHILE a reasoning block streams THE SYSTEM SHALL show it open, scrolled to its newest line, and WHEN
  it ends THE SYSTEM SHALL close it unless the owner opened or closed it meanwhile. (Weigh: the reference's default)
- **TRANSCRIPT-11.4** WHEN a new edit call with a diff or a new command call arrives THE SYSTEM SHALL show it expanded, and
  other tool calls collapsed. (Weigh)
- **TRANSCRIPT-11.5** WHEN a tool call edits one file THE SYSTEM SHALL show the file's badge with "Go to file", and its
  diff inline when expanded. (Weigh)
- **TRANSCRIPT-11.6** WHEN a tool call shows a command THE SYSTEM SHALL show the command with its own Copy, its elapsed time
  and its exit status. (Weigh)
- **TRANSCRIPT-11.7** WHEN a file-read call's output has line numbers THE SYSTEM SHALL show it as a numbered code block with
  Copy, and WHEN the owner opens "Raw input" on a call THE SYSTEM SHALL show the call's input. (Weigh)
- **TRANSCRIPT-11.8** WHEN a turn ends in an error THE SYSTEM SHALL show the error in a callout with "Copy error". (Weigh)
- **TRANSCRIPT-11.9** THE SYSTEM SHALL hide an empty reply entry and a tool call canceled before any output. (Weigh)
- **TRANSCRIPT-11.10** (guard) WHILE a live run is between steps THE SYSTEM SHALL CONTINUE TO show the "Thinking" line under
  the last row, on the shared pulse clock.
- **TRANSCRIPT-11.11** (guard) WHEN a run asks for an approval or an answer THE SYSTEM SHALL CONTINUE TO show it as one
  muted line in the transcript, and let the owner approve, deny or answer it in the dock above the composer.
- **TRANSCRIPT-11.12** (guard) WHEN the owner opens a tool call with output THE SYSTEM SHALL CONTINUE TO show its first
  lines, "Show N more lines" ("Show 1 more line"), "Output was cut: <size> total" when the daemon cut it, and a Copy that
  copies the whole output it holds; a call without output SHALL CONTINUE TO add no block.
- **TRANSCRIPT-11.13** (guard) WHERE the connection offers run activity (the fake daemon today) THE SYSTEM SHALL CONTINUE
  TO show a subagent's chip under the top-level reply that spawned it, and a click SHALL CONTINUE TO open its tab.
- **TRANSCRIPT-11.14** (guard) WHEN a reply streams THE SYSTEM SHALL CONTINUE TO reparse only from the last stable block,
  and to mend a hanging `**`, `*`, backtick, `~~` or link in the last block.

## 12. Read rich markdown

**Story:** As the owner, I want tables, callouts and footnotes in a reply to read as they would on a docs page, so that
long structured answers stay readable.

- **TRANSCRIPT-12.1** WHEN a table is wider than the column THE SYSTEM SHALL scroll it sideways with its own scrollbar.
- **TRANSCRIPT-12.2** WHEN a block quote starts with `[!NOTE]`, `[!TIP]`, `[!WARNING]` or `[!CAUTION]` THE SYSTEM SHALL draw
  it as a callout in that kind's colour and label.
- **TRANSCRIPT-12.3** THE SYSTEM SHALL draw footnotes at the end of their reply, hide HTML comments, and draw a leading
  front-matter block as a small key-value table.
- **TRANSCRIPT-12.4** THE SYSTEM SHALL draw task-list items with a checkbox that shows their state and does not toggle.

## 13. Try every reading tool on the mocked daemon

**Story:** As the owner, I want to open seeded threads on `reno-app --fake` that hold both lock examples, so that I click
through and grade the reading tools on mocked data.

- **TRANSCRIPT-13.1** WHERE the app runs on the fake daemon THE SYSTEM SHALL offer a finished thread "fix the ACP v2
  handshake" of 2,000 messages, with "Worked for" folds, in which `supervisor.rs` occurs exactly 7 times in text the app
  holds, the first in turn 3's `cargo test` step.
- **TRANSCRIPT-13.2** WHERE the app runs on the fake daemon THE SYSTEM SHALL offer a thread whose reply holds
  `$$\int_0^1 x^2\,dx = \tfrac13$$`, a Mermaid flowchart, a code block, a worktree image, a remote image, a file link and an
  agent-posted visual.
- **TRANSCRIPT-13.3** WHERE the app runs on the fake daemon THE SYSTEM SHALL offer a run with one very long streaming reply
  that has code blocks and math.
- **TRANSCRIPT-13.4** (guard) WHERE the app runs on the fake daemon THE SYSTEM SHALL CONTINUE TO offer its seeded runs and
  the `/burst N`, `/long`, `/refused` and `/fails` prompt scripts as they behave now.

## 14. Quality attributes

**Section-kind:** nfr

**Story:** As a stakeholder, I want measurable quality targets for this feature, so that how-well is not left implicit.

- **Performance:** No standing metrics doc (docs/product/metrics.md absent); targets come from the operator's Lock and the
  signed-off budgets (docs/BENCHMARKS.md).
  - **TRANSCRIPT-14.1** WHEN `cargo xtask bench --app` runs the long-transcript scenario (2,000 messages, every fold open)
    THE SYSTEM SHALL keep frame time at or under 16.7 ms at p95, and the `long_frame_p95_ms` row SHALL fail the run when
    over (today it is a target that does not fail, xtask/src/app_stats.rs:86-91) — verified by the bench.
  - **TRANSCRIPT-14.2** WHEN the bench runs a new one-very-long-reply scenario THE SYSTEM SHALL keep frame time at or under
    16.7 ms at p95 — verified by the bench.
  - **TRANSCRIPT-14.3** THE SYSTEM SHALL stay inside the signed-off long-transcript peak footprint (≤ 370 MiB), streaming CPU
    (≤ 15 %) and peak footprint (≤ 260 MiB), idle CPU (< 1 %), idle wakeups (< 1 per second) and cold start (< 400 ms)
    budgets — verified by the bench.
  - **TRANSCRIPT-14.4** THE SYSTEM SHALL bound every cache this feature adds (math, diagrams, images, search, text layout)
    by entry count or bytes — verified by unit tests at the bound.
- **Security:** No threat model (docs/security/threat-model.md absent). The trust boundary crossed is untrusted agent output
  reaching the app.
  - **TRANSCRIPT-14.5** THE SYSTEM SHALL make no outbound network request because of anything in a transcript — verified by
    a lock test whose counting HTTP client sees zero sends for a reply with an https image and links.
  - **TRANSCRIPT-14.6** THE SYSTEM SHALL open links from transcript content only through the app's link opener, which
    tests forbid from reaching a real browser — verified by lock tests.
  - **TRANSCRIPT-14.7** THE SYSTEM SHALL read local images only through the daemon's worktree-rooted file read — verified by
    a lock test with a path outside the worktree that shows "Image not loaded".
  - **TRANSCRIPT-14.12** THE SYSTEM SHALL pin each new crate (the math renderer, `merman`) to an exact version, and the
    workspace SHALL pass `cargo deny check` and `cargo machete` — verified in the verify run.
- **Reliability:** No standing reliability doc (docs/ops/reliability.md absent). The feature adds no durable state; what it
  must keep durable is the existing contracts (Compat obligation Internal, docs/agents/project.md:24).
  - **TRANSCRIPT-14.10** (guard) THE SYSTEM SHALL CONTINUE TO match the protocol schema snapshot unchanged: no new event,
    feature flag or method — verified by `protocol_schema_matches_snapshot`.
  - **TRANSCRIPT-14.11** (guard) THE SYSTEM SHALL CONTINUE TO read and write `panel-state.json` and `app-settings.json` in
    their current shapes — verified by the existing persist and prefs tests.
- **Accessibility:** No standing accessibility doc (docs/standards/accessibility.md absent).
  - **TRANSCRIPT-14.8** THE SYSTEM SHALL make every new control (find bar and its toggles, jump to latest, Copy and wrap
    buttons, diagram tabs and zoom reset, the context menu) reachable and operable by keyboard alone — verified by GPUI
    keystroke tests.
  - **TRANSCRIPT-14.9** THE SYSTEM SHALL draw highlights, the current match, callouts and diagram colours from theme tokens
    in both light and dark themes — verified by lock tests on both themes.

## Guard map

Every file the change touches, and the guards that protect its existing behaviour:

| File | Guards |
|---|---|
| `apps/reno-app/src/transcript.rs` | 1.6, 1.7 |
| `apps/reno-app/src/screens/run.rs` | 1.6, 1.8, 1.9, 1.10, 5.7 |
| `apps/reno-app/src/screens/run/transcript.rs` | 1.8, 2.11, 2.12, 11.10, 11.11 |
| `apps/reno-app/src/vm/convo.rs` | 2.11, 5.7, 11.10 |
| `apps/reno-app/src/screens/run/output.rs` | 11.12 |
| `apps/reno-app/src/screens/run/activity.rs` | 11.13 |
| `apps/reno-app/src/ui/markdown_view.rs`, `apps/reno-app/src/screens/run/files.rs` | 6.7 |
| `apps/reno-app/src/links.rs` | 6.8 |
| `apps/reno-app/src/screens/run/dock.rs`, `apps/reno-app/src/screens/run/question.rs` | 3.13, 11.11 |
| `apps/reno-app/src/ui/composer.rs`, `apps/reno-app/src/vm/focus_guard.rs`, `apps/reno-app/src/prefs.rs` | 3.13, 14.11 |
| `apps/reno-app/src/bench.rs`, `apps/reno-app/src/app.rs`, `xtask/src/app_bench.rs`, `xtask/src/app_stats.rs`, `apps/renod/src/executors.rs` | 1.11 |
| `apps/reno-app/src/vm/capabilities.rs`, `apps/reno-app/src/store.rs` | 10.5 |
| `crates/reno-client/src/fake/` | 13.4 |
| `crates/reno-client/src/` (a new desk module) | no behavior to guard (new file) |
| `crates/reno-markdown/src/` | 11.14 |
| `crates/reno-proto/` (not changed) | 14.10 |
| `apps/reno-app/src/panel/persist.rs` (not changed) | 14.11 |
| `Cargo.toml`, `deny.toml` | no behavior to guard; new crates under 14.12 |

## Out of Scope

- Agent-written scripts Reno runs (ROAD-58).
- The real daemon side of agent-posted visuals and tool-result images (after MILE-10, ROAD-54).
- Any change to how the daemon keeps run journals.
- Any wire change: new events, feature flags or methods.
- Any new or changed persisted format; see S1, S3, S4.
- An internet client in the app; see S2.
- Editing a past prompt and regenerating from it (ROAD-59).
- Pieces of the reference agent panel not built, each for a documented reason:
  - "Open thread as markdown" and "Follow the agent": Reno has no editor.
  - Thumbs feedback: Reno has no account or cloud service (GOAL-6).
  - "Discard interrupted edit": Reno adds no file action of its own (vision non-goal 1).
  - "Stop tool call": see S5.
- Toggling task-list checkboxes from the transcript.
- Markdown in the user's own message bubble.

## Open Questions

Owned unknowns from the close package (§2a). Each is the operator's; no one builds on a guess.

- S1 Scroll position surviving an app relaunch (a persisted format) — operator — 2026-11-22 — forbid-guess (cấm đoán)
- S2 Remote http(s) images in replies; runner-up is click to load — operator — 2026-11-22 — forbid-guess (cấm đoán)
- S3 Thinking display-mode and the edit/command auto-expand settings (persisted settings) — operator — 2026-11-22 — forbid-guess (cấm đoán)
- S4 Math toggle and size in Settings › Appearance (a persisted setting) — operator — 2026-11-22 — forbid-guess (cấm đoán)
- S5 Stopping one tool call without stopping the run (no per-tool cancel on the wire): a road or not — operator — 2026-11-22 — forbid-guess (cấm đoán)
