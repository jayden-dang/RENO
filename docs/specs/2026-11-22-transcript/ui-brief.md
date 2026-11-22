# UI brief: reading, finding and copying in a conversation (ROAD-33, TRANSCRIPT)

Status: Locked
Date: 2026-11-22
Screenshot: [ui-brief.png](./ui-brief.png)
Inherits: [../../design/2026-11-19-app-shell-ui-brief.md](../../design/2026-11-19-app-shell-ui-brief.md) (tokens, type,
icons, gutter), [../../design/2026-11-19-run-activity-ui-brief.md](../../design/2026-11-19-run-activity-ui-brief.md)
(subagent chips).

## Decision

Direction **A "Quiet strip"**, amended with B's result ticks on the tick rail. Two directions were dropped:
- **B "Ledger":** a floating find panel, a full-width "Latest" bar, fold heads with step counts, a permanent code header.
- **C "Results":** a Find tab of hits in the side panel, a centred "Latest" pill, fold cards with tool chips.

The owner's words: "A, thêm vạch kết quả trên minimap lấy từ B" (A, with B's result ticks on the minimap), then "go".

## Grounding

- The token source of record is the app's `Theme` (`apps/reno-app/src/ui/theme.rs`), with the values in the shell brief. No
  token was added.
- Typefaces: Geist for UI and prose, Geist Mono for code, commands, paths and counts. Icons: Phosphor.
- Precedence: the owner's words (the ROAD-33 card and the pick), then the shell brief and the kit, then the variant plan
  (`RUN/ui/plan.md`, A).

## Surfaces

### Find strip (⌘F)

- **Layout:**
  - A 36 pt strip pinned to the top of the transcript canvas, full width of the canvas, under the status bar.
  - It holds, left to right: a search icon, the query field, the count "1 of 7", ↑ ↓ and ✕.
  - The transcript scrolls under it.
- **Components:**
  - rung 1: `ui/field.rs` (the query field), `kit::icon_button` (↑ ↓ ✕), `kit::tip`, `kit::focus_ring`.
  - new (rung 7): the strip frame, because no kit piece is a pinned bar over a list.
- **States:**
  - closed;
  - open and empty;
  - "N of M";
  - "No results";
  - counting while the thread loads: the count restarts at the first hit until one is gone to.
- **Type & color:**
  - The strip is on `surface`; the field uses the kit's own colours.
  - The count is Geist 13 in `muted`.
  - Matches in the transcript get a `warning`-tint background; the current match gets an `accent` fill with text from
    `on_fill`.
- **A11y:**
  - ⌘F opens or focuses the field. Enter/⌘G and ⇧Enter/⇧⌘G go to the next and previous hit.
  - Esc and ✕ close; while the strip is open, Esc never reaches stopping the run or the approval dock.
  - Every control has a label and the kit focus ring.

### Tick rail: result ticks (the amendment)

- **Layout:** while the strip holds a query, each row with a hit gets a short tick on the prompt tick rail, beside the
  prompt ticks; the current hit's tick is longer. The ticks go when the strip closes. The rail itself shows only when the
  transcript scrolls, as today.
- **Components:** rung 2 — the existing tick rail (`convo::tick_marks`, `run.rs` `tick_rail`), extended with result marks.
- **States:** no query (prompt ticks only); a query with hits; the current hit moves.
- **Type & color:** result ticks in `accent`; prompt ticks unchanged.
- **A11y:** the rail is a pointer aid; the same navigation is on the keyboard through the strip.

### Transcript rows

- **Layout:**
  - One list item per entry; an open "Worked for" fold's entries are items of their own.
  - Fold heads stay the single muted line "Worked for <duration> ›".
  - A ⌘F hit inside an edit step makes that line name the file, with the match highlighted.
- **Components:** rung 1 — the existing row views (`screens/run/transcript.rs`, `output.rs`, `activity.rs`).
- **States:**
  - a fold closed, opened by the owner, or opened by search; search closes again only what it opened;
  - streaming;
  - finished.
