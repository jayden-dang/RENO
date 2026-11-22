# Design: Reading a conversation (transcript)

Feature code: TRANSCRIPT
Status: Draft
Date: 2026-11-22
Requirements: ./requirements.md
UI brief: ./ui-brief.md (Locked)

## Context

**What exists.** `reno-app` draws a run's transcript as a GPUI `list()`. The pure row model (`apps/reno-app/src/transcript.rs`,
`Row`, `Key`) folds a finished turn's interim steps into one `Row::Worked`. Before this branch, an open fold drew all its
inner rows inside one list item, and every daemon event rebuilt the latest run and spliced a prefix diff. The bench measured
31.4 ms p95 on 2,000 messages with every fold open (docs/BENCHMARKS.md:198). That cause is read from the code, and an
unsymbolised run on the fake is consistent with it (RUN `ui/profile/U1-profile.md`); it is not profiled. U1 stays open
until plan task 1 runs the real bench. The markdown parser (`crates/reno-markdown`)
already recognised math and images but drew them as text. There was no selection, no ⌘F and no scroll restore.

**What the branch already keeps.** The draft-ui stop left kept code on `feat/transcript` (lock d011985, brief
`./ui-brief.md`), with a requirement-by-requirement status in RUN `scan-design.md` §2. Built:
- the flat item projection;
- find over the data, with the operator's result ticks on the tick rail;
- source-anchored selection and ⌘C;
- jump to latest;
- code Copy and wrap;
- typeset math with its two settings;
- a flowchart and sequence renderer;
- worktree, `data:` and remote-link images;
- file and web link policy;
- the agent-visuals desk;
- the seeded fake threads.

Measured on the fake: p95 10.0 ms over 4,048 items with every fold open (RUN `ui/profile/U1-profile.md`). Missing are the
pieces listed in §2 of that scan: keyboard paging, scroll save, the context menu, diagram tabs and zoom, the streaming
reveal, the tool cards, the Thinking modes, rich-markdown extras, and the two bench scenarios. This design states how all
of it is built, kept and finished.

**The binding constraint** is that no daemon contract moves:
- no wire change (ARCH-13; the `protocol_schema` snapshot stays as it is);
- no change to how the daemon keeps run journals (L5);
- no outbound network from the app (16.5);
- and the nested `rows()` model that every existing lock test reads (L4, 1.6).

That rules out three alternatives:
- an `ExecEvent` for posted visuals or images (the visuals live behind an app-local desk instead);
- a server-side search or a per-tool cancel (search runs over the data the app already holds; per-tool cancel is S5);
- replacing `Row` with a flat entry type (a projection sits under the nested model instead).

**Retrieval (load-subgraph, fresh, 2026-11-22).**
- Catalog: 10 Recognized cards, owns_coverage **0 of 10**, because no card has a `tasks.md`. P0 term seeds matched only
  TRANSCRIPT's own triad. The neighbour list is empty because of coverage, not because nothing is near.
- Surface-root neighbours, from the catalog query:
  - MOCKDESK (`crates/reno-client/src/fake/`): the seeded threads and the visuals desk;
  - SETTINGS (`prefs.rs`): the math and Thinking settings;
  - APPBENCH (`xtask` bench): the frame budget;
  - RUNACTIVITY (`panel/`, `activity.rs`): subagent chips.
- Spine invariants relied on: **ARCH-2, ARCH-8, ARCH-10, ARCH-11, ARCH-12, ARCH-13, ARCH-16, ARCH-17**. None is
  contradicted. System docs consulted: `docs/architecture/runtime.md` "Timers and clocks" (every timer is a deadline; no
  periodic wake) and `docs/standards/testing.md` (no wall clock, no network; a test that reaches the owner's machine must
  fail).

## Decisions

1. **Rows.** `Row` and `rows()` stay the nested model. A flat projection (`vm/items.rs`) makes one list item per entry, and
   an open fold's entries become items of their own. Height changes use `remeasure_items`, never a `splice` over the
   anchor (agent-decided, Delegated: row split).
2. **Events.** The latest run is built incrementally:
   - new events append to a kept builder state instead of rebuilding from the first event;
   - a turn's `Done` replaces that turn's items with its fold in one splice that keeps the reading place.
   This closes 1.2's remaining O(events) rebuild (run.rs:678).
3. **Find.** Runs over the source text of every row the app holds, closed folds included. A hit is (row `Key`, part, byte
   range). Search opens rows into its own set beside the owner's (`Opens<K>`), and Esc closes only search's set. The count
   restarts at the first hit until one is gone to (L2: "1 of 7").
4. **Selection.** Resolves to source anchors: (entry `Key`, slot, UTF-8 byte range on character boundaries). Text with no
   source anchors to its row key with no range; a fold head anchors to `Key::fold(first)` with `Slot::HEAD` (vm/selection.rs
   :198-200). This is the operator's Lock, TRANSCRIPT-2.13.
5. **Copy.** ⌘C gives what is seen, in order. "Copy as Markdown" gives the source bytes (Weigh W6–W10).
6. **Reading place.** Saved per thread in `panel-state.json` as an additive `#[serde(default)] place` field on the
   thread's `Descriptors`:
   - `VERSION` stays 1;
   - it inherits the 100-key bound and the run-to-thread carry-over (`resolve_panel_key`, `run.rs:906-920`);
   - each scroll event marks it changed, and the existing `Saver` coalesces the writes to one, 750 ms after the last
     change;
   - quit and window close flush it (`panel/saved.rs:58`, `app.rs:401`);
   - the offset is whole points, because `Descriptors` derives `Eq`;
   - a view following the tail saves no place, so the thread reopens at the tail and follows (4.5).
   (Operator-confirmed frame default; conductor clarification 11:12.)
7. **Renderers.** Both are agent-decided (Delegated: renderers):
   - math: `ratex-*` `=0.1.16`, with an OFL notice;
   - diagrams: `merman` `=0.11.0`, with its export feature off, the app's own `ttf-parser` text measurer, and the
     flowchart, sequence, class, state, ER, gantt and pie features.
   Both rasterise through the linked `resvg`/`usvg` `=0.46.0` with the `no_files()` resolver, which resolves no path.
