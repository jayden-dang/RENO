# Requirements: Reading a conversation (transcript)

Feature code: TRANSCRIPT
Status: Draft
Date: 2026-11-22

Roadmap item ROAD-33 (MILE-10, designed on the mocked daemon and kept as real code). Frame: the operator's confirmed frame
and the close package approved under the run's charter. "The reference agent panel" is the shipped product this feature
follows; its pieces and the ones not built are listed in Out of Scope. Labels: a criterion marked `(agent-decided)` or
`(Weigh)` carries a choice the operator delegated or that the evidence chose. `(Weigh: reference #N)` points at
piece N of the reference agent panel's inventory, brought in by the operator's "everything the reference agent panel ships"
(Lock); the number or string it carries is the reference's, not the operator's. `(Lock …)` cites the operator's card,
frame or amendment. A `(guard)` keeps an existing behaviour. No other criterion is unlabelled.

## 1. Scroll a long conversation with every fold open

**Story:** As the owner, I want to open every "Worked for" fold of a 2,000-message thread and scroll it top to bottom
without a stutter, so that reading never waits on the app.

- **TRANSCRIPT-1.1** THE SYSTEM SHALL draw the transcript as one list item per transcript entry (a prompt, a reply, a
  reasoning block, a tool group, a fold head, a card), and, WHILE a "Worked for" fold is open, as one list item per entry
  inside it, so that a frame lays out only the items in the viewport and its overdraw band. (agent-decided: row split)
- **TRANSCRIPT-1.2** WHEN a daemon event adds, grows or changes one entry THE SYSTEM SHALL lay out again only the list
  items of that entry, keeping the item under the scroll anchor where it is on screen. (agent-decided)
- **TRANSCRIPT-1.3** WHEN a turn finishes and its interim entries fold under a new "Worked for" head THE SYSTEM SHALL keep
  the entry the owner is reading at the same place on screen (agent-decided).
- **TRANSCRIPT-1.4** WHEN a restored thread opens THE SYSTEM SHALL parse a reply's markdown no earlier than its first draw,
  and SHALL hold an entry's measured height until its pending render lands, so that the list does not jump. (Weigh)
- **TRANSCRIPT-1.5** WHEN the owner opens or closes a fold THE SYSTEM SHALL keep the fold's head where it was on screen (agent-decided).
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
  messages and runs of the thread, in reading order (Lock L7).
- **TRANSCRIPT-2.2** WHILE a selection's start or end is scrolled out of view THE SYSTEM SHALL keep the selection and
  autoscroll the list while the drag is held past the list's edge (Weigh: reference #1).