- **Type & color:** unchanged from the shell and run-activity briefs. Match highlights as in the find strip.
- **A11y:** fold heads are buttons with their duration in the label.

### Selection and copy

- **Layout:**
  - A drag selects across entries, messages and runs in reading order.
  - It autoscrolls past the list's edge and survives scrolling.
  - Control text is never selected.
- **Components:** new (rung 7) — the selection tracker and the source anchors (`vm/selection.rs`, `vm/anchor.rs`,
  `ui/markdown_view/select.rs`), because no kit piece selects across list items.
- **States:**
  - none;
  - selecting;
  - selected;
  - copied (⌘C; the clipboard holds what is seen, in order).
- **Type & color:** the selection background is `accent` at low alpha.
- **A11y:** ⌘C copies the selection only when the transcript holds focus; the composer's ⌘C is unaffected.

### Jump to latest

- **Layout:** a round 32 pt icon button over the bottom-right corner of the transcript, shown only while the view is not
  following the tail.
- **Components:** rung 1 — `kit::icon_button`, `kit::tip`.
- **States:**
  - hidden while following;
  - shown when detached, including when the owner opens a fold, a reasoning block or an output while following;
  - pressed: scrolls to the end and follows again.
- **Type & color:** the kit's button on `surface`, with an arrow-down icon.
- **A11y:** the label is "Jump to latest", with the focus ring.

### Rich blocks: code, diagrams, math, images, posted visuals

- **Layout:**
  - **Code blocks:** Copy and Wrap appear as icon buttons at the top-right on hover only. Unwrapped lines scroll sideways.
  - **Diagrams:** the same frame and hover chrome. A diagram is fitted to the column, never below 3/4 of its size;
    wider diagrams scroll sideways.
  - **Display math** on its own line at 1.2× the body size × the Math size setting. **Inline math** sits on the baseline.
  - **Worktree images** are scaled down to the column. A remote image is a link card (alt text, host).
  - **Posted visuals** follow their entry, with a caption.
- **Components:**
  - rung 1: `kit::icon_button`, the toast "Copied".
  - new (rung 7): `ui/markdown_view/{math,mermaid,picture,chrome,visual,link,lru}.rs`.
- **States:**
  - "Rendering…" with the source;
  - drawn;
  - a failed diagram or formula shows its source as code;
  - "Image not loaded: <alt or path>";
  - "Visual not loaded";
  - a link card.
- **Type & color:** math in `text`. Diagrams themed from tokens: background `surface`, nodes on `raised` with `faint`
  borders, lines in `muted`, labels in `text` (`ui/markdown_view/mermaid.rs`). Code in Geist Mono. Code blocks have no
  header in this direction.
- **A11y:** Copy and Wrap carry labels; a link card is a button that opens through the app's link opener.

## Signature

Nothing shows until you reach for it: the transcript stays text. The tick rail is the one map of where everything is,
your prompts and your hits.

## Amendments (decided constraints)

- The owner amended A with B's result ticks on the tick rail. They use the rail's own tick code; there is no second rail.
- The owner opened every fold, then ⌘F → Enter → Esc. Esc closes only what search opened, and a fold the owner opened stays
  open (frame decision 5; the conductor relayed the go with this reading kept).
- Before the pick, at the owner's review stage:
  - the flowchart was fitted whole, at no less than 3/4 of its size;
  - a hit inside an edit step names the file;
  - "1 of 7" shows on typing.

The screenshot is the locked direction with the find strip open, "1 of 7" and the result ticks on the rail. It was taken
in the review build, so its status bar still shows the review label "A — Quiet strip", which the kept code does not have.

Code:
- `apps/reno-app/src/vm/items.rs` (the flat projection), `vm/find.rs`, `vm/selection.rs`, `vm/anchor.rs`.
- `apps/reno-app/src/screens/run.rs`, `screens/run/{transcript,find,select,output}.rs`; `apps/reno-app/src/ui/find.rs`.
- `apps/reno-app/src/ui/markdown_view.rs` and `ui/markdown_view/{math,mermaid,picture,chrome,visual,link,lru,select}.rs`.
- `crates/reno-client/src/visuals.rs`; `crates/reno-client/src/fake/{transcript,visuals}.rs`.
- The fake's switch: the app's existing `--fake`. `Store::start_fake` sets `FakeConfig::transcript_threads`, which adds
  the seeded threads, `/longreply` and the visuals desk. `Capabilities::REAL.agent_visuals` is off.