8. **Images and visuals.**
   - Worktree SVGs draw through that same `no_files()` rasteriser, never GPUI's `img()`/`svg()`, which follow file paths
     inside an SVG (interpret W19 condition).
   - An image source reaches the screen only as a typed `ImageSource::{Worktree, Data}`. A remote image is a link card,
     and nothing is fetched (fork 3).
   - Agent-posted visuals and tool-result images live behind a reno-client desk and an app-local `Capabilities` field:
     `REAL` false, `FAKE` true. This follows the ActivityDesk precedent (crates/reno-client/src/activity.rs:6) and needs no
     wire change.
9. **Timers.** Nothing polls (ARCH-10, runtime.md "Timers and clocks"). Two kinds exist, and neither runs while idle:
   - **One-shot deadlines:**
     - The find recount is a 150 ms **throttle**: the first change arms it, and later changes inside the window ride the
       same deadline (`run/find.rs:229-241`). It is not a debounce, so a stream cannot starve the count.
     - The diagram re-raster fires 300 ms after the last zoom step.
   - **Bounded animation ticks** of 16 ms. Each exists only while its cause holds, the same discipline as the shared
     pulse clock and its leases (`ui/motion.rs:40-64`):
     - the streaming reveal, while received text is ahead of shown text; it stops when caught up and is off under
       reduced motion;
     - the drag autoscroll, while the pointer is held past the list edge; it stops on release (`run/select.rs:321-325`).
   Tests drive all of them on GPUI's test clock (ARCH-16).
10. **Bench.** `long_frame_p95_ms` becomes a failing budget, and BENCHMARKS.md's "a target, not a budget" line goes. A
    bench-only `renod --stream-mock=longreply` adds the one-very-long-reply scenario. That is bench executor code in
    `apps/renod`, not a change to how journals are kept (L5).
11. **No ADR.** No decision here is at once hard to reverse, surprising and a real trade-off. The renderers are pinned
    crates behind one module each; the reading place is an additive optional field; the desk is app-local.

## Architecture

Vocabulary: **module**, **interface**, **implementation**, **seam**. File paths are under `apps/reno-app/src/` unless they
say otherwise.

### Row projection and list events

Satisfies: TRANSCRIPT-1.1, TRANSCRIPT-1.2, TRANSCRIPT-1.3, TRANSCRIPT-1.4, TRANSCRIPT-1.5, TRANSCRIPT-1.6, TRANSCRIPT-1.7,
TRANSCRIPT-1.8, TRANSCRIPT-1.9, TRANSCRIPT-1.10, TRANSCRIPT-1.11
Reuse: none — new code (rung 7) for the projection `vm/items.rs`, which is new on this branch and kept from the draft;
it builds on rung 2 (`transcript.rs` `build_rows`, `fold_turns`, `Key`) and rung 4 (GPUI `ListState::remeasure_items`).
Respects: ARCH-12, ARCH-10
Surface:
- `RunScreen::rows()`, `top_row()`, `older_builds()`, `facts()`: cfg(test) accessors read by `tests.rs:327`,
  `convo_tests.rs:228`, `thread_tests.rs:151` and the other lock tests. **frozen**: built around, since `rows()` stays the
  nested model and `top_row()` stays a top-level index.
- `run.rs` `rebuild_rows` (:627): **replace** with the incremental builder; its only caller is `refresh`.
- `list.splice` at `run.rs:513` (`apply_plan`): **replace**, splice only for a fold replacing its turn; growth uses
  `remeasure_items`.
- `bench_scroll` (`run.rs:756`) and `open_all_folds` (`run.rs:735`), read by `app.rs:604,612` and the bench: **replace**,
  they scroll and open over items; the hooks named in 1.11 keep their names.
Interface: `Projection { items, … }` with `item_of(Key)`, `item_of_top(row)`, `top_of(item)`, `reproject(rows, from,
is_open)`, `diff(old, new, at) -> Plan { splice, remeasure }`; `RunScreen` keeps `rows()`; a `TurnBuilder` exposes
`push(seq, event) -> Change { Grew(Key) | Added(Key) | Folded { turn, fold } }`, `rows()`, and a call index
`call(id) -> Option<CallInfo { input: &ToolCall, result_seq: Option<Seq> }>` over the run's events, which the store
already holds.
Depth: if `vm/items.rs` vanished, callers would still need to know only that a list item maps to (row, inner index) and
back by `Key`; the folding rules stay in `transcript.rs`.
Locality: extend `run.rs` (refresh path) and `transcript.rs` (an incremental builder beside `build_rows`, sharing its row
rules); leave `screens/run/transcript.rs` row drawing; activity chips read `prose_at` from the projection (done).

`refresh` feeds only events past the last seen seq into the latest run's `TurnBuilder`. A grown or changed entry yields a
remeasure of its item. A new entry yields an insert splice at the end. A `Done` yields the folded turn, whose items replace
the turn's items in one splice; when the anchor item sits inside that range, the view lands on the fold head (1.3). Older
runs keep the cached segments (`older_builds`, 1.10). Markdown parses on an item's first draw, and its measured height is
held until a pending render settles (`settled_generation`, 1.4).

### Find

Satisfies: TRANSCRIPT-3.1, TRANSCRIPT-3.2, TRANSCRIPT-3.3, TRANSCRIPT-3.4, TRANSCRIPT-3.5, TRANSCRIPT-3.6, TRANSCRIPT-3.7,
TRANSCRIPT-3.8, TRANSCRIPT-3.9, TRANSCRIPT-3.10, TRANSCRIPT-3.11, TRANSCRIPT-3.12, TRANSCRIPT-3.13
Reuse: none — new code (rung 7) for `vm/find.rs`, `screens/run/find.rs` and `ui/find.rs`, new on this branch and kept
from the draft. Rung 2: the kit field `ui/field.rs`. Rung 5: `regex` 1.13.1, already in `Cargo.lock` as a transitive
dependency; the plan adds it as a direct workspace dependency pinned `=1.13.1`, for 3.9's regex mode.
Respects: ARCH-10, ARCH-16
Surface:
- Esc in the approval dock (`run/dock.rs:38`, `DenyHeld`) and the composer's stop (`ui/composer.rs:1247`): **replace**,
  both pass Esc on while the strip is open (built), and keep it when closed (3.13).