- **TRANSCRIPT-2.3** WHEN the owner double-clicks a word, triple-clicks a line, clicks four times in a block, or
  shift-clicks after a selection THE SYSTEM SHALL select that word, that line, that block, or extend the selection to the
  click (Weigh: reference #1).
- **TRANSCRIPT-2.4** WHEN the owner presses ⌘C with a selection THE SYSTEM SHALL put on the clipboard the text the owner
  sees in the selected range, in order, with one blank line between entries. (Weigh)
- **TRANSCRIPT-2.5** WHEN the selected range covers a closed fold, collapsed reasoning, a collapsed user message or a cut
  tool output THE SYSTEM SHALL copy only the lines shown for it (for a closed fold, its head line such as
  "Worked for 3m 12s"). (Weigh)
- **TRANSCRIPT-2.6** WHEN the selected range covers typeset math, a diagram or an image THE SYSTEM SHALL copy its text
  equivalent: math as its TeX between `$…$` (inline) or `$$…$$` (display) whatever delimiter the source used, a diagram as
  its fenced source, and an image as its alt text in square brackets. (Weigh; the image alt text is agent-decided)
- **TRANSCRIPT-2.7** WHEN the selected range covers a file link drawn with a short label (new with 8.1; the parser's
  unused `file_label` field anticipates it) THE SYSTEM SHALL copy the link's path, not its label (Weigh: W8; new work).
- **TRANSCRIPT-2.8** THE SYSTEM SHALL never select or copy control text: buttons and their labels, "Show N more lines",
  subagent chips, the "Thinking" line, hover times and the prompt tick rail. (Weigh)
- **TRANSCRIPT-2.9** WHEN the owner chooses "Copy as Markdown" on a selection THE SYSTEM SHALL copy the markdown source
  bytes of the selected range, original math delimiters included. (Weigh)
- **TRANSCRIPT-2.10** WHEN the owner clicks in the transcript outside the selection without dragging THE SYSTEM SHALL
  clear the selection (agent-decided).
- **TRANSCRIPT-2.11** (guard) WHEN a turn has finished THE SYSTEM SHALL CONTINUE TO offer its final message's quiet Copy,
  which copies that message's raw text.
- **TRANSCRIPT-2.12** (guard) WHEN the owner hovers a user message THE SYSTEM SHALL CONTINUE TO show its Copy icon (and,
  on the prompt, its time), a long one SHALL CONTINUE TO collapse with Show more / Show less, and the bubble SHALL
  CONTINUE TO show plain text.
  (agent-decided: the frame default of markdown in the bubble is set aside)
- **TRANSCRIPT-2.13** THE SYSTEM SHALL resolve every selection to source anchors, never to painted positions alone: for
  each entry the selection covers, the entry's key and the character range of that entry's source text, stored as UTF-8
  byte offsets on character boundaries (the unit is agent-decided); a span over text that has no source (a fold head
  such as "Worked for 3m 12s", a tool group's "Ran N commands", an "Output was cut" note) resolves to that row's key with
  no range (agent-decided). (Lock: operator
  amendment 2026-11-22, so that a later comments item anchors to a selection without rework)

## 3. Find anything in the conversation

**Story:** As the owner, I want to press ⌘F, type `supervisor.rs`, see "1 of 7" and press Enter to land on turn 3's
`cargo test` step, so that I find anything without opening folds by hand.

- **TRANSCRIPT-3.1** WHEN the owner presses ⌘F on the run screen THE SYSTEM SHALL open a find bar above the transcript with
  its query field focused, or focus the query field when the bar is already open (Weigh: reference #6).
- **TRANSCRIPT-3.2** WHILE the find bar holds a query THE SYSTEM SHALL count matches across the whole thread, in prompts,
  replies, interim text, reasoning, commands and all tool output the app holds, inside closed folds too, and show
  "<current> of <total>", such as "1 of 7" (Lock L6, L7).
- **TRANSCRIPT-3.3** WHEN the owner presses Enter or ⌘G (next), or ⇧Enter or ⇧⌘G (previous), THE SYSTEM SHALL make the
  next or previous match current, wrapping at the ends, and scroll it into view (Weigh: reference #6).
- **TRANSCRIPT-3.4** WHEN the current match sits inside a closed fold, collapsed reasoning, a closed tool output or the
  cut part of a shown output THE SYSTEM SHALL open what hides it and scroll to the match (Lock L6, L7).
- **TRANSCRIPT-3.5** THE SYSTEM SHALL highlight every visible match, and the current match with a distinct highlight (Weigh: reference #8).
- **TRANSCRIPT-3.6** WHEN the owner presses Esc in the find bar or clicks its close button THE SYSTEM SHALL close the bar,
  clear the highlights, and close again exactly the folds, reasoning and outputs that search opened. A fold the owner
  opened stays open (Lock L2, L7).
- **TRANSCRIPT-3.7** WHEN the owner opens or closes a row that search had opened THE SYSTEM SHALL treat that row as the
  owner's from then on (agent-decided).
- **TRANSCRIPT-3.8** WHILE the find bar is open THE SYSTEM SHALL send Esc only to the find bar, never to stopping the run
  or to denying a held approval (Lock L2; Weigh: reference #17).
- **TRANSCRIPT-3.9** WHEN the owner turns on match case, whole word or regex (buttons, or ⌥⌘C, ⌥⌘W, ⌥⌘X) THE SYSTEM SHALL
  recount with that option. (Weigh: the reference's toggles)
- **TRANSCRIPT-3.10** IF the regex query does not parse THEN THE SYSTEM SHALL show the field in an error state with the
  reason, and show no count (Weigh: reference #7).
- **TRANSCRIPT-3.11** WHEN new events arrive, or the owner opens or closes a row, while the find bar holds a query THE SYSTEM SHALL update the count without
  moving the current match, at most once per 150 ms. (Weigh)
- **TRANSCRIPT-3.12** WHILE a reasoning row is open only because it is streaming THE SYSTEM SHALL leave it open when search
  closes, and SHALL not close a row search opened when its stream ends while the find bar is open (agent-decided: close package decision 8).
- **TRANSCRIPT-3.13** (guard) WHILE the find bar is closed THE SYSTEM SHALL CONTINUE TO deny the held approval on Esc when
  the approval panel has focus, and, where "Escape stops the agent" is on, to stop the run on Esc from the composer while
  Stop shows.

## 4. Come back to where I was reading

**Story:** As the owner, I want to leave a thread and come back to it in the same session and find the view where I left
it, so that I keep my place in a long conversation.

- **TRANSCRIPT-4.1** WHEN the owner leaves a thread's run screen, or quits the app, THE SYSTEM SHALL save, for that
  thread, the entry at the top of the view and the offset into it, with that thread's panel state under the data dir, as
  an additive key that a file without it reads as "no saved place", so that the saved places share the panel state's
  bound of 100 threads and its carry-over from a run's key to its thread's key (4.4). (frame default, operator-confirmed 2026-11-22)
- **TRANSCRIPT-4.2** WHEN the owner returns to that thread, in the same session or after the app restarts, THE SYSTEM SHALL
  show the saved entry at the saved offset (Weigh: W3; across a restart: frame default, operator-confirmed 2026-11-22).
- **TRANSCRIPT-4.3** IF the remembered entry is gone, or now sits inside a closed fold, THEN THE SYSTEM SHALL restore to
  that fold's head, else to the next entry after it in the thread (Weigh: W4, fork 1).
- **TRANSCRIPT-4.4** WHEN a run's view moves from the run's key to its thread's key because the daemon named the thread
  THE SYSTEM SHALL carry the remembered place over (Weigh: W5, fork 1).
- **TRANSCRIPT-4.5** WHEN the owner opens a thread with no remembered place THE SYSTEM SHALL open it at the tail,
  following new events (agent-decided: today's behaviour).

## 5. Page through by keyboard

**Story:** As the owner, I want to page through a long conversation with the keyboard and jump between my own messages, so that I reach any part of it without dragging the scrollbar.

- **TRANSCRIPT-5.1** WHILE the transcript has focus THE SYSTEM SHALL scroll by 90 % of the viewport on Page Up / Page Down,
  by three lines on ↑ / ↓, and to the start or end on Home / End (Weigh: reference #13).
- **TRANSCRIPT-5.2** WHEN the owner presses ⇧Page Up or ⇧Page Down THE SYSTEM SHALL scroll to the previous or next user
  message (Weigh: reference #13).
- **TRANSCRIPT-5.3** THE SYSTEM SHALL show a vertical scrollbar for the transcript beside the prompt tick rail (Weigh: reference #14).
- **TRANSCRIPT-5.4** WHEN the owner hovers the end of a finished turn THE SYSTEM SHALL offer "Scroll to user message" (that
  turn's prompt) and, on the last turn, "Scroll to top" (Weigh: reference #18).
- **TRANSCRIPT-5.5** (guard) WHILE a transcript is taller than its view THE SYSTEM SHALL CONTINUE TO show the prompt tick
  rail, one tick per user message with the current one marked, and a click SHALL CONTINUE TO scroll to that message; a
  transcript that fits SHALL CONTINUE TO show no rail.

## 6. Get back to the live end

**Story:** As the owner, I want one click on "jump to latest" to take me back to the end of a live run and follow it again, so that I can read back and return without losing the stream.

- **TRANSCRIPT-6.1** WHILE the view is not following the tail THE SYSTEM SHALL show a "jump to latest" control, and WHEN
  the owner uses it THE SYSTEM SHALL scroll to the end and follow the tail again. (Weigh: W1; its look is agent-decided)
- **TRANSCRIPT-6.2** WHEN the owner opens a fold, a reasoning block, an output or a diagram zoom while following the tail
  THE SYSTEM SHALL stop following, so that what grew stays in view (Weigh: reference #12).

## 7. Copy just the code

**Story:** As the owner, I want to press a code block's Copy, so that I paste only that code.

- **TRANSCRIPT-7.1** WHEN the owner hovers a code block THE SYSTEM SHALL show its Copy and wrap buttons, and WHEN the owner
  presses Copy THE SYSTEM SHALL copy only the block's code (Copy: Lock L3; wrap: Weigh: reference #25).
- **TRANSCRIPT-7.2** WHEN the owner presses a code block's wrap button THE SYSTEM SHALL wrap or unwrap its long lines;
  unwrapped lines scroll sideways (Weigh: reference #25).

## 8. Follow a file or web link

**Story:** As the owner, I want to click `src/supervisor.rs:42` in a reply and land on that line in the Files tab, so that I reach what the agent points at.

- **TRANSCRIPT-8.1** WHEN the owner clicks a link or an inline-code path (`path`, `path:line`, `path#Lline`) that resolves
  to a file in the run's worktree THE SYSTEM SHALL open that file in the side panel's Files tab at that line (Weigh: W1, reference #20, #21).
- **TRANSCRIPT-8.2** WHEN the owner clicks a web link (http or https) in the transcript THE SYSTEM SHALL open it only
  through the app's link opener, and IF the target is neither a web link nor a worktree file (a `mailto:` link included,
  which today opens directly) THEN THE SYSTEM SHALL leave the text unlinked. (default: the opener opens only web links
  from what a daemon says, apps/reno-app/src/links.rs:27.) The Files tab's markdown preview keeps its own link handling
  as today. (agent-decided: the renderer takes a link policy from its host; fork-order step 4)
- **TRANSCRIPT-8.3** WHEN the owner right-clicks in the transcript THE SYSTEM SHALL show a menu with, as they apply:
  Open and Copy Link (on a link), Copy and Copy as Markdown (with a selection), Copy Message (on a reply), and Scroll to top or
  Scroll to bottom (Weigh: reference #4, #24, W1).
- **TRANSCRIPT-8.4** WHEN the owner clicks a footnote reference THE SYSTEM SHALL scroll to its footnote (Weigh: reference #22).
- **TRANSCRIPT-8.5** (guard) WHEN the Files tab previews a markdown file THE SYSTEM SHALL CONTINUE TO draw it with the
  transcript's markdown renderer, and an image file SHALL CONTINUE TO show in the Files tab.
- **TRANSCRIPT-8.6** (guard) WHEN a test runs THE SYSTEM SHALL CONTINUE TO forbid the link opener from reaching a real
  browser.

## 9. Read typeset math

**Story:** As the owner, I want `$$\int_0^1 x^2\,dx = \tfrac13$$` in a reply to show as typeset math, so that I read
formulas as formulas.

- **TRANSCRIPT-9.1** WHEN a reply holds display math (`$$…$$`, `\[…\]`) THE SYSTEM SHALL draw it typeset on its own line (Lock L3, L7).
- **TRANSCRIPT-9.2** WHEN a reply holds inline math (`$…$`, `\(…\)`) THE SYSTEM SHALL draw it typeset in the line, aligned
  on the text's baseline (Lock L7).
- **TRANSCRIPT-9.3** IF a formula does not typeset THEN THE SYSTEM SHALL show its source as code, with no error dialog (Weigh: W1).
- **TRANSCRIPT-9.4** THE SYSTEM SHALL size math to the surrounding text by default and colour it from the theme
  (agent-decided).
- **TRANSCRIPT-9.5** WHERE the owner turns "Typeset math" off in Settings › Appearance THE SYSTEM SHALL show math as its TeX
  source in code style; it is on by default, and the setting is an additive app-local key (frame default,
  operator-confirmed 2026-11-22; the name "Typeset math": agent-decided).
- **TRANSCRIPT-9.6** WHEN the owner sets the math size in Settings › Appearance (90 % to 150 % of the text size, in 10 %
  steps, default 100 %) THE SYSTEM SHALL draw math at that size; the setting is an additive app-local key (frame default, operator-confirmed 2026-11-22; the
  90–150 % range and its steps: agent-decided).

## 10. See diagrams

**Story:** As the owner, I want a ```mermaid flowchart in a reply to show as a drawn diagram I can zoom, so that I read the
structure the agent described.

- **TRANSCRIPT-10.1** WHEN a reply holds a `mermaid` code block of a supported diagram type (at least flowchart, sequence, class, state,
  ER, gantt and pie) THE SYSTEM SHALL draw the diagram
  in its place, with Preview and Code tabs that switch between the drawing and its source (Lock L3; tabs: Weigh: reference #29; the type list: agent-decided).
- **TRANSCRIPT-10.2** WHILE a diagram is being drawn THE SYSTEM SHALL show its source with a "Rendering…" note, and keep the
  previous drawing while a changed source redraws (Weigh: reference #29).
- **TRANSCRIPT-10.3** IF a diagram does not parse or its type is not supported THEN THE SYSTEM SHALL show it as a plain code
  block (Weigh: reference #29).
- **TRANSCRIPT-10.4** WHEN the owner ⌘-scrolls over a diagram THE SYSTEM SHALL zoom it between fit-to-width and 200 %, show
  the zoom level with a reset button while zoomed, and scroll it sideways when it is wider than the column (Weigh: reference #29; the fit-to-width lower bound: agent-decided).
- **TRANSCRIPT-10.5** WHEN the theme changes THE SYSTEM SHALL draw diagrams again in the new theme's colours (Weigh: reference #29).
- **TRANSCRIPT-10.6** WHEN the owner presses a diagram's Copy THE SYSTEM SHALL copy its source (Weigh: reference #29).

## 11. See images in replies

**Story:** As the owner, I want `![build graph](docs/graph.png)` in a reply to show the picture from the run's worktree, so
that I see what the agent points at.

- **TRANSCRIPT-11.1** WHEN a reply holds an image whose source is a path in the run's worktree THE SYSTEM SHALL load it
  through the daemon's file read and show it scaled down to the column width (Weigh: W1, W11, reference #30).
- **TRANSCRIPT-11.2** WHEN a reply holds a `data:` image THE SYSTEM SHALL decode it in the app and show it the same way (Weigh: W13, reference #30).
- **TRANSCRIPT-11.3** IF an image cannot be loaded (missing, unreadable, an unsupported type, or the thread's worktree is
  gone) THEN THE SYSTEM SHALL show "Image not loaded: <alt or path>" in its place (Weigh: reference #30; the wording: agent-decided).
- **TRANSCRIPT-11.4** WHEN a reply holds an http or https image THE SYSTEM SHALL show a link card with its alt text and host
  that opens in the browser through the app's link opener, and SHALL make no network request for it. (Weigh; see Open
  Questions S2)
- **TRANSCRIPT-11.5** WHEN the owner clicks an image wrapped in a link THE SYSTEM SHALL follow the link as in 8.1 and 8.2 (Weigh: reference #30).

## 12. See what an agent posts

**Story:** As the owner, I want an image or diagram that an agent posts to show in the conversation where it was posted,
so that I see it without opening another tool. (On the mocked daemon during MILE-10.)

- **TRANSCRIPT-12.1** WHERE the connection offers agent-posted visuals THE SYSTEM SHALL show each visual a run posts, an
  image or a diagram, in the transcript after the entry it was posted with, titled with its caption (Lock L7, decision 3).
- **TRANSCRIPT-12.2** WHERE the connection offers images in tool results THE SYSTEM SHALL show a tool result's image under
  its call, scaled down to at most 384 × 384 px, with "Go to file" when the call names a file (Weigh: reference #31).
- **TRANSCRIPT-12.3** WHERE the connection does not offer agent-posted visuals or tool-result images (the real daemon
  today) THE SYSTEM SHALL show none and no placeholder for them (Lock L7; default: vm/capabilities.rs REAL).
- **TRANSCRIPT-12.4** IF a posted visual fails to load or to draw THEN THE SYSTEM SHALL show its caption with
  "Visual not loaded" (agent-decided).
- **TRANSCRIPT-12.5** (guard) WHEN the app connects to the real daemon THE SYSTEM SHALL CONTINUE TO turn every mocked
  capability off.

## 13. Read thinking and tool calls as they stream

**Story:** As the owner, I want to watch a run's reasoning and tool calls appear smoothly and then fold away, so that I
follow a live run without it jumping.

- **TRANSCRIPT-13.1** WHEN reply text streams in THE SYSTEM SHALL reveal it in steps of 16 ms, catching up with the
  received text within 200 ms of the last delta, and SHALL stop the reveal timer once caught up. (agent-decided: streaming pace)
- **TRANSCRIPT-13.2** WHILE reduce motion is on THE SYSTEM SHALL show received text at once (agent-decided: close package decision 20).
- **TRANSCRIPT-13.3** WHILE "Thinking" is Auto and a reasoning block streams THE SYSTEM SHALL show it open, scrolled to its newest line, and WHEN
  it ends THE SYSTEM SHALL close it unless the owner opened or closed it meanwhile. (Weigh: the reference's default)
- **TRANSCRIPT-13.4** WHEN a new edit call with a diff or a new command call arrives while its turn is live THE SYSTEM
  SHALL show that call expanded inside its tool group, opening the group to show it, and other tool calls collapsed.
  (Weigh: reference #39; inside a group: agent-decided)
- **TRANSCRIPT-13.17** WHERE the owner sets "Thinking" in Settings › General to Auto (the default), Preview (open while
  streaming at most 16 lines high), Always open or Always closed THE SYSTEM SHALL show reasoning blocks that way; the
  setting is an additive app-local key (Weigh: reference #38, built under the conductor's clarification of 2026-11-22;
  the 16-line Preview height: agent-decided).
- **TRANSCRIPT-13.18** WHERE the owner turns off "Expand new edits" or "Expand new commands" in Settings › General (both
  on by default) THE SYSTEM SHALL show that kind of new call collapsed; each is an additive app-local key (Weigh:
  reference #38, built under the conductor's clarification of 2026-11-22).
- **TRANSCRIPT-13.5** WHEN a tool call edits one file THE SYSTEM SHALL show the file's badge with "Go to file", and its
  diff inline when expanded; WHEN a call applies a patch to several files THE SYSTEM SHALL show one hunk block per file.
  (Weigh: reference #40, #41)
- **TRANSCRIPT-13.6** WHEN a tool call shows a command THE SYSTEM SHALL show the command with its own Copy, its elapsed time
  and its exit status. (Weigh)
- **TRANSCRIPT-13.7** WHEN a file-read call's output has line numbers THE SYSTEM SHALL show it as a numbered code block with
  Copy, and WHEN the owner opens "Raw input" on a call THE SYSTEM SHALL show the call's input. (Weigh)
- **TRANSCRIPT-13.8** WHEN a turn ends in an error THE SYSTEM SHALL show the error in a callout with "Copy error". (Weigh)
- **TRANSCRIPT-13.9** THE SYSTEM SHALL hide an empty reply entry and a tool call canceled before any output. (Weigh)
- **TRANSCRIPT-13.10** (guard) WHILE a live run is between steps THE SYSTEM SHALL CONTINUE TO show the "Thinking" line under
  the last row, on the shared pulse clock.
- **TRANSCRIPT-13.11** (guard) WHEN a run asks for an approval or an answer THE SYSTEM SHALL CONTINUE TO show it as one
  muted line in the transcript, and let the owner approve, deny or answer it in the dock above the composer.
- **TRANSCRIPT-13.12** (guard) WHEN the owner opens a tool call with output THE SYSTEM SHALL CONTINUE TO show its first
  lines, "Show N more lines" ("Show 1 more line"), "Output was cut: <size> total" when the daemon cut it, and a Copy that
  copies the whole output it holds; a call without output SHALL CONTINUE TO add no block.
- **TRANSCRIPT-13.13** (guard) WHERE the connection offers run activity (the fake daemon today) THE SYSTEM SHALL CONTINUE
  TO show a subagent's chip under the top-level reply that spawned it, and a click SHALL CONTINUE TO open its tab.
- **TRANSCRIPT-13.14** (guard) WHEN a reply streams THE SYSTEM SHALL CONTINUE TO reparse only from the last stable block,
  and to mend a hanging `**`, `*`, backtick, `~~` or link in the last block.
- **TRANSCRIPT-13.15** (guard) WHEN consecutive tool calls arrive THE SYSTEM SHALL CONTINUE TO join them into one tool group
  row ("Ran N commands") with their results, which opens to its calls.
- **TRANSCRIPT-13.16** WHEN a tool result carries a link to a file in the run's worktree THE SYSTEM SHALL show it as a button
  that opens that file in the Files tab. (Weigh: reference #23)

## 14. Read rich markdown

**Story:** As the owner, I want tables, callouts and footnotes in a reply to read as they would on a docs page, so that
long structured answers stay readable.

- **TRANSCRIPT-14.1** WHEN a table is wider than the column THE SYSTEM SHALL scroll it sideways with its own scrollbar (Weigh: reference #26).
- **TRANSCRIPT-14.2** WHEN a block quote starts with `[!NOTE]`, `[!TIP]`, `[!WARNING]` or `[!CAUTION]` THE SYSTEM SHALL draw
  it as a callout in that kind's colour and label (Weigh: reference #28).
- **TRANSCRIPT-14.3** THE SYSTEM SHALL draw footnotes at the end of their reply, hide HTML comments, and draw a leading
  front-matter block as a small key-value table (Weigh: reference #28).
- **TRANSCRIPT-14.4** THE SYSTEM SHALL draw task-list items with a checkbox that shows their state and does not toggle (Weigh: reference #27).

## 15. Try every reading tool on the mocked daemon

**Story:** As the owner, I want to open seeded threads on `reno-app --fake` that hold both lock examples, so that I click
through and grade the reading tools on mocked data.

- **TRANSCRIPT-15.1** WHERE the app runs on the fake daemon THE SYSTEM SHALL offer a finished thread "fix the ACP v2
  handshake" of 2,000 messages, with "Worked for" folds, in which `supervisor.rs` occurs exactly 7 times in text the app
  holds, the first in turn 3's `cargo test` step (Lock L2; seeding: default).
- **TRANSCRIPT-15.2** WHERE the app runs on the fake daemon THE SYSTEM SHALL offer a thread whose reply holds
  `$$\int_0^1 x^2\,dx = \tfrac13$$`, a Mermaid flowchart, a code block, a worktree image, a remote image, a file link and an
  agent-posted visual (Lock L3; seeding: default).
- **TRANSCRIPT-15.3** WHERE the app runs on the fake daemon THE SYSTEM SHALL offer a run with one very long streaming reply
  that has code blocks and math (Weigh: close package success).
- **TRANSCRIPT-15.4** (guard) WHERE the app runs on the fake daemon THE SYSTEM SHALL CONTINUE TO offer its seeded runs and
  the `/burst N`, `/long`, `/refused` and `/fails` prompt scripts as they behave now.

## 16. Quality attributes

**Section-kind:** nfr

**Story:** As a stakeholder, I want measurable quality targets for this feature, so that how-well is not left implicit.

- **Performance:** No standing metrics doc (docs/product/metrics.md absent); targets come from the operator's Lock and the
  signed-off budgets (docs/BENCHMARKS.md).
  - **TRANSCRIPT-16.1** WHEN `cargo xtask bench --app` runs the long-transcript scenario (2,000 messages, every fold open)
    THE SYSTEM SHALL keep frame time at or under 16.7 ms at p95, and the `long_frame_p95_ms` row SHALL fail the run when
    over (today it is a target that does not fail, xtask/src/app_stats.rs:86-91; docs/BENCHMARKS.md's "a target, not a
    budget" line is rewritten with it) — verified by the bench (Lock L10).
  - **TRANSCRIPT-16.2** WHEN the bench runs a new one-very-long-reply scenario THE SYSTEM SHALL keep frame time at or under
    16.7 ms at p95 — verified by the bench (Weigh: close package success).
  - **TRANSCRIPT-16.3** THE SYSTEM SHALL stay inside the signed-off long-transcript peak footprint (≤ 370 MiB), streaming CPU
    (≤ 15 %) and peak footprint (≤ 260 MiB), idle CPU (< 1 %), idle wakeups (< 1 per second) and cold start (< 400 ms)
    budgets — verified by the bench (default: the signed-off budgets, docs/BENCHMARKS.md).
  - **TRANSCRIPT-16.4** THE SYSTEM SHALL bound every cache this feature adds (math, diagrams, images, search, text layout)
    by entry count or bytes — verified by unit tests at the bound (ARCH-12).
- **Security:** No threat model (docs/security/threat-model.md absent). The trust boundary crossed is untrusted agent output
  reaching the app.
  - **TRANSCRIPT-16.5** THE SYSTEM SHALL make no outbound network request because of anything in a transcript — verified by
    a lock test whose counting HTTP client sees zero sends for a reply with an https image and links (Weigh: W12).
  - **TRANSCRIPT-16.6** THE SYSTEM SHALL open links from transcript content only through the app's link opener, which
    tests forbid from reaching a real browser — verified by lock tests (Weigh: W14).
  - **TRANSCRIPT-16.7** THE SYSTEM SHALL read local images only through the daemon's worktree-rooted file read — verified by
    a lock test with a path outside the worktree that shows "Image not loaded" (Weigh: W11).
  - **TRANSCRIPT-16.12** THE SYSTEM SHALL pin each new crate (the math renderer, `merman`) to an exact version, and the
    workspace SHALL pass `cargo deny check` and `cargo machete` — verified in the verify run (ARCH-17).
- **Reliability:** No standing reliability doc (docs/ops/reliability.md absent). The feature adds no durable state; what it
  must keep durable is the existing contracts (Compat obligation Internal, docs/agents/project.md:24).
  - **TRANSCRIPT-16.10** (guard) THE SYSTEM SHALL CONTINUE TO match the protocol schema snapshot unchanged: no new event,
    feature flag or method — verified by `protocol_schema_matches_snapshot`.
  - **TRANSCRIPT-16.11** (guard) THE SYSTEM SHALL CONTINUE TO read every existing `panel-state.json` and `app-settings.json`
    as today, adding only optional keys (a file without them reads as the defaults; `panel-state.json` stays version 1) — verified by the existing persist and prefs tests and a test that reads a file
    from before this feature.
  - **TRANSCRIPT-16.13** (guard) WHEN the owner opens Settings › Appearance or Settings › General THE SYSTEM SHALL CONTINUE
    TO show and apply every existing control as today; the new math and thinking rows are added beside them — verified
    by the existing settings tests.
- **Accessibility:** No standing accessibility doc (docs/standards/accessibility.md absent).
  - **TRANSCRIPT-16.8** THE SYSTEM SHALL make every new control (find bar and its toggles, jump to latest, Copy and wrap
    buttons, diagram tabs and zoom reset, the context menu) reachable and operable by keyboard alone — verified by GPUI
    keystroke tests (agent-decided).
  - **TRANSCRIPT-16.9** THE SYSTEM SHALL draw highlights, the current match, callouts and diagram colours from theme tokens
    in both light and dark themes — verified by lock tests on both themes (agent-decided).

## Guard map

Every file the change touches, and the guards that protect its existing behaviour:

| File | Guards |
|---|---|
| `apps/reno-app/src/transcript.rs` | 1.6, 1.7, 13.15 |
| `apps/reno-app/src/screens/run.rs` | 1.6, 1.8, 1.9, 1.10, 5.5 |
| `apps/reno-app/src/screens/run/transcript.rs` | 1.8, 2.11, 2.12, 13.10, 13.11 |
| `apps/reno-app/src/vm/convo.rs` | 2.11, 5.5, 13.10 |
| `apps/reno-app/src/screens/run/output.rs` | 13.12 |
| `apps/reno-app/src/screens/run/activity.rs` | 13.13 |
| `apps/reno-app/src/ui/markdown_view.rs`, `apps/reno-app/src/screens/run/files.rs` | 8.5 |
| `apps/reno-app/src/links.rs` | 8.6 |
| `apps/reno-app/src/screens/run/dock.rs`, `apps/reno-app/src/screens/run/question.rs` | 3.13, 13.11 |
| `apps/reno-app/src/ui/composer.rs`, `apps/reno-app/src/vm/focus_guard.rs`, `apps/reno-app/src/prefs.rs` | 3.13, 16.11 |
| `apps/reno-app/src/bench.rs`, `apps/reno-app/src/app.rs`, `xtask/src/app_bench.rs`, `xtask/src/app_stats.rs`, `apps/renod/src/executors.rs` | 1.11 |
| `apps/reno-app/src/vm/capabilities.rs`, `apps/reno-app/src/store.rs` | 12.5 |
| `crates/reno-client/src/fake/` | 15.4 |
| `crates/reno-client/src/` (a new desk module) | no behavior to guard (new file) |
| `crates/reno-markdown/src/` | 13.14 |
| `crates/reno-proto/` (not changed) | 16.10 |
| `apps/reno-app/src/panel/persist.rs` (additive key only) | 4.1, 16.11 |
| `apps/reno-app/src/screens/settings_pages.rs` (new rows in Appearance and General) | 16.13 |
| `Cargo.toml`, `deny.toml` | no behavior to guard; new crates under 16.12 |

## Out of Scope

- Agent-written scripts Reno runs (ROAD-58).
- The real daemon side of agent-posted visuals and tool-result images (after MILE-10, ROAD-54).
- Any change to how the daemon keeps run journals.
- Any wire change: new events, feature flags or methods.
- Any change to the daemon's store or the wire (the persisted-format hard stop, as the conductor clarified
  2026-11-22). App-local settings and panel state gain only additive keys (16.11).
- An internet client in the app; see S2.
- Editing a past prompt and regenerating from it (ROAD-59).
- Pieces of the reference agent panel not built, each for a documented reason:
  - "Open thread as markdown" and "Follow the agent": Reno has no editor.
  - Thumbs feedback: Reno has no account or cloud service (GOAL-6).
  - "Discard interrupted edit": Reno adds no file action of its own (vision non-goal 1).
  - "Stop tool call": see S5.
- Images at an absolute path or outside the run's worktree (reference #30 loads them): the daemon's file sandbox refuses
  absolute paths (apps/renod/src/files/sandbox.rs:125), so TRANSCRIPT-16.7 shows "Image not loaded".
- Comments anchored to passages: their own MILE-10 item, ROAD-60, after ROAD-33 and ROAD-31 (operator amendment
  2026-11-22). ROAD-33 gives them
  source-anchored selections (2.13) and builds no comment code.
- Toggling task-list checkboxes from the transcript.
- Markdown in the user's own message bubble.

## Open Questions

Owned unknowns from the close package (§2a). Each is the operator's; no one builds on a guess.

- S2 Remote http(s) images in replies; runner-up is click to load — operator — 2026-11-22 — forbid-guess (cấm đoán)
- S5 Stopping one tool call without stopping the run (no per-tool cancel on the wire): a road or not — operator — 2026-11-22 — forbid-guess (cấm đoán)

Unverified (each has its check; none is guessed):

- U1 The 31 ms cause (one list item holding an open fold's ~4,000 rows, plus the per-event rebuild) was read from code, not
  profiled; 1.1, 1.2 and 16.1 rest on it — check: the plan's first task profiles the 2,000-message bench before the row
  model is built, and a different cause goes through `reroute-plan`.
- U2 The reference keeps the scroll position across a relaunch — read from source, not run; no longer
  load-bearing, as the operator confirmed saving the position (4.1).
- U3 Math copies as TeX in common web math renderers — recalled; checked at the operator's pick.
- U4 Markdown-image exfiltration from agent chat UIs is a published class — recalled; 11.4 does not rest on it.
- U5 Which executors expose a rewind call — not researched; for ROAD-59's frame.