Lock tests (each seen red, then green):
- `screens/run/find_tests.rs`:
  - `find_opens_a_strip_pinned_above_the_transcript`;
  - `match_ticks_show_on_the_rail_while_find_holds_a_query`;
  - `the_cards_own_order_with_every_fold_open`;
  - `find_goes_to_hits_opens_what_hides_them_and_closes_only_that` ("1 of 7").
- `vm/find.rs`: `the_count_reads_one_of_n_…`.
- The list test `the_flat_list_keeps_the_anchor_and_follows_the_tail` (jump to latest; the fold head is the single line
  "Worked for <duration>").
- `ui/markdown_view/chrome.rs`: `the_controls_are_hidden_at_rest_on_a_code_block_and_a_diagram`.
- `renderers_tests.rs`: `a_code_block_has_copy_and_wrap_in_its_frame_and_no_header`.
- `ui/markdown_view.rs`: `the_render_checks_flowchart_is_drawn_whole_and_legible_in_the_column`.
- `screens/run/select_tests.rs`: `a_selection_is_highlighted_over_the_words_it_covers`.

What the lock tests do not hold:
- Hover-only chrome is locked at rest (hidden), not at the reveal: GPUI's `group_hover` is not readable from a test.
- The fold-head check reads selectable text only; a non-selectable child added to the head would pass.

## Contract

The calls the screen makes. Only the first is a mocked contract (the real daemon has no such method yet). The others are
daemon methods that already exist, which the fake answers from its demo tree. Full detail: RUN `ui/contract-calls.md`.

1. **Agent-posted visuals: `VisualDesk`, via `Store::run_visuals(&RunId) -> Vec<Visual>`.** Local, with no wire call,
   gated by `Capabilities::agent_visuals`.
   - `Visual { id, caption, after: Seq, kind: Image(Worktree(path) | Data { mime, bytes }) | Diagram(source) }`, shown
     once, after the entry whose event is `after`.
   - Errors and states:
     - capability off, or none posted: nothing, and no placeholder;
     - `after` names no entry: the visual is dropped;
     - a refused path (absolute, `..`, a scheme): "Visual not loaded";
     - empty, unsupported or SVG bytes: "Visual not loaded";
     - the image read fails: "Visual not loaded", and "Loading image…" while it is read;
     - a diagram that does not draw: "Visual not loaded", and "Drawing…" while it draws.
2. **`fs.list`: resolving file links (`FsListParams { run_id, dir, cursor, limit }` → `FsListResult { entries,
   next_cursor }`).**
   - One request per directory a reply names, up to 5 pages. A named path that is a `File` becomes a link to the Files
     tab at its line.
   - Errors (`NotFound`, `InvalidParams`, `WorkspaceUnavailable`, not connected, a transport error) and a directory not
     yet answered all leave the path as plain, unlinked text.
3. **`fs.read_image`: a worktree image (`FsReadImageParams { run_id, path, offset }` → `FsImageChunk`).**
   - Chunked to EOF, with offset and hash checked. Cached by (run, path): 64 images, 48 MiB.
   - Never sent for `data:` images, web images or refused paths.
   - Errors: `NotFound`, `InvalidParams` (an unsupported type, over 8 MiB), `WorkspaceUnavailable`, a protocol fault in a
     chunk, an SVG or unknown type returned, or not connected. Each shows "Image not loaded: <alt or path>", and is not
     retried until the cache drops it.
4. **`fs.read`:** the Files tab's existing call when a file link is clicked. The transcript adds no new handling.
5. **The seeded runs and `/longreply`:** read through the existing store and `task.run`. No new request and no new error.