- `vm::find::{find_all, matches, count_label}`, read by `run/find.rs` and by the prose marks in `ui/markdown_view.rs:630`
  (`marks.needle`): **replace**, they take a `Query` instead of a bare `&str`, and every reader moves in the same change.
  (`open` is already `Opens<Key>`; nothing to change there.)
Interface: `vm::find::{find_all(rows, query: &Query) -> Vec<Hit>, Found { hits, current }, Opens<K>, count_label}` with
`Query { text, case, word, regex }` and an invalid-regex error; `RunScreen::{open_find, close_find, find_step}`.
Depth: if `vm/find.rs` vanished, callers would still need only a hit list of (row key, part, byte range) and a current
index; the matching rules and the owner/search open sets live inside it.
Locality: extend `vm/find.rs` (`Query` and options for 3.9 and 3.10) and `run/find.rs` (strip toggles, ⌥⌘C/W/X); leave the
rest.

The strip's toggles (case, whole word, regex) feed one `Query`. A regex that does not parse yields `Err(reason)`, and the
strip shows its error state with no count (3.10). The recount is a 150 ms throttle, armed by a store change or an
open/close; changes inside the window ride the armed deadline (3.11).

**Scan cost.** A recount rescans only the latest run. Hits in older runs are kept in a bounded `Lru` of 64 entries
(agent-decided), keyed by (segment index, `Segment.signature` at `screens/run.rs:82`, `Query`). That is the same
signature that already caches their rows (1.10, `older_builds`). Each recount is therefore bounded by one run's text, not
the thread's. Plan task 1's bench adds a check that streams with a query held in the strip. Reopen trigger: if that check
misses 16.7 ms p95, the recount moves to an owned background task (ARCH-11).

**Regex.** The pattern compiles through `regex::RegexBuilder` with Unicode on, case-insensitive unless match case is on,
and a 1 MiB `size_limit` (agent-decided). A pattern over the limit shows the error state (3.10). The crate matches in
linear time.

**Hit cap.** Hits stop at 10,000, and the count reads "10,000+" (agent-decided). That narrows 3.2: a hit past the cap is
not counted and cannot be reached by Enter. Reopen trigger: a real thread whose query passes 10,000 hits.

**Additions to the locked strip.** The toggles and the error state (3.9, 3.10) are built in the kit's toggle style. The
operator's go covers that they exist (ui-pick.md listed them as "built after your go"), not how they look on the locked
strip. They are shown at one UI stop before landing, together with the scrollbar (5.3) and the turn-end hover actions
(5.4), which were never on the go sheet. Reasoning that is open only because it streams is held in neither set;
closing search never closes it, and a stream ending never closes a row search opened while the strip is open (3.12).

### Selection and copy

Satisfies: TRANSCRIPT-2.1, TRANSCRIPT-2.2, TRANSCRIPT-2.3, TRANSCRIPT-2.4, TRANSCRIPT-2.5, TRANSCRIPT-2.6, TRANSCRIPT-2.7,
TRANSCRIPT-2.8, TRANSCRIPT-2.9, TRANSCRIPT-2.10, TRANSCRIPT-2.11, TRANSCRIPT-2.12, TRANSCRIPT-2.13
Reuse: none — new code (rung 7) for `vm/selection.rs`, `vm/anchor.rs`, `ui/markdown_view/select.rs` and
`screens/run/select.rs`, new on this branch and kept from the draft; rung 4: GPUI `TextLayout::index_for_position`.
Respects: ARCH-12
Surface:
- `transcript.rs:323-337` whole-message quiet Copy and `user-copy-<key>`: unchanged readers; their tests keep passing
  (2.11, 2.12). **frozen**, built around.
- ROAD-60 (comments) will read `selection_anchors()` (`run/select.rs:411`, `#[allow(dead_code)]` today). **compat**: kept
  unused; the follow-up that removes the allow is ROAD-60.
Interface: `Selection { anchor: Pos, head: Pos }`; `copy_text(sel, entries, shown) -> String`; `anchors(sel, entries,
shown) -> Vec<Anchor { key, slot, range: Option<Range<usize>> }>`; `RunScreen::{copy_selection, copy_markdown,
selection_anchors}`.
Depth: if these modules vanished, callers would still need only `Selection { anchor, head }`, `copy_text` and `anchors`;
the slot maps, atoms and the painted-text registry stay inside.
Locality: extend `run/select.rs` (click counts: word, line, block; shift-click extend; "Copy as Markdown" via
`anchor::source_of(sel)`); leave the renderer's `Track` registry, which registers only painted items, so its cost scales
with the viewport.

### Reading place

Satisfies: TRANSCRIPT-4.1, TRANSCRIPT-4.2, TRANSCRIPT-4.3, TRANSCRIPT-4.4, TRANSCRIPT-4.5
Reuse: rung 2 — `panel/persist.rs` (`PanelStore`; `Saver` with `SAVE_DELAY` 750 ms and `flush` at :334, which quit and
window close call through `Saved::flush`, `panel/saved.rs:58` and `app.rs:401`), `panel_key_for`, `migrated` and
`resolve_panel_key` (`run.rs:906-920`, the run-to-thread carry-over), and the projection's `item_of`.
Respects: ARCH-8 (the place is view state in an app-local file, not a second source of truth for daemon data)
Surface:
- `panel/mod.rs` `Descriptors { tabs, active, closed }` (derives `Eq`): **frozen** as a persisted shape, built around with
  an additive `#[serde(default)] place: Option<Place { key: u64, offset_pt: i32 }>`; `VERSION` stays 1; a file without it
  reads `None`.
- `screens/run/panel.rs:128` `persist_panel`, `panel/mod.rs:200` `PanelState::snapshot`, and
  `screens/run/terminal.rs:131,153` `Terms::restore`/`to_saved`: each builds `Descriptors` field by field. **replace**, each
  carries `place`, or a write would drop it.
- `run.rs:906-920` `resolve_panel_key`: on the run-to-thread move it restores the thread's saved `Descriptors`, or persists
  the run's. **replace**, the place moves with the tabs (4.4).
- The `Descriptors { .. }` struct literals in tests: `panel/mod.rs:361,550,556,576,590`, `persist.rs:360` and
  `terminal_tests.rs:839`. **replace**, each gains `place: None`, or uses `..Default::default()`. A test edit for
  compilation only; no assertion changes.
- `persist.rs:174`, the drop-empty rule: **replace**, a key holding only a `place` is kept.
- `app.rs:1594` `open_run`, which makes a fresh `RunScreen` on every open: **replace**, the outgoing screen's place is
  already in the store (each scroll event marks it), and the incoming screen reads it.
Interface: `RunScreen::{saved_place() -> Option<Place>, restore_place(Place)}`; `Place::resolve(&Projection, &rows) ->
ListOffset`, which falls back to the fold head, then the next key (4.3).
Depth: n/a — extends `panel/persist.rs` and `RunScreen`.
Locality: extend `panel/mod.rs`, `panel/persist.rs`, `run/panel.rs`, `run/terminal.rs` and `run.rs`; leave the rest.

Each scroll event records the top item's `Key` and offset into the thread's `Descriptors` in memory and marks the store
changed. The `Saver` writes once, 750 ms after the last change, so a long scroll is one write; quit and window close
flush it. While the view follows the tail it records "no place". Opening a thread with a saved place restores it after
the first layout; with none, the view opens at the tail and follows it (4.5).

### Navigation and jump to latest

Satisfies: TRANSCRIPT-5.1, TRANSCRIPT-5.2, TRANSCRIPT-5.3, TRANSCRIPT-5.4, TRANSCRIPT-5.5, TRANSCRIPT-6.1, TRANSCRIPT-6.2
Reuse: rung 2 — the `Transcript` key context and focus node (`run/select.rs:33`), the tick rail (`run.rs` `tick_rail`,
`convo::tick_marks`/`row_starts`), `kit::icon_button`, the kept `jump_control`; rung 4 — GPUI list `scroll_by`/`scroll_to`
and its scrollbar hooks (`set_offset_from_scrollbar`, `max_offset_for_scrollbar`, `scrollbar_drag_started`); new (rung 7) —
`ui/scrollbar.rs`, a scrollbar element over those hooks, because neither GPUI's `list()` nor the app has one.
Interface: actions `PageUp`, `PageDown`, `LineUp`, `LineDown`, `Home`, `End`, `PrevPrompt`, `NextPrompt` in the
`Transcript` context; `RunScreen::{scroll_page(dir), jump_to_latest}`.
Depth: if `ui/scrollbar.rs` vanished, callers would still need only to hand it a `ListState`; the thumb geometry and drag
stay inside it. The key bindings extend `run/select.rs`.
Locality: extend `run.rs` (the scrollbar beside the rail; the turn-end hover action "Scroll to user message" on every
finished turn, and "Scroll to top" on the last turn only, 5.4); leave the composer's own key bindings.

### Renderer host, code blocks, links and rich markdown

Satisfies: TRANSCRIPT-7.1, TRANSCRIPT-7.2, TRANSCRIPT-8.1, TRANSCRIPT-8.2, TRANSCRIPT-8.3, TRANSCRIPT-8.4, TRANSCRIPT-8.5,
TRANSCRIPT-8.6, TRANSCRIPT-14.1, TRANSCRIPT-14.2, TRANSCRIPT-14.3, TRANSCRIPT-14.4
Reuse: rung 2 — `ui/markdown_view.rs` (`Host`, `blocks_with`), `md/chrome.rs`, `md/link.rs` and `run/links.rs` (new on
this branch, kept); `ui/menu.rs` `popup(at: Point)` for the context menu, extended with ↑/↓/Enter/Esc keyboard navigation
(16.8); `links::open` and `open_file_at` (`run/files.rs:496`). Rung 5 — pulldown-cmark 0.12.2 options the parser does not
enable today (`crates/reno-markdown/src/parser.rs:143-147` sets only tables, strikethrough, tasklists and math):
`ENABLE_FOOTNOTES`, `ENABLE_YAML_STYLE_METADATA_BLOCKS` (front matter) and `ENABLE_GFM` (`BlockQuoteKind` for
`[!NOTE]`-style callouts).
Respects: ARCH-16
Surface:
- `crates/reno-markdown` `options()` and its block types: enabling footnotes, metadata blocks and GFM block-quote kinds
  adds block kinds (`Footnote`, `FrontMatter`, a `BlockQuote` kind, and task markers drawn as boxes). The readers are
  `ui/markdown_view.rs`, `vm/anchor.rs` (`atoms`), the Files tab preview (`run/files.rs:1138`) and the crate's 100 parser
  tests. **replace**, all move in the same change, and the 100 existing tests stay green unchanged.
- The renderer entry `blocks_with(tree, theme, Host)` is already in place (callers `run/files.rs:1138` with
  `Host::preview()`, which keeps today's links, 8.5; `run/transcript.rs:177`; `renderers_tests.rs:69`). Nothing changes.
- `cx.open_url` in the transcript path: **replace** by `links::open` (8.2). The Files tab preview keeps its own call
  (`md/link.rs:113`). A `mailto:` in the transcript becomes plain text (links.rs:27 policy).
Interface: `Host::{transcript(run, client, files), preview(), while_streaming, marked, selectable}`; `blocks_with`;
`LinkPolicy`, `FileLinks`; `chrome::card(kind, …)`.
Depth: if the renderer vanished, callers would still need only `blocks_with(tree, theme, Host)`; the block drawing,
chrome, link policy and caches stay inside.
Locality: extend `crates/reno-markdown/src/parser.rs` (the three options and their block kinds, with parser tests) and
`markdown_view.rs`:
- callouts: `[!NOTE]`, `[!TIP]`, `[!WARNING]`, `[!CAUTION]` block quotes;
- wide tables in a horizontal scroll with a scrollbar;
- footnotes at the reply's end, with a click scrolling to the note;
- front matter as a key-value table;
- HTML comments hidden;
- task boxes drawn and not toggled.
Add a context menu in `run.rs` on right-click (Open, Copy Link, Copy, Copy as Markdown, Copy Message, Scroll to top or
bottom). The menu opens on right-click and on ⇧F10 in the `Transcript` context (a new binding).

### Math and its settings

Satisfies: TRANSCRIPT-9.1, TRANSCRIPT-9.2, TRANSCRIPT-9.3, TRANSCRIPT-9.4, TRANSCRIPT-9.5, TRANSCRIPT-9.6
Reuse: rung 5 — `ratex-parser`/`-layout`/`-types`/`-svg` `=0.1.16` (added by the branch, agent-decided under Delegated:
renderers; OFL notice THIRD_PARTY_NOTICES.md:9); `resvg`/`usvg` `=0.46.0` already linked through GPUI; rung 2 — `prefs.rs`
(`typesetMath`, `mathSize`, `math(cx)`) and `settings_pages.rs` rows (built).
Respects: ARCH-12, ARCH-17
Surface: `app-settings.json`, read by `prefs.rs` load and save and by older builds: **frozen**, built around with additive
keys. Unknown keys are kept (prefs.rs:5-6), and a missing key reads as the default (16.11).
Interface: `md::math::typeset(tex, display, size, color, scale, cx) -> Option<Rc<Typeset>>`; `Prefs::math(cx) -> (bool,
u8)`.
Depth: if `md/math.rs` (rung 7, new on this branch) vanished, callers would still need only `typeset(..)` returning a
raster with its above-baseline and below-baseline heights.
Locality: leave; typesetting stays synchronous on a cache miss (0.06 ms parse and layout, 0.2 ms raster in the spike;
4 KiB source cap and an LRU of 256 entries / 24 MiB, both agent-decided). Reopen trigger: if the 16.2 bench misses its target, move it to an owned
background task with a placeholder slot.

### Diagrams

Satisfies: TRANSCRIPT-10.1, TRANSCRIPT-10.2, TRANSCRIPT-10.3, TRANSCRIPT-10.4, TRANSCRIPT-10.5, TRANSCRIPT-10.6
Reuse: rung 5 — `merman =0.11.0` (added, agent-decided; `cssparser` MPL-2.0 is allowed by deny.toml:108); the kept
`md/mermaid.rs` and its `no_files()` resolver (:282); `md/chrome.rs` frame.
Respects: ARCH-11, ARCH-12, ARCH-17
Interface: `mermaid::drawing(source, theme, scale, cx) -> Drawing { Rendering, Ready, Failed }`; the diagram card with
`Preview | Code` tabs and a zoom state.
Depth: if `md/mermaid.rs` (rung 7, new on this branch) vanished, callers would still need only `drawing(..)` and its three
states; the measurer, theme init and cache stay inside.
Locality: extend `Cargo.toml` (merman features `diagram-class`, `diagram-state`, `diagram-er`, `diagram-gantt`,
`diagram-pie`, with one test each; a type it cannot draw goes through reroute-plan) and `md/mermaid.rs` (keep the previous
`Ready` while a changed source renders, 10.2) and the card (tabs; ⌘-scroll zoom from fit-to-width to 200 % with a reset;
a re-raster 300 ms after the last zoom step (agent-decided), as an owned deadline task; pause following on zoom, 6.2).

### Images

Satisfies: TRANSCRIPT-11.1, TRANSCRIPT-11.2, TRANSCRIPT-11.3, TRANSCRIPT-11.4, TRANSCRIPT-11.5
Reuse: rung 2 — kept `md/image.rs` (`ImageSource`, `classify`, `resolve`, an LRU of 64 / 48 MiB and an 8 MiB cap, both
agent-decided), the
`fs.read_image` client call (`crates/reno-client/src/files.rs` `fs_read_image_all`), and the `no_files()` rasteriser for
SVG.
Respects: ARCH-2, ARCH-12, ARCH-16
Interface: `image::{classify(src) -> Classified { Source(ImageSource), Remote, Refused }, resolve(source, host, cx) ->
Shown}`.
Depth: if `md/image.rs` (rung 7, new on this branch) vanished, callers would still need only `classify` and `resolve`;
the chunked read, the cache and the SVG rasteriser stay inside.
Locality: extend `md/image.rs`: an SVG returned by `fs.read_image`, or given as `data:` (11.2, "the same way"), is
rasterised by `usvg` with `no_files()` instead of being refused. Lock test: an SVG whose `<image href>` names a file outside the worktree paints nothing. Leave the rest.

### Agent-posted visuals and tool-result images

Satisfies: TRANSCRIPT-12.1, TRANSCRIPT-12.2, TRANSCRIPT-12.3, TRANSCRIPT-12.4, TRANSCRIPT-12.5
Reuse: rung 2 — kept `crates/reno-client/src/visuals.rs` (`VisualDesk`), `fake/visuals.rs`, `Store::run_visuals`
(`store.rs:544`), `Capabilities.agent_visuals` and `md/visual.rs`; the ActivityDesk pattern.
Respects: ARCH-2, ARCH-13
Surface: `Capabilities` (`vm/capabilities.rs`), whose only constructors are the `REAL` and `FAKE` consts: **replace** (a
field is added to both; done). A new `tool_images` flag follows the same shape.
Interface: `VisualDesk::visuals(&RunId) -> Vec<Visual>`, plus `VisualDesk::tool_images(&RunId, call: &str) ->
Vec<VisualImage>` for 12.2; `Store::{run_visuals, tool_images}` returning empty when the capability is off.
Depth: if the desk (rung 7, new on this branch) vanished, callers would still need only `visuals(run)` and
`tool_images(run, call)` returning plain structs; the seeding stays in the fake.
Locality: extend `visuals.rs` (tool images), `fake/visuals.rs` (one seeded tool image) and `run/output.rs` (the image
under its call, scaled down to at most 384 × 384, with "Go to file" when the call names a file); leave `reno-proto`.

### Streaming, reasoning and tool calls

Satisfies: TRANSCRIPT-13.1, TRANSCRIPT-13.2, TRANSCRIPT-13.3, TRANSCRIPT-13.4, TRANSCRIPT-13.5, TRANSCRIPT-13.6,
TRANSCRIPT-13.7, TRANSCRIPT-13.8, TRANSCRIPT-13.9, TRANSCRIPT-13.10, TRANSCRIPT-13.11, TRANSCRIPT-13.12, TRANSCRIPT-13.13,
TRANSCRIPT-13.14, TRANSCRIPT-13.15, TRANSCRIPT-13.16, TRANSCRIPT-13.17, TRANSCRIPT-13.18
Reuse: rung 2:
- `screens/run/transcript.rs` (`reasoning`, `worked`, the Thinking line), `run/output.rs` (`step_row`, output block,
  Copy), the `ui/motion.rs` pulse discipline and `cx.reduce_motion()`;
- `convo::step_lines` and `line_calls`, the diff line runs (`ui/markdown_view.rs:569` `line_runs`, as the Changes tab uses),
and `prefs.rs`
  plus `settings_pages.rs` for the two settings rows.
Respects: ARCH-10, ARCH-16
Surface:
- `app-settings.json`: **frozen**, built around with additive keys `thinking` (`auto | preview | open | closed`),
  `expandNewEdits` and `expandNewCommands`.
- Tool output data (`ToolOutput`, `crates/reno-proto/src/exec.rs:246-263`): read only, **frozen**, not changed.
Interface: `StreamReveal { shown: usize }` (named apart from `vm::find::Reveal`), with `RunScreen::reveal_tick()` armed by
a delta; `ThinkingMode`; `Prefs::{thinking,
expand_new_edits, expand_new_commands}`.
Depth: n/a — extends the existing row views and prefs.

Where each tool card's data comes from. The wire is `ExecEvent::Tool { id, call: ToolCall { name, input, class } }` and
`ToolResult { id, ok, summary, output }` (`crates/reno-proto/src/exec.rs:269-281`, `:438-457`), with no per-event time:
- An edit's badge, inline diff and per-file hunks come from `call.input`: the edit's path and old/new text, or the patch
  text, read per `ActionClass`. An input shape the app does not know shows the raw input instead.
- A command, its Copy and the raw input come from `call.input`.
- The exit status is `ok`, with `summary`.
- Elapsed time: the store records when the app saw the `Tool` and the `ToolResult` arrive, on GPUI's executor clock (the
  test clock in tests). Records are keyed by (run, call id) in a bounded `Lru` of 1,024 (agent-decided), so they outlive
  the `RunScreen` that `open_run` rebuilds on every open (`app.rs:1594`). Calls restored from history show none
  (TRANSCRIPT-13.6 as corrected).
- A call canceled before output is a `Tool` with no `ToolResult` in a turn whose `Done` carries
  `StopReason::Interrupted` (`crates/reno-proto/src/run.rs:102-112`). In a turn that stopped `Completed` (or any other
  reason) such a call stays `Outcome::Pending` in its group, as today (`transcript.rs:84-95`; guard 13.15).
- A resource link is a worktree path in the result's output text, resolved like file links (`run/links.rs`).
- The `TurnBuilder` call index (Row projection) gives each card its `ToolCall`. `ToolEntry` and `Row` stay unchanged.
Locality: extend `run/transcript.rs`:
- reasoning open while streaming in Auto, capped at 16 lines in Preview;
- empty replies and tool calls canceled before any output are hidden.
Extend `run/output.rs`:
- an edit call gets a file badge and an inline diff, with one hunk block per file for a patch;
- a command gets its own Copy, elapsed time and exit status;
- a numbered read-file block;
- a "Raw input" disclosure;
- an error callout with "Copy error";
- a resource-link button.
Extend `run.rs` for the 16 ms reveal deadline. Leave the Thinking line, inline approvals, chips, the parser's reparse and
the tool grouping (13.10–13.15, guards).

### Fake scenes

Satisfies: TRANSCRIPT-15.1, TRANSCRIPT-15.2, TRANSCRIPT-15.3, TRANSCRIPT-15.4
Reuse: rung 2 — kept `crates/reno-client/src/fake/transcript.rs` and `fake/{script,data,files,world/threads}.rs`;
`FakeConfig::transcript_threads`, which the app's `--fake` turns on (`store.rs:1002`).
Interface: the seeded runs `acp1`–`acp8` and `render1`, and the prompt script `/longreply`; `FakeConfig::transcript_threads`.
Depth: if the seeded scenes (rung 7, new on this branch) vanished, callers would still need only the run ids and
`FakeConfig::transcript_threads`; the event scripts stay inside the fake.
Locality: extend `fake/transcript.rs` with the seeded content the unbuilt stories need, each behind the same flag:
- a tool-result image;
- an error turn;
- a patch over two files;
- a numbered file read;
- a callout, a wide table, footnotes and front matter.
Leave the fake's existing scenes (15.4 guard).

### Bench scenarios and the frame budget

Satisfies: TRANSCRIPT-16.1, TRANSCRIPT-16.2, TRANSCRIPT-16.3
Reuse: rung 2 — `xtask/src/app_bench.rs` (`long_transcript`), `xtask/src/app_stats.rs` (the budget table, `fails` :349),
`apps/renod/src/executors.rs` `StreamMock`, `apps/reno-app/src/bench.rs` (`ScrollPlan`, `RENO_BENCH_SCROLL`).
Respects: ARCH-16
Surface:
- `app_stats.rs:86-91`, the row `long_frame_p95_ms` (`target: Some("ROAD-33")`), and `docs/BENCHMARKS.md:165-174`:
  **replace**, the row becomes a budget and the doc line is rewritten.
- `StreamMock::{Steady, Burst}` in renod, read by `apps/renod/src/main.rs:30-33` (`parse_stream_mock`), `daemon.rs:75` and
  `executors.rs:152-172` and their tests: **replace**, adding `LongReply` and the word `longreply`, with the existing kinds
  unchanged (1.11 guard). xtask passes the word as a string to `--stream-mock`.
- `app_stats.rs:399,724,731-754` (the target-row tests and the report) and `BENCHMARKS.md:172,198`: **replace**, the row
  is now reported and tested as a budget.
Interface: `renod --stream-mock=longreply`; `long_reply` scenario in `cargo xtask bench --app`.
Depth: n/a — extends the bench.
Locality: extend `apps/renod/src/executors.rs` (a bench-only stream of one very long reply with code blocks and math),
`xtask/src/app_bench.rs` and `xtask/src/app_stats.rs`, and `docs/BENCHMARKS.md`. Plan task 1 runs the real long-transcript
scenario before and after (U1). Peak footprint and the streaming, idle and cold-start budgets are checked in the same run
(16.3).

### Limits, contracts and safety (infrastructure)

Satisfies: TRANSCRIPT-16.4, TRANSCRIPT-16.5, TRANSCRIPT-16.6, TRANSCRIPT-16.7, TRANSCRIPT-16.8, TRANSCRIPT-16.9,
TRANSCRIPT-16.10, TRANSCRIPT-16.11, TRANSCRIPT-16.12, TRANSCRIPT-16.13
Reuse: rung 2 — `md/lru.rs` `Lru<K, V>` for every cache; `links::open` and its test guard; GPUI's `HttpClient` seam for
the zero-send test; `protocol_schema_matches_snapshot`; the persist and prefs tests; the kit focus ring; `cargo deny` and
`cargo machete`.
Respects: ARCH-12, ARCH-13, ARCH-16, ARCH-17
Surface: `RunScreen.markdown`, `.probed` and `file_index.dirs` are unbounded today (scan-design §6). **replace** each with
an `Lru`:
- parsed trees: 512 entries (agent-decided), reparsed on demand. The live streaming row's entry is pinned and never
  evicted, since its incremental parser holds state. A top-to-bottom scroll of 2,000 messages evicts and reparses
  replies inside the frames that draw them, so 16.1's bench is the check. Reopen trigger: if 16.1 misses its target,
  raise the bound, or hold the parse off-frame behind a placeholder, as Math would.
- probed directories: 256 entries (agent-decided);
- listed directories: 256 entries (agent-decided).
Also bounded:
- `Found.hits` stops at 10,000 hits, and the count reads "10,000+" (agent-decided; see Find);
- text layout is GPUI's own per-frame line-layout cache, bounded by the viewport.
Interface: none new.
Depth: n/a — applies `Lru` to three maps.
Locality: extend `run.rs` and `run/links.rs`. Leave the rest, which is covered by the lock tests listed in `ui-brief.md`
and the tests named in Seams.

## UI design

Grounding: lifted from the locked `./ui-brief.md`, including its Decision, Signature and Amendments. Tokens come from the
app's `Theme` (`ui/theme.rs`), with values in `docs/design/2026-11-19-app-shell-ui-brief.md`. Precedence: the owner's words
(the card, the pick "A, thêm vạch kết quả trên minimap lấy từ B", and the go), then the shell brief and the kit, then the
variant plan.
- The surfaces the brief locked are lifted 1:1.
- The surfaces still to be built follow the brief's signature (nothing shows until you reach for it) and use kit pieces
  only.
- Under the charter's UI stop, any change to what a locked surface shows or does goes back to the operator.

### Find strip (⌘F)

Layout:
- a 36 pt strip pinned to the top of the transcript canvas, over the scrolling list;
- left to right: a search icon, the query field, the count, ↑ ↓ and ✕;
- the case, word and regex toggles sit inside the field's right edge.

Components:
- rung 1 — `ui/field.rs`, `kit::icon_button`, `kit::tip`, `kit::focus_ring`;
- new (rung 7) — the strip frame (no kit piece is a bar pinned over a list).

States: closed · open and empty · "N of M" · "No results" · counting while the thread loads (the count restarts at the
first hit until one is gone to) · "10,000+" · an invalid regex (the field in its `danger` state with the reason, and no
count) · a toggle on (the icon pressed).

Type & color:
- the strip on `surface`; the count in Geist 13 `muted`;
- a match gets a `warning` tint; the current match an `accent` fill with `on_fill` text.

A11y:
- keyboard: ⌘F, Enter/⌘G, ⇧Enter/⇧⌘G, ⌥⌘C/W/X, Esc;
- labels on every control, with the kit focus ring;
- while open, Esc goes only to the strip.

### Tick rail with result ticks

Layout: the existing prompt rail. While the strip holds a query, a short `accent` tick marks each row with a hit, and the
current hit's tick is longer. The rail shows only when the transcript scrolls.

Components: rung 2 — `tick_rail` and `convo::tick_marks`, extended (`find_rail_ticks`).

States: no query · hits · current moves · strip closed (the result ticks go).

Type & color: result ticks in `accent`; prompt ticks unchanged.

A11y: a pointer aid; the same navigation is on the keyboard through the strip.

### Transcript rows, folds, reasoning and tool calls

Layout:
- one item per entry;
- fold heads are the single muted line "Worked for <duration> ›";
- a hit inside an edit step names the file;
- reasoning opens while streaming in Auto, and is capped at 16 lines in Preview;
- tool calls collapse by default; a new edit or command opens inside its group.

Components:
- rung 1 — the existing row views, and `kit::icon_button` for each Copy;
- rung 2 — the Changes tab's diff line runs for inline diffs.

States: fold closed / owner-opened / search-opened · streaming · finished · a canceled call or an empty reply (hidden) · an
error turn (a callout with "Copy error") · an output cut by the daemon ("Output was cut: <size> total").

Type & color: unchanged from the shell and run-activity briefs. Diffs use the Changes tab's `success` and `danger` tints.

A11y: fold heads and disclosures are buttons with labels; "Raw input" is a disclosure button.

### Selection, copy and the context menu

Layout:
- a drag selects across entries in reading order, autoscrolls past the edge, and survives scrolling;
- double, triple and quadruple click select a word, a line, a block; a shift-click extends;
- right-click opens a menu: Open · Copy Link (on a link), Copy · Copy as Markdown (with a selection), Copy Message (on a
  reply), Scroll to top / Scroll to bottom.

Components:
- rung 1 — `ui/menu.rs`, `ui/toast.rs` ("Copied");
- new (rung 7) — the selection tracker (no kit piece selects across list items).

States: none · selecting · selected · copied (toast) · menu open.

Type & color: the selection background is `accent` at low alpha; the menu uses the kit's colours.

A11y: ⌘C only when the transcript has focus; the composer's ⌘C is unaffected; ⇧F10 opens the menu (a new binding in the
`Transcript` context), and ↑/↓/Enter/Esc move through it (`ui/menu.rs` extended), with focus rings.

### Navigation: jump to latest, scrollbar and keys

Layout:
- a round 32 pt jump button over the bottom-right of the transcript, shown only while detached;
- a vertical scrollbar beside the tick rail;
- on hover at the end of a finished turn: "Scroll to user message"; on the last turn also "Scroll to top".

Components: rung 1 — `kit::icon_button`, `kit::tip`; new (rung 7) — `ui/scrollbar.rs` over the list's scrollbar hooks
(the thumb on `raised`, hover `faint`; no new token).

States: following (no button) · detached (button) · pressed (follows again) · UI-grown content (detached).

Type & color: the kit button on `surface`, with an arrow-down icon; the hover actions in `muted`.

A11y: PgUp/PgDn, ↑/↓, Home/End, ⇧PgUp/⇧PgDn in the `Transcript` context; "Jump to latest" carries a label.

### Rich blocks: code, diagrams, math, images, visuals, markdown extras

Layout:
- code and diagrams sit in one frame, with Copy and Wrap (code) or Copy (diagram) at the top-right on hover;
- a diagram has Preview | Code tabs, and ⌘-scroll zoom from fit-to-width to 200 % with a zoom readout and reset;
- a diagram fits the column, never below 3/4 of its size;
- display math sits on its own line at 1.2 × body × the Math size setting; inline math on the baseline;
- images are scaled down to the column; a remote image is a link card;
- posted visuals follow their entry, with a caption; tool images under their call, at most 384 × 384;
- callouts carry their kind's label and colour; wide tables scroll sideways; footnotes sit at the end; front matter is a
  key-value table; task boxes are drawn.

Components:
- rung 1 — `kit::icon_button`, the toast;
- new (rung 7) — `ui/markdown_view/{math,mermaid,picture,chrome,visual,link,lru}.rs` (built).

States:
- loading: "Rendering…" with the source; "Loading image…"; "Drawing…";
- drawn; zoomed (readout and reset);
- failed: a diagram or formula falls back to its source as code; "Image not loaded: <alt or path>"; "Visual not loaded";
- a remote image as a link card;
- Typeset math off: math shows as its TeX source.

Type & color:
- math in `text`;
- diagrams: background `surface`, nodes `raised`, borders `faint`, lines `muted`, labels `text`;
- callouts: note `accent`, tip `success`, warning `warning`, caution `danger`;
- code in Geist Mono.

A11y: Copy, Wrap, the tabs and the zoom reset carry labels and focus rings; a link card is a button through the app's link
opener.

### Settings rows (Appearance and General)

Layout:
- Settings › Appearance: "Typeset math" (switch) and "Math size" (90–150 %) beside the existing rows (built);
- Settings › General: "Thinking" (Auto · Preview · Always open · Always closed), "Expand new edits" and "Expand new commands"
  (switches).

Components: rung 1 — the existing settings row and switch kit (`screens/settings_pages.rs`).

States: on/off · a chosen size or mode · the default when the key is missing.

Type & color: as the settings pages brief.

A11y: switches and choices are keyboard reachable, with labels.

## Seams for testing

One new seam: the incremental `TurnBuilder` (unit). Every other seam exists today.

| Seam | Kind | Covers |
|---|---|---|
| `vm::items` (projection, `diff`, `Plan`) and the incremental `TurnBuilder` beside `transcript.rs` `build_rows` | unit | 1.1, 1.2, 1.3, 1.5, 1.6, 1.7 |
| `RunScreen` on the fake store (`tests.rs` `fake_store`, `window`, `draw`, `click`; `find_tests.rs`, `select_tests.rs`, `convo_tests.rs`, `thread_tests.rs`, `activity_tests.rs`, `changes_tests.rs`) | integration | 1.3, 1.4, 1.5, 1.8, 1.9, 1.10, 2.1–2.5, 2.10–2.12, 3.1–3.13, 4.2–4.5, 5.1–5.5, 6.1, 6.2, 8.1–8.4, 12.1–12.5, 13.1–13.18 |
| `vm::find` | unit | 3.2, 3.3, 3.9, 3.10, 3.11 |
| `vm::selection`, `vm::anchor` | unit | 2.4–2.9, 2.13 |
| `markdown_view` render tests (`renderers_tests.rs`, `markdown_view` and `md/*` tests) | integration | 2.6, 7.1, 7.2, 8.2, 8.5, 8.6, 9.1–9.4, 10.1–10.6, 11.1–11.5, 14.1–14.4, 16.4, 16.5, 16.6, 16.7, 16.9 |
| `panel::persist` (`PanelStore`, `Saver`, a file from before this feature) | unit | 4.1, 16.11 |
| `prefs` and `settings_pages_tests.rs` | unit / integration | 9.5, 9.6, 13.17, 13.18, 16.11, 16.13 |
| `reno-client` fake (`crates/reno-client/tests/fake_transcript.rs`, `fake::transcript` tests) | integration | 15.1–15.4 |
| `reno-proto` `protocol_schema_matches_snapshot` | unit | 16.10 |
| `xtask` `app_stats` tests, `renod` executors tests, `cargo xtask bench --app` (manual, macOS) | unit / e2e | 1.11, 16.1, 16.2, 16.3 |
| Key and focus checks on the fake store (`simulate_keystrokes`) | integration | 16.8 |
| `cargo deny check`, `cargo machete` | check | 16.12 |

## Coverage check

Every requirement ID from TRANSCRIPT-1.1 to TRANSCRIPT-16.13 appears in exactly one `Satisfies:` line above:
- story 1 → Row projection;
- story 2 → Selection and copy;
- story 3 → Find;
- story 4 → Reading place;
- stories 5 and 6 → Navigation;
- stories 7, 8 and 14 → Renderer host;
- story 9 → Math;
- story 10 → Diagrams;
- story 11 → Images;
- story 12 → Visuals;
- story 13 → Streaming, reasoning and tool calls;
- story 15 → Fake scenes;
- 16.1–16.3 → Bench;
- 16.4–16.13 → Limits.

No ID is deliberately unmapped. No requirement was changed by this design: TRANSCRIPT-10.1 (seven diagram types) and
TRANSCRIPT-11.1 (worktree images, SVG included) stand as approved, and the plan closes the draft's gaps in both.
