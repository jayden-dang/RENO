# Tasks: Reading a conversation (transcript)

> **For agentic workers:** after plan approval, pick one execute skill:
> - `build` (wave mode): a continuous, dependency-aware scheduler;
> - `build` (story mode): human-gated story review units;
> - `build` (inline mode): the controller implements, with no implementer subagents.
>
> The chosen skill writes `Execution-mode:`. Steps use checkbox (`- [ ]`) syntax for tracking.

Feature code: TRANSCRIPT
Status: Draft
Date: 2026-11-22
Execution-mode: unset
Max-concurrency: auto
Requirements: ./requirements.md
Design: ./design.md
UI brief: ./ui-brief.md (Locked)

**Goal:** Finish ROAD-33 on the kept draft code, so the owner reads, finds, selects and copies anything in a conversation of
any length without a dropped frame, with typeset math, diagrams, images and agent-posted visuals.

**Architecture:**
- The nested `Row` model stays what the lock tests read. A flat projection (`vm/items.rs`) drives the GPUI list, and an
  incremental turn builder replaces the per-event rebuild.
- Find, selection, the reading place and the renderers are modules already kept on `feat/transcript`. The tasks finish
  what `RUN/scan-design.md` §2 marks partial or missing.
- Each task also re-runs the tests that hold what the draft already built.

**Tech Stack:**
- Rust 2024 (`rust-version` 1.98), GPUI (pinned git rev 596dfdc).
- `pulldown-cmark` 0.12.2 (`reno-markdown`), `ratex-*` `=0.1.16`, `merman` `=0.11.0`, `resvg`/`usvg` `=0.46.0`.
- Tests: `cargo test` with `#[gpui::test]` on the in-process fake daemon (`reno-client` feature `fake`).
- Bench: `cargo xtask bench --app` (macOS, manual).

## Global Constraints

Every task follows these sources; none is copied here.
- **Engineering standards:** `docs/standards/INDEX.md` (sha256 `c9022b57a31d…`) and `docs/standards/testing.md`
  (sha256 `bfd06e83b4ca…`): test-first; no network, no real harness, no wall clock; a test never reaches the owner's
  machine (a browser, the Keychain, `launchctl`, `~/.reno`); dependencies declared once in the workspace manifest;
  `cargo deny` and `cargo machete` pass.
- **Architecture spine:** `docs/architecture/INDEX.md` (sha256 `e052f5366f23…`). Hard invariants every task inherits:
  - **ARCH-2:** the app reaches the daemon only through `reno-client`.
  - **ARCH-10:** nothing polls; every timer is a deadline or a bounded tick tied to its cause.
  - **ARCH-11:** every task is owned.
  - **ARCH-12:** every cache is bounded.
  - **ARCH-13:** no wire change; the `protocol_schema` snapshot stays as it is.
  - **ARCH-16:** tests have no network and no wall clock.
  - **ARCH-17:** permissive, pinned dependencies.
- **Verify commands** (`docs/agents/project.md`), all of which must pass before a task's commit:
  - `cargo check --workspace --all-targets`;
  - `cargo fmt --all --check`;
  - `cargo clippy -p reno-app -p reno-client -p reno-markdown --all-targets -- -D warnings`. Workspace-wide clippy has a
    failure from before this branch, in `xtask/tests/wiring_diagram.rs`.
  - the task's own test command.
- **Run boundaries** (charter): never touch `dev.reno.renod`, `~/Library/Application Support/Reno`, `~/.reno`,
  `~/.local/bin`, `launchctl` or the Keychain. A `--fake` run with `RENO_DATA_DIR` points under a temp dir.
- **Names:** tracked files name no outside project; the reference is "the reference agent panel".
- **Lock tests:** the lock tests named in `./ui-brief.md` and every test that existed before this branch stay green and
  unedited, except for the compile-only `Descriptors` struct-literal additions in Task 8.
- Team: Solo (`docs/agents/project.md` `## Team`).

## File Structure

Paths are under `apps/reno-app/src/` unless they say otherwise.

| File | Responsibility | Tasks |
|---|---|---|
| `crates/reno-client/src/fake/transcript.rs` | Seeded fake threads and their content | 1 |
| `crates/reno-client/src/fake/visuals.rs` | Fake visuals desk, including tool images | 1, 21 |
| `crates/reno-client/src/visuals.rs` | `VisualDesk` trait (`visuals`, `tool_images`) | 21 |
| `crates/reno-client/tests/fake_transcript.rs` | Fake scene tests | 1 |
| `transcript.rs` | Row model; `TurnBuilder` beside `build_rows` | 3 |
| `vm/items.rs` | Flat projection | 3 |
| `screens/run.rs` | `RunScreen`: refresh, list, caches, place, keys, scrollbar, turn-end actions, reveal, context menu | 3, 4, 8, 9, 10, 11, 14, 17, 22 |
| `vm/find.rs` | Find model: `Query`, hits, `Opens<K>` | 6 |
| `screens/run/find.rs` | Find strip and recount | 6 |
| `vm/selection.rs`, `vm/anchor.rs` | Selection model, source anchors, Copy as Markdown source | 7 |
| `screens/run/select.rs` | Pointer and key handling for selection; key context `Transcript` | 7, 9, 17 |
| `ui/markdown_view/select.rs` | Painted-text registry, click counts | 7 |
| `panel/mod.rs`, `panel/persist.rs` | `Descriptors.place`, drop-empty rule | 8 |
| `screens/run/panel.rs`, `screens/run/terminal.rs` | Field-by-field `Descriptors` builders | 8 |
| `ui/scrollbar.rs` (new) | Scrollbar element over GPUI list hooks | 10 |
| `ui/mod.rs` | Module list | 10 |
| `screens/run/transcript.rs` | Row drawing: turn-end actions, reasoning modes, empty and canceled rows, reveal | 5, 11, 22, 25 |
| `screens/run/output.rs` | Tool cards: images, edit badge and diff, command, read-file, raw input, error, resource link | 21, 24, 25 |
| `ui/markdown_view.rs` | Renderer: callouts, tables, footnotes, front matter, task boxes, diagram card | 12, 13, 16, 18 |
| `ui/markdown_view/mermaid.rs` | Diagram types, previous drawing, zoom re-raster | 12, 13 |
| `ui/markdown_view/chrome.rs` | Card chrome: tabs, zoom readout and reset | 12, 13 |
| `ui/markdown_view/image.rs` | Worktree SVG through `no_files()` | 20 |
| `ui/menu.rs` | Keyboard navigation in popups | 17 |
| `actions.rs` | Key bindings: paging, ⇧F10, find toggles | 6, 9, 17 |
| `prefs.rs`, `screens/settings_pages.rs` | Thinking mode; expand-new-call settings | 5, 23 |
| `crates/reno-markdown/src/parser.rs` | Footnotes, metadata blocks, GFM block-quote kinds | 16 |
| `Cargo.toml` (workspace), `apps/reno-app/Cargo.toml` | `regex =1.13.1`; merman diagram features | 6, 12 |
| `apps/renod/src/executors.rs`, `apps/renod/src/main.rs`, `apps/renod/src/daemon.rs` | `StreamMock::LongReply` | 28 |
| `xtask/src/app_bench.rs`, `xtask/src/app_stats.rs` | Long-reply scenario; frame budget; find-while-streaming check | 28 |
| `docs/BENCHMARKS.md` | Budget text and results | 2, 28 |
| Test files: `screens/run/{find,select,convo,changes,panel,persist,terminal}_tests.rs`, `tests.rs`, `thread_tests.rs`, `activity_tests.rs`, `renderers_tests.rs`, `settings_pages_tests.rs` | Tests at the agreed seams | all |

Retrieval (load-subgraph, 2026-11-22):
- `blast_radius` on every mapped path returns no codes; `cluster(TRANSCRIPT)` returns TRANSCRIPT alone. owns_coverage is
  **0 of 10**: no catalog card has a `tasks.md` yet, so the result is empty for lack of coverage.
- Surface-root neighbours, from the catalog query:
  - MOCKDESK (fake): Tasks 1 and 21;
  - SETTINGS (`prefs.rs`): Tasks 5 and 23;
  - APPBENCH: Tasks 2 and 28;
  - RUNACTIVITY (chips): guarded in Task 25.

---

### Task 1: Seed the fake threads every later story needs

**Files:**
- Modify: `crates/reno-client/src/fake/transcript.rs`, `crates/reno-client/src/fake/visuals.rs`
- Test: `crates/reno-client/tests/fake_transcript.rs`

**Reuse:** rung 2 — kept `crates/reno-client/src/fake/transcript.rs` and `fake/{script,data,files,world/threads}.rs`;
`FakeConfig::transcript_threads`, which the app's `--fake` turns on (`store.rs:1002`).

**Interfaces:**
- Consumes: `FakeConfig::transcript_threads`.
- Produces: seeded run `render2`, "Render check, part 2", in the same thread as `render1`. It holds:
  - an error turn;
  - a patch over two files;
  - a numbered file read with raw input;
  - a resource link in a tool result;
  - a tool call canceled before output;
  - a `[!NOTE]` callout;
  - a table wider than the column;
  - two footnotes;
  - YAML front matter;
  - task boxes;
  - an HTML comment;
  - an SVG worktree image `docs/flow.svg`, served by the fake tree.
- Produces: constants `RENDER2_RUN = "render2"` and `TOOL_IMAGE_CALL`.

**Depends-on:** none

- [ ] **Step 1:** Add the failing test `the_second_render_run_has_every_remaining_kind_of_content` to `fake_transcript.rs`.
  It asserts that the seeded `render2` events contain each item listed above.
  Run: `cargo test -p reno-client --features fake the_second_render_run` — expect: fails, because there is no run
  `render2`.
- [ ] **Step 2:** Seed `render2` behind `FakeConfig::transcript_threads`, and serve `docs/flow.svg` from the fake tree.
  Run the test above — expect: pass.
- [ ] **Step 3:** Run `cargo test -p reno-client --features fake` and `cargo test -p reno-app` — expect: pass, with no
  existing test changed (15.4).
- [ ] **Step 4:** Commit `fake: a second render run with the content the remaining stories draw`.

_Requirements: TRANSCRIPT-15.1, TRANSCRIPT-15.2, TRANSCRIPT-15.3, TRANSCRIPT-15.4_

---

### Task 2: Measure the long transcript on the real bench, before the row work (U1)

**Files:**
- Modify: `docs/BENCHMARKS.md` (results table only)

**Reuse:** rung 2 — `xtask/src/app_bench.rs` (`long_transcript`), `xtask/src/app_stats.rs`.

**Interfaces:**
- Consumes: `cargo xtask bench --app`.
- Produces: a recorded long-transcript p95 and p50 for the branch head, set beside the 31.4 ms baseline.

**Depends-on:** none

- [ ] **Step 1:** Manual acceptance. On a quiet Mac with the window in front, run `cargo xtask bench --app` and record the
  `long_frame_p95_ms`, p50 and peak footprint rows.
- [ ] **Step 2:** Compare with the 31.4 ms baseline (BENCHMARKS.md:198).
  - p95 ≤ 16.7 ms: the U1 cause is confirmed. Record it in `BENCHMARKS.md` under the ROAD-33 results.
  - Otherwise: stop and take it through `reroute-plan` before Task 3.
- [ ] **Step 3:** Commit `bench: long transcript measured on the item-per-entry list (ROAD-33 U1)`.

_Requirements: TRANSCRIPT-1.1_

---

### Task 3: Build the latest run incrementally instead of rebuilding it per event

**Files:**
- Modify: `transcript.rs`, `screens/run.rs` (`refresh` and `rebuild_rows`, near :627), `vm/items.rs`
- Test: `transcript.rs` (tests module), `screens/run/find_tests.rs`, `tests.rs`, `thread_tests.rs`

**Reuse:** none — new code (rung 7) for the projection `vm/items.rs`, which is new on this branch and kept from the draft;
it builds on rung 2 (`transcript.rs` `build_rows`, `fold_turns`, `Key`) and rung 4 (GPUI `ListState::remeasure_items`).

**Interfaces:**
- Consumes: `Projection`, `diff`, `Plan { splice, remeasure }` (`vm/items.rs`).
- Produces:
  - `TurnBuilder` with `TurnBuilder::new(segment: usize)`, `push(&mut self, seq: Seq, event: &ExecEvent) -> Change` and
    `rows(&self) -> &[Row]`;
  - `enum Change { Grew(Key), Added(Key), Folded { turn: Range<usize>, fold: Key } }`.

**Depends-on:** Task 2

- [ ] **Step 1:** In `transcript.rs`, add the failing unit test `a_turn_builder_matches_build_rows_event_by_event`. It feeds
  the seeded handshake run (`fake::transcript`) one event at a time and asserts that `rows()` equals
  `fold_turns(build_rows(..))` after every event. Add `a_done_folds_the_turn_as_one_change`.
  Run: `cargo test -p reno-app turn_builder` — expect: fails to compile, with `TurnBuilder` undefined.
- [ ] **Step 2:** Implement `TurnBuilder` with the same row rules as `build_rows` and `fold_turns`. Run the tests above —
  expect: pass.
- [ ] **Step 3:** In `find_tests.rs`, add the failing GPUI test `a_streamed_delta_remeasures_one_item_and_rebuilds_nothing`.
  It counts `rebuild_rows` calls and asserts zero after the first build.
  Run: `cargo test -p reno-app a_streamed_delta` — expect: fails, with one rebuild per event.
- [ ] **Step 4:** Route `refresh` through `TurnBuilder` for the latest run, keep `older_builds` for older runs, and make a
  turn's fold one splice that keeps the anchor (1.3). Hold an item's measured height until its render settles (1.4). Run:
  `cargo test -p reno-app` — expect: pass, including every kept test for 1.1 and 1.5–1.11 (`find_tests`, `tests.rs`
  `streaming_events_append_rows_in_order`, `thread_tests.rs`
  `the_older_runs_are_not_built_again_on_every_change`, `convo_tests.rs`).
- [ ] **Step 5:** Commit `transcript: the latest run is built event by event, and a finished turn folds in one splice`.

_Requirements: TRANSCRIPT-1.1, TRANSCRIPT-1.2, TRANSCRIPT-1.3, TRANSCRIPT-1.4, TRANSCRIPT-1.5, TRANSCRIPT-1.6, TRANSCRIPT-1.7, TRANSCRIPT-1.8, TRANSCRIPT-1.9, TRANSCRIPT-1.10, TRANSCRIPT-1.11_

---

### Task 4: Bound every map the run screen grows

**Files:**
- Modify: `screens/run.rs` (`markdown`, `probed`), `screens/run/links.rs` (`file_index.dirs`), `vm/find.rs` (hit cap)
- Test: `screens/run/find_tests.rs`, `vm/find.rs` (tests module)

**Reuse:** rung 2 — `md/lru.rs` `Lru<K, V>` for every cache; `links::open` and its test guard; GPUI's `HttpClient` seam for
the zero-send test; `protocol_schema_matches_snapshot`; the persist and prefs tests; the kit focus ring; `cargo deny` and
`cargo machete`.

**Interfaces:**
- Consumes: `Lru::{new, get, insert, remove}`.
- Produces: `RunScreen.markdown: Lru<Key, Streaming>` with `pin(key)`; `HIT_CAP: usize = 10_000`;
  `count_label` reads "10,000+".

**Depends-on:** Task 3

- [ ] **Step 1:** Add failing tests:
  - `the_parsed_trees_stop_at_their_bound_and_never_drop_the_live_reply`;
  - `the_directory_probes_stop_at_their_bound`;
  - `hits_stop_at_ten_thousand_and_the_count_says_so`.
  Run: `cargo test -p reno-app bound` — expect: fails, because the maps are unbounded.
- [ ] **Step 2:** Replace the three maps with `Lru`, with 512, 256 and 256 entries (agent-decided), and pin the live
  streaming key. Cap `find_all` at `HIT_CAP`.
  Run: `cargo test -p reno-app` — expect: pass.
- [ ] **Step 3:** Commit `transcript: bounded parse, probe and directory caches; find stops at 10,000 hits`.

_Requirements: TRANSCRIPT-16.4_

---

### Task 5: Reasoning shows open while it streams, with a Thinking setting

**Files:**
- Modify: `screens/run/transcript.rs` (`reasoning`), `prefs.rs`, `screens/settings_pages.rs`, `screens/run.rs`
- Test: `screens/run/convo_tests.rs`, `settings_pages_tests.rs`, `prefs.rs` (tests module)

**Reuse:** rung 2:
- `screens/run/transcript.rs` (`reasoning`, `worked`, the Thinking line), `run/output.rs` (`step_row`, output block,
  Copy), the `ui/motion.rs` pulse discipline and `cx.reduce_motion()`;
- `convo::step_lines` and `line_calls`, the diff line runs (`ui/markdown_view.rs:569` `line_runs`, as the Changes tab
  uses), and `prefs.rs` plus `settings_pages.rs` for the two settings rows.

**Interfaces:**
- Consumes: `Opens<Key>` (`vm/find.rs`).
- Produces:
  - `enum ThinkingMode { Auto, Preview, Open, Closed }`, under app-settings key `thinking` (default `auto`);
  - `Prefs::thinking(cx) -> ThinkingMode`;
  - `RunScreen::streaming_open: Option<Key>`, a third open source held in neither the owner's set nor search's.

**Depends-on:** Task 3

- [ ] **Step 1:** Add failing tests:
  - `in_auto_reasoning_is_open_while_it_streams_and_closes_when_it_ends`;
  - `an_owner_toggle_while_streaming_keeps_the_owners_choice`;
  - `preview_caps_streaming_reasoning_at_sixteen_lines`;
  - `the_thinking_setting_round_trips_and_a_missing_key_is_auto`.
  Run: `cargo test -p reno-app thinking` — expect: fails.
- [ ] **Step 2:** Implement the four modes and the Settings › General row. Run: `cargo test -p reno-app` — expect: pass.
- [ ] **Step 3:** Commit `transcript: reasoning opens while it streams; Thinking setting in General`.

_Requirements: TRANSCRIPT-13.3, TRANSCRIPT-13.17_

---

### Task 6: Find with match case, whole word and regex, and a recount bounded to the live run

**Files:**
- Modify: `vm/find.rs`, `screens/run/find.rs`, `ui/markdown_view.rs` (the `marks.needle` reader, near :630), `actions.rs`,
  `Cargo.toml` (workspace `regex = "=1.13.1"`), `apps/reno-app/Cargo.toml`
- Test: `vm/find.rs` (tests module), `screens/run/find_tests.rs`

**Reuse:** none — new code (rung 7) for `vm/find.rs`, `screens/run/find.rs` and `ui/find.rs`, which are new on this branch
and kept from the draft. Rung 2: the kit field `ui/field.rs`. Rung 5: `regex` 1.13.1, already in `Cargo.lock` as a
transitive dependency; the plan adds it as a direct workspace dependency pinned `=1.13.1`, for 3.9's regex mode.

**Interfaces:**
- Consumes: `ThinkingMode` and `RunScreen::streaming_open` (Task 5); `HIT_CAP` (Task 4).
- Produces:
  - `struct Query { text: String, case: bool, word: bool, regex: bool }`;
  - `Query::compile(&self) -> Result<Matcher, String>` (`RegexBuilder`, Unicode, `size_limit(1 << 20)`);
  - `find_all(rows, &Matcher) -> Vec<Hit>`;
  - segment hit cache `HashMap<(SegmentSignature, Query), Vec<Hit>>`;
  - actions `ToggleMatchCase`, `ToggleWholeWord`, `ToggleRegex` (⌥⌘C, ⌥⌘W, ⌥⌘X).

**Depends-on:** Task 4, Task 5

- [ ] **Step 1:** Add failing unit tests in `vm/find.rs`:
  - `match_case_whole_word_and_regex_each_change_the_count`;
  - `a_regex_that_does_not_parse_or_is_too_large_says_why`.
  Run: `cargo test -p reno-app vm::find` — expect: fails to compile, with `Query` undefined.
- [ ] **Step 2:** Implement `Query` and `Matcher`, and move `find_all`, `matches` and the prose marks to them.
  Run: `cargo test -p reno-app vm::find` — expect: pass.
- [ ] **Step 3:** Add failing GPUI tests in `find_tests.rs`:
  - `toggles_and_their_keys_recount_and_an_invalid_regex_shows_no_count`;
  - `a_recount_while_streaming_rescans_only_the_live_run`, which counts the rows scanned per recount;
  - `closing_search_leaves_reasoning_open_while_it_streams`.
  Run: `cargo test -p reno-app find_tests` — expect: fails.
- [ ] **Step 4:** Add the strip's toggles in the kit toggle style, the error state, and the per-segment hit cache keyed by
  `(signature, Query)`. Make the streaming-open source survive `end_search`.
  Run: `cargo test -p reno-app` — expect: pass, including the kept find tests (3.1–3.8, 3.13).
- [ ] **Step 5:** Run `cargo deny check` and `cargo machete` — expect: pass. Commit
  `find: match case, whole word and regex; recount rescans only the live run`.

_Requirements: TRANSCRIPT-3.1, TRANSCRIPT-3.2, TRANSCRIPT-3.3, TRANSCRIPT-3.4, TRANSCRIPT-3.5, TRANSCRIPT-3.6, TRANSCRIPT-3.7, TRANSCRIPT-3.8, TRANSCRIPT-3.9, TRANSCRIPT-3.10, TRANSCRIPT-3.11, TRANSCRIPT-3.12, TRANSCRIPT-3.13_

---

### Task 7: Word, line and block clicks, shift-click, and Copy as Markdown

**Files:**
- Modify: `screens/run/select.rs` (`select_down` ignores clicks and modifiers today, near :237), `vm/selection.rs`,
  `vm/anchor.rs`, `ui/markdown_view/select.rs`
- Test: `screens/run/select_tests.rs`, `vm/selection.rs` and `vm/anchor.rs` (tests modules)

**Reuse:** none — new code (rung 7) for `vm/selection.rs`, `vm/anchor.rs`, `ui/markdown_view/select.rs` and
`screens/run/select.rs`, which are new on this branch and kept from the draft; rung 4: GPUI
`TextLayout::index_for_position`.

**Interfaces:**
- Consumes: `Selection`, `Anchor`, `Atom`.
- Produces:
  - `anchor::source_of(sel, entries, shown) -> String`, the source bytes in reading order;
  - `RunScreen::copy_markdown(&mut self, cx)`;
  - `SelectUnit { Char, Word, Line, Block }` from the click count.

**Depends-on:** none

- [ ] **Step 1:** Add failing tests:
  - `double_triple_and_quadruple_click_select_a_word_a_line_and_a_block`;
  - `shift_click_extends_the_selection`;
  - `copy_as_markdown_gives_the_source_with_its_math_delimiters`.
  Run: `cargo test -p reno-app select` — expect: fails.
- [ ] **Step 2:** Implement click units, shift-extend and `source_of`. Run: `cargo test -p reno-app` — expect: pass,
  including the kept selection tests for 2.1, 2.2, 2.4–2.8 and 2.10–2.13.
- [ ] **Step 3:** Commit `selection: word, line and block clicks, shift-click, and Copy as Markdown`.

_Requirements: TRANSCRIPT-2.1, TRANSCRIPT-2.2, TRANSCRIPT-2.3, TRANSCRIPT-2.4, TRANSCRIPT-2.5, TRANSCRIPT-2.6, TRANSCRIPT-2.7, TRANSCRIPT-2.8, TRANSCRIPT-2.9, TRANSCRIPT-2.10, TRANSCRIPT-2.11, TRANSCRIPT-2.12, TRANSCRIPT-2.13_

---

### Task 8: Save each thread's reading place and restore it, after a restart too

**Files:**
- Modify:
  - `panel/mod.rs` (`Descriptors`, `PanelState::snapshot` near :200; the test struct literals near :361, :550, :556, :576,
    :590);
  - `panel/persist.rs` (the drop-empty rule near :174; the test literal near :360);
  - `screens/run/panel.rs` (`persist_panel` near :128);
  - `screens/run/terminal.rs` (`Terms::restore` and `to_saved` near :131 and :153);
  - `screens/run.rs` (`resolve_panel_key` near :906; the scroll handler);
  - `screens/run/terminal_tests.rs` (the struct literal near :839).
- Test: `screens/run/persist_tests.rs`, `screens/run/panel_tests.rs`, `panel/persist.rs` (tests module)

**Reuse:** rung 2 — `panel/persist.rs` (`PanelStore`; `Saver` with `SAVE_DELAY` 750 ms and `flush` at :334, which quit and
window close call through `Saved::flush`, `panel/saved.rs:58` and `app.rs:401`), `panel_key_for`, `migrated` and
`resolve_panel_key` (`run.rs:906-920`, the run-to-thread carry-over), and the projection's `item_of`.

**Interfaces:**
- Consumes: `Projection::item_of` (`vm/items.rs`).
- Produces:
  - `struct Place { key: u64, offset_pt: i32 }`;
  - `Descriptors.place: Option<Place>` (`#[serde(default)]`);
  - `RunScreen::{saved_place, restore_place}`;
  - `Place::resolve(&Projection, &[Row]) -> ListOffset`.

**Depends-on:** Task 3

- [ ] **Step 1:** Add failing tests:
  - `a_file_from_before_reads_with_no_place_and_version_stays_one`;
  - `a_key_with_only_a_place_is_kept`;
  - `the_place_survives_snapshot_persist_and_the_terminal_round_trip`;
  - `returning_to_a_thread_shows_the_saved_place_after_a_restart`;
  - `a_gone_or_folded_anchor_restores_to_its_fold_head_else_the_next_entry`;
  - `the_place_moves_with_the_run_to_thread_key`;
  - `a_view_at_the_tail_saves_no_place_and_reopens_following`.
  Run: `cargo test -p reno-app place` — expect: fails to compile, with `Place` undefined.
- [ ] **Step 2:** Add `Place` and the field; carry it through every field-by-field builder and `resolve_panel_key`. Add
  `place: None` to the test struct literals; that is a compile-only edit, with no assertion changed.
- [ ] **Step 3:** Mark the place changed on each scroll event, let `Saver` coalesce, and restore after the first layout.
  Run: `cargo test -p reno-app` — expect: pass.
- [ ] **Step 4:** Commit `transcript: each thread's reading place is saved with its panel and restored after a restart`.

_Requirements: TRANSCRIPT-4.1, TRANSCRIPT-4.2, TRANSCRIPT-4.3, TRANSCRIPT-4.4, TRANSCRIPT-4.5_

---

### Task 9: Page through the transcript by keyboard

**Files:**
- Modify: `actions.rs`, `screens/run/select.rs` (`bind_keys`), `screens/run.rs`
- Test: `screens/run/convo_tests.rs`

**Reuse:** rung 2 — the `Transcript` key context and focus node (`run/select.rs:27`), the tick rail (`run.rs` `tick_rail`,
`convo::tick_marks`/`row_starts`), `kit::icon_button`, the kept `jump_control`; rung 4 — GPUI list `scroll_by`/`scroll_to`
and its scrollbar hooks (`set_offset_from_scrollbar`, `max_offset_for_scrollbar`, `scrollbar_drag_started`); new (rung 7) —
`ui/scrollbar.rs`, a scrollbar element over those hooks, because neither GPUI's `list()` nor the app has one.

**Interfaces:**
- Consumes: `CONTEXT = "Transcript"` (`run/select.rs`).
- Produces: actions `PageUp`, `PageDown`, `LineUp`, `LineDown`, `Home`, `End`, `PrevPrompt` and `NextPrompt`, bound in
  `Transcript`; `RunScreen::scroll_page(dir: i8)`.

**Depends-on:** Task 3

- [ ] **Step 1:** Add failing tests:
  - `page_line_home_and_end_keys_scroll_the_transcript`;
  - `shift_page_keys_go_to_the_previous_and_next_prompt`.
  Run: `cargo test -p reno-app keys_scroll` — expect: fails.
- [ ] **Step 2:** Bind the actions. A page is 90 % of the viewport and a line step is three lines.
  Run: `cargo test -p reno-app` — expect: pass, with the composer's keys unchanged.
- [ ] **Step 3:** Commit `transcript: page, line, home, end and prompt-to-prompt keys`.

_Requirements: TRANSCRIPT-5.1, TRANSCRIPT-5.2_

---

### Task 10: A scrollbar beside the tick rail

**Files:**
- Create: `ui/scrollbar.rs`
- Modify: `ui/mod.rs`, `screens/run.rs`
- Test: `ui/scrollbar.rs` (tests module), `screens/run/convo_tests.rs`

**Reuse:** rung 2 — the `Transcript` key context and focus node (`run/select.rs:27`), the tick rail (`run.rs` `tick_rail`,
`convo::tick_marks`/`row_starts`), `kit::icon_button`, the kept `jump_control`; rung 4 — GPUI list `scroll_by`/`scroll_to`
and its scrollbar hooks (`set_offset_from_scrollbar`, `max_offset_for_scrollbar`, `scrollbar_drag_started`); new (rung 7) —
`ui/scrollbar.rs`, a scrollbar element over those hooks, because neither GPUI's `list()` nor the app has one.

**Interfaces:**
- Consumes: `ListState`.
- Produces: `scrollbar(list: &ListState, theme: &Theme) -> impl IntoElement` with a `transcript-scrollbar` debug
  selector; `thumb_geometry(viewport, content, offset) -> (top, height)`.

**Depends-on:** Task 3

- [ ] **Step 1:** Add failing tests:
  - `thumb_geometry_tracks_the_offset_and_the_content`;
  - `a_tall_transcript_shows_a_scrollbar_beside_the_rail_and_a_drag_scrolls_it`.
  Run: `cargo test -p reno-app scrollbar` — expect: fails to compile.
- [ ] **Step 2:** Implement the element over the list's scrollbar hooks, using the existing tokens: thumb `raised`, hover
  `faint`. Run: `cargo test -p reno-app` — expect: pass.
- [ ] **Step 3:** Commit `ui: a scrollbar for the transcript list`.

_Requirements: TRANSCRIPT-5.3_

---

### Task 11: Turn-end actions: Scroll to user message, and Scroll to top on the last turn

**Files:**
- Modify: `screens/run/transcript.rs`, `screens/run.rs`
- Test: `screens/run/convo_tests.rs`

**Reuse:** rung 2 — the `Transcript` key context and focus node (`run/select.rs:27`), the tick rail (`run.rs` `tick_rail`,
`convo::tick_marks`/`row_starts`), `kit::icon_button`, the kept `jump_control`; rung 4 — GPUI list `scroll_by`/`scroll_to`
and its scrollbar hooks (`set_offset_from_scrollbar`, `max_offset_for_scrollbar`, `scrollbar_drag_started`); new (rung 7) —
`ui/scrollbar.rs`, a scrollbar element over those hooks, because neither GPUI's `list()` nor the app has one.

**Interfaces:**
- Consumes: `Projection::item_of_top`.
- Produces: debug selectors `turn-to-prompt-<key>` and `turn-to-top`.

**Depends-on:** Task 3

- [ ] **Step 1:** Add the failing test `a_finished_turn_offers_scroll_to_its_prompt_and_the_last_turn_scroll_to_top`.
  Run: `cargo test -p reno-app turn_offers` — expect: fails.
- [ ] **Step 2:** Implement the hover actions in `muted`, using the kit's icon buttons. Run: `cargo test -p reno-app` —
  expect: pass, including the tick-rail lock tests (5.5).
- [ ] **Step 3:** Commit `transcript: scroll to a turn's prompt, and to the top from the last turn`.

_Requirements: TRANSCRIPT-5.4, TRANSCRIPT-5.5_

---

### Task 12: Every diagram type, Preview and Code tabs, and the previous drawing kept while redrawing

**Files:**
- Modify: `Cargo.toml` (workspace `merman` features `diagram-class`, `diagram-state`, `diagram-er`, `diagram-gantt`,
  `diagram-pie`), `ui/markdown_view/mermaid.rs`, `ui/markdown_view/chrome.rs`, `ui/markdown_view.rs`
- Test: `ui/markdown_view/mermaid.rs` (tests module), `renderers_tests.rs`

**Reuse:** rung 5 — `merman =0.11.0` (added, agent-decided; `cssparser` MPL-2.0 is allowed by deny.toml:108); the kept
`md/mermaid.rs` and its `no_files()` resolver (:282); `md/chrome.rs` frame.

**Interfaces:**
- Consumes: `mermaid::drawing`, `chrome::card`.
- Produces: `enum DiagramTab { Preview, Code }` per card key; `Drawing::Rendering { previous: Option<Rc<Raster>> }`.

**Depends-on:** none

- [ ] **Step 1:** Add failing tests:
  - `class_state_er_gantt_and_pie_each_draw` (table-driven, one row per type);
  - `a_changed_source_keeps_the_previous_drawing_until_the_new_one_lands`;
  - `the_code_tab_shows_the_source_and_preview_the_drawing`.
  Run: `cargo test -p reno-app mermaid` — expect: fails.
- [ ] **Step 2:** Enable the features and implement the tabs and the previous-drawing state. If a type cannot draw, stop and
  take it through `reroute-plan`. Run: `cargo test -p reno-app` — expect: pass, including the kept 10.3, 10.5 and 10.6
  tests.
- [ ] **Step 3:** Run `cargo deny check` and `cargo machete` — expect: pass. Commit
  `diagrams: class, state, ER, gantt and pie; Preview and Code tabs; keep the previous drawing`.

_Requirements: TRANSCRIPT-10.1, TRANSCRIPT-10.2, TRANSCRIPT-10.3, TRANSCRIPT-10.5, TRANSCRIPT-10.6_

---

### Task 13: Zoom a diagram with ⌘-scroll

**Files:**
- Modify: `ui/markdown_view/chrome.rs`, `ui/markdown_view/mermaid.rs`, `ui/markdown_view.rs`
- Test: `renderers_tests.rs`

**Reuse:** rung 5 — `merman =0.11.0` (added, agent-decided; `cssparser` MPL-2.0 is allowed by deny.toml:108); the kept
`md/mermaid.rs` and its `no_files()` resolver (:282); `md/chrome.rs` frame.

**Interfaces:**
- Consumes: the `DiagramTab` card state (Task 12).
- Produces:
  - `Zoom { factor: f32 }`, clamped between fit-to-width and 2.0;
  - a `diagram-zoom-reset` selector;
  - `RERASTER_AFTER: Duration = 300 ms` (agent-decided), an owned one-shot deadline.

**Depends-on:** Task 12

- [ ] **Step 1:** Add failing tests:
  - `cmd_scroll_zooms_between_fit_and_two_hundred_percent_and_reset_returns`;
  - `the_re_raster_fires_once_after_the_last_zoom_step`, on the test clock.
  Run: `cargo test -p reno-app zoom` — expect: fails.
- [ ] **Step 2:** Implement zoom, the readout, the reset, sideways scroll and the deadline re-raster.
  Run: `cargo test -p reno-app` — expect: pass.
- [ ] **Step 3:** Commit `diagrams: cmd-scroll zoom with a readout and reset`.

_Requirements: TRANSCRIPT-10.4_

---

### Task 14: Content the owner grows stops following the tail, diagram zoom included

**Files:**
- Modify: `screens/run.rs`
- Test: `screens/run/find_tests.rs`

**Reuse:** rung 2 — the `Transcript` key context and focus node (`run/select.rs:27`), the tick rail (`run.rs` `tick_rail`,
`convo::tick_marks`/`row_starts`), `kit::icon_button`, the kept `jump_control`; rung 4 — GPUI list `scroll_by`/`scroll_to`
and its scrollbar hooks (`set_offset_from_scrollbar`, `max_offset_for_scrollbar`, `scrollbar_drag_started`); new (rung 7) —
`ui/scrollbar.rs`, a scrollbar element over those hooks, because neither GPUI's `list()` nor the app has one.

**Interfaces:**
- Consumes: `Zoom` (Task 13), `jump_control`.
- Produces: `RunScreen::pause_following(&mut self, cx)`, called by every UI-grown change.

**Depends-on:** Task 13

- [ ] **Step 1:** Add the failing test `zooming_a_diagram_while_following_stops_following_and_shows_jump_to_latest`.
  Run: `cargo test -p reno-app zooming_a_diagram` — expect: fails.
- [ ] **Step 2:** Route fold, reasoning, output and zoom growth through `pause_following`. Run: `cargo test -p reno-app` —
  expect: pass, including the kept jump-to-latest tests (6.1).
- [ ] **Step 3:** Commit `transcript: any content the owner grows stops following the tail`.

_Requirements: TRANSCRIPT-6.1, TRANSCRIPT-6.2_

---

### Task 15: Confirm code-block Copy and wrap on the kept renderer

**Files:**
- Test: `renderers_tests.rs`, `ui/markdown_view/chrome.rs` (tests module)

**Reuse:** rung 2 — `ui/markdown_view.rs` (`Host`, `blocks_with`), `md/chrome.rs`, `md/link.rs` and `run/links.rs` (new on
this branch, kept); `ui/menu.rs` `popup(at: Point)` for the context menu, extended with ↑/↓/Enter/Esc keyboard navigation
(16.8); `links::open` and `open_file_at` (`run/files.rs:496`). Rung 5 — pulldown-cmark 0.12.2 options the parser does not
enable today (`crates/reno-markdown/src/parser.rs:143-147` sets only tables, strikethrough, tasklists and math):
`ENABLE_FOOTNOTES`, `ENABLE_YAML_STYLE_METADATA_BLOCKS` (front matter) and `ENABLE_GFM` (`BlockQuoteKind` for
`[!NOTE]`-style callouts).

**Interfaces:**
- Consumes: `chrome::card`.
- Produces: no new interface.

**Depends-on:** none

- [ ] **Step 1:** Add the test `wrap_toggles_long_lines_and_unwrapped_lines_scroll_sideways` beside the kept
  `a_code_block_has_copy_and_wrap_in_its_frame_and_no_header`. Run it and watch it fail once by breaking `toggle_wrap`,
  then restore.
  Run: `cargo test -p reno-app code_block` — expect: pass after restoring.
- [ ] **Step 2:** Commit `renderer: wrap is locked by a test`.

_Requirements: TRANSCRIPT-7.1, TRANSCRIPT-7.2_

---

### Task 16: Footnotes, metadata blocks and callout kinds in the parser; footnote links

**Files:**
- Modify: `crates/reno-markdown/src/parser.rs` (`options()` near :143), `ui/markdown_view.rs`, `vm/anchor.rs`
- Test: `crates/reno-markdown/src/parser.rs` (tests module), `renderers_tests.rs`, `screens/run/find_tests.rs`

**Reuse:** rung 2 — `ui/markdown_view.rs` (`Host`, `blocks_with`), `md/chrome.rs`, `md/link.rs` and `run/links.rs` (new on
this branch, kept); `ui/menu.rs` `popup(at: Point)` for the context menu, extended with ↑/↓/Enter/Esc keyboard navigation
(16.8); `links::open` and `open_file_at` (`run/files.rs:496`). Rung 5 — pulldown-cmark 0.12.2 options the parser does not
enable today (`crates/reno-markdown/src/parser.rs:143-147` sets only tables, strikethrough, tasklists and math):
`ENABLE_FOOTNOTES`, `ENABLE_YAML_STYLE_METADATA_BLOCKS` (front matter) and `ENABLE_GFM` (`BlockQuoteKind` for
`[!NOTE]`-style callouts).

**Interfaces:**
- Consumes: `pulldown_cmark::Options::{ENABLE_FOOTNOTES, ENABLE_YAML_STYLE_METADATA_BLOCKS, ENABLE_GFM}`.
- Produces:
  - `Block::Footnote { label, blocks }`;
  - `Block::FrontMatter { pairs: Vec<(String, String)> }`;
  - `Block::BlockQuote { kind: Option<Callout>, .. }`, with `enum Callout { Note, Tip, Important, Warning, Caution }`;
  - `InlineStyle.footnote_ref: Option<String>`.

**Depends-on:** none

- [ ] **Step 1:** Add failing parser tests:
  - `footnotes_parse_as_references_and_definitions`;
  - `yaml_front_matter_parses_as_pairs`;
  - `gfm_callouts_carry_their_kind`;
  - `html_comments_are_dropped`.
  Run: `cargo test -p reno-markdown` — expect: the new tests fail; the 100 existing ones pass.
- [ ] **Step 2:** Enable the options and map their events. Run: `cargo test -p reno-markdown` — expect: all pass.
- [ ] **Step 3:** Add the failing GPUI test `clicking_a_footnote_reference_scrolls_to_its_note`. Implement it.
  Run: `cargo test -p reno-app` — expect: pass, including the kept 8.1, 8.2, 8.5 and 8.6 tests.
- [ ] **Step 4:** Commit `markdown: footnotes, front matter and callout kinds; a footnote reference scrolls to its note`.

_Requirements: TRANSCRIPT-8.1, TRANSCRIPT-8.2, TRANSCRIPT-8.4, TRANSCRIPT-8.5, TRANSCRIPT-8.6_

---

### Task 17: The transcript's context menu, reachable by keyboard

**Files:**
- Modify: `ui/menu.rs`, `screens/run.rs`, `screens/run/select.rs`, `actions.rs`
- Test: `screens/run/select_tests.rs`, `palette_tests.rs` (menu keyboard)

**Reuse:** rung 2 — `ui/markdown_view.rs` (`Host`, `blocks_with`), `md/chrome.rs`, `md/link.rs` and `run/links.rs` (new on
this branch, kept); `ui/menu.rs` `popup(at: Point)` for the context menu, extended with ↑/↓/Enter/Esc keyboard navigation
(16.8); `links::open` and `open_file_at` (`run/files.rs:496`). Rung 5 — pulldown-cmark 0.12.2 options the parser does not
enable today (`crates/reno-markdown/src/parser.rs:143-147` sets only tables, strikethrough, tasklists and math):
`ENABLE_FOOTNOTES`, `ENABLE_YAML_STYLE_METADATA_BLOCKS` (front matter) and `ENABLE_GFM` (`BlockQuoteKind` for
`[!NOTE]`-style callouts).

**Interfaces:**
- Consumes: `RunScreen::copy_markdown` (Task 7), `links::open`, `open_file_at`.
- Produces: `TranscriptMenu { link: Option<String>, has_selection: bool, reply: Option<Key> }`; the action `OpenMenu` on
  ⇧F10 in `Transcript`; `menu.rs` keyboard handling for ↑/↓/Enter/Esc.

**Depends-on:** Task 7

- [ ] **Step 1:** Add failing tests:
  - `right_click_offers_open_copy_link_copy_copy_as_markdown_copy_message_and_scroll_as_they_apply`;
  - `shift_f10_opens_it_and_arrows_and_enter_pick`.
  Run: `cargo test -p reno-app menu` — expect: fails.
- [ ] **Step 2:** Implement the menu and the keyboard handling. Run: `cargo test -p reno-app` — expect: pass, with every
  existing menu test unchanged.
- [ ] **Step 3:** Commit `transcript: a context menu with Open, Copy Link, Copy, Copy as Markdown and Copy Message`.

_Requirements: TRANSCRIPT-8.3_

---

### Task 18: Callouts, wide tables, footnotes, front matter, task boxes and hidden comments drawn

**Files:**
- Modify: `ui/markdown_view.rs`
- Test: `renderers_tests.rs`

**Reuse:** rung 2 — `ui/markdown_view.rs` (`Host`, `blocks_with`), `md/chrome.rs`, `md/link.rs` and `run/links.rs` (new on
this branch, kept); `ui/menu.rs` `popup(at: Point)` for the context menu, extended with ↑/↓/Enter/Esc keyboard navigation
(16.8); `links::open` and `open_file_at` (`run/files.rs:496`). Rung 5 — pulldown-cmark 0.12.2 options the parser does not
enable today (`crates/reno-markdown/src/parser.rs:143-147` sets only tables, strikethrough, tasklists and math):
`ENABLE_FOOTNOTES`, `ENABLE_YAML_STYLE_METADATA_BLOCKS` (front matter) and `ENABLE_GFM` (`BlockQuoteKind` for
`[!NOTE]`-style callouts).

**Interfaces:**
- Consumes: `Block::{Footnote, FrontMatter, BlockQuote { kind }}` (Task 16).
- Produces: selectors `md-callout-<kind>`, `md-table-scroll`, `md-footnotes`, `md-front-matter`, `md-task-box`.

**Depends-on:** Task 16, Task 1

- [ ] **Step 1:** Add failing tests on the `render2` reply:
  - `a_callout_draws_with_its_kind_label`;
  - `a_wide_table_scrolls_sideways`;
  - `footnotes_draw_at_the_end_and_comments_are_hidden`;
  - `front_matter_draws_as_pairs`;
  - `task_items_draw_a_box_that_does_not_toggle`.
  Run: `cargo test -p reno-app renderers` — expect: fails.
- [ ] **Step 2:** Implement the drawing with theme tokens: note `accent`, tip `success`, warning `warning`, caution
  `danger`. Run: `cargo test -p reno-app` — expect: pass, with the Files tab preview tests unchanged.
- [ ] **Step 3:** Commit `markdown: callouts, wide tables, footnotes, front matter and task boxes`.

_Requirements: TRANSCRIPT-14.1, TRANSCRIPT-14.2, TRANSCRIPT-14.3, TRANSCRIPT-14.4_

---

### Task 19: Confirm math and its settings on the kept renderer

**Files:**
- Test: `ui/markdown_view/math.rs` (tests module), `settings_pages_tests.rs`, `renderers_tests.rs`

**Reuse:** rung 5 — `ratex-parser`/`-layout`/`-types`/`-svg` `=0.1.16` (added by the branch, agent-decided under Delegated:
renderers; OFL notice THIRD_PARTY_NOTICES.md:9); `resvg`/`usvg` `=0.46.0` already linked through GPUI; rung 2 — `prefs.rs`
(`typesetMath`, `mathSize`, `math(cx)`) and `settings_pages.rs` rows (built).

**Interfaces:**
- Consumes: `md::math::typeset`, `Prefs::math`.
- Produces: no new interface.

**Depends-on:** none

- [ ] **Step 1:** Run `cargo test -p reno-app math` and `cargo test -p reno-app settings_pages` — expect: pass. These are the
  kept tests for display and inline math, the fallback to source, the size, and the two settings.
- [ ] **Step 2:** For any of 9.1–9.6 that the run shows no test for, add one. Watch it fail by breaking the guarded line,
  then restore. Commit `math: every math requirement held by a test`.

_Requirements: TRANSCRIPT-9.1, TRANSCRIPT-9.2, TRANSCRIPT-9.3, TRANSCRIPT-9.4, TRANSCRIPT-9.5, TRANSCRIPT-9.6_

---

### Task 20: Worktree SVG images, drawn by the app's own rasteriser

**Files:**
- Modify: `ui/markdown_view/image.rs`
- Test: `renderers_tests.rs`, `ui/markdown_view/image.rs` (tests module)

**Reuse:** rung 2 — kept `md/image.rs` (`ImageSource`, `classify`, `resolve`, an LRU of 64 / 48 MiB and an 8 MiB cap, both
agent-decided), the `fs.read_image` client call (`crates/reno-client/src/files.rs` `fs_read_image_all`), and the
`no_files()` rasteriser for SVG.

**Interfaces:**
- Consumes: `mermaid::no_files()`, `image::resolve`.
- Produces: `image::rasterize_svg(bytes: &[u8], scale: f32) -> Option<Raster>`.

**Depends-on:** Task 1

- [ ] **Step 1:** Add failing tests:
  - `a_worktree_svg_draws`, using `docs/flow.svg` from `render2`;
  - `an_svg_whose_image_href_names_a_file_outside_the_worktree_paints_nothing`.
  Run: `cargo test -p reno-app svg` — expect: the first fails ("Image not loaded").
- [ ] **Step 2:** Rasterise an SVG returned by `fs.read_image` through `usvg` with `no_files()`. Run:
  `cargo test -p reno-app` — expect: pass, including the kept 11.2–11.5 tests and the zero-send test.
- [ ] **Step 3:** Commit `images: worktree SVGs drawn by the app's own rasteriser, which follows no paths`.

_Requirements: TRANSCRIPT-11.1, TRANSCRIPT-11.2, TRANSCRIPT-11.3, TRANSCRIPT-11.4, TRANSCRIPT-11.5_

---

### Task 21: Images in tool results, on the mocked desk

**Files:**
- Modify: `crates/reno-client/src/visuals.rs`, `crates/reno-client/src/fake/visuals.rs`, `store.rs` (seam near :544),
  `vm/capabilities.rs`, `screens/run/output.rs`
- Test: `activity_tests.rs`, `crates/reno-client/tests/fake_transcript.rs`

**Reuse:** rung 2 — kept `crates/reno-client/src/visuals.rs` (`VisualDesk`), `fake/visuals.rs`, `Store::run_visuals`
(`store.rs:544`), `Capabilities.agent_visuals` and `md/visual.rs`; the ActivityDesk pattern.

**Interfaces:**
- Consumes: `VisualImage`, `image::resolve`.
- Produces: `VisualDesk::tool_images(&RunId, call: &str) -> Vec<VisualImage>`; `Store::tool_images`; `Capabilities.tool_images`
  (`REAL` false, `FAKE` true).

**Depends-on:** Task 1

- [ ] **Step 1:** Add failing tests:
  - `a_tool_results_image_shows_under_its_call_at_most_384_square_with_go_to_file`;
  - `on_the_real_capabilities_no_tool_image_or_placeholder_shows`.
  Run: `cargo test -p reno-app tool_image` — expect: fails.
- [ ] **Step 2:** Implement the desk method, the fake seed, the capability and the drawing. Run: `cargo test -p reno-app`
  and `cargo test -p reno-client --features fake` — expect: pass, including the kept 12.1, 12.3, 12.4 and 12.5 tests and
  `protocol_schema_matches_snapshot`.
- [ ] **Step 3:** Commit `visuals: tool-result images on the mocked desk`.

_Requirements: TRANSCRIPT-12.1, TRANSCRIPT-12.2, TRANSCRIPT-12.3, TRANSCRIPT-12.4, TRANSCRIPT-12.5_

---

### Task 22: Reveal streamed text in 16 ms steps

**Files:**
- Modify: `screens/run.rs`, `screens/run/transcript.rs`
- Test: `tests.rs`

**Reuse:** rung 2:
- `screens/run/transcript.rs` (`reasoning`, `worked`, the Thinking line), `run/output.rs` (`step_row`, output block,
  Copy), the `ui/motion.rs` pulse discipline and `cx.reduce_motion()`;
- `convo::step_lines` and `line_calls`, the diff line runs (`ui/markdown_view.rs:569` `line_runs`, as the Changes tab
  uses), and `prefs.rs` plus `settings_pages.rs` for the two settings rows.

**Interfaces:**
- Consumes: `TurnBuilder` changes (Task 3).
- Produces: `StreamReveal { shown: usize }`; `RunScreen::reveal_tick(&mut self, cx)`, a 16 ms tick that exists only while
  shown text is behind received text.

**Depends-on:** Task 3

- [ ] **Step 1:** Add failing tests:
  - `streamed_text_catches_up_within_200_ms_and_the_tick_stops`, on the test clock;
  - `with_reduce_motion_text_shows_at_once`;
  - `an_idle_screen_arms_no_reveal_tick`.
  Run: `cargo test -p reno-app reveal` — expect: fails.
- [ ] **Step 2:** Implement the bounded tick, and stop it when caught up. Run: `cargo test -p reno-app` — expect: pass.
- [ ] **Step 3:** Commit `transcript: streamed text is revealed smoothly and the tick stops when it catches up`.

_Requirements: TRANSCRIPT-13.1, TRANSCRIPT-13.2_

---

### Task 23: New edits and commands open in their group, with settings to turn that off

**Files:**
- Modify: `screens/run/output.rs`, `screens/run/transcript.rs`, `prefs.rs`, `screens/settings_pages.rs`
- Test: `screens/run/changes_tests.rs`, `settings_pages_tests.rs`

**Reuse:** rung 2:
- `screens/run/transcript.rs` (`reasoning`, `worked`, the Thinking line), `run/output.rs` (`step_row`, output block,
  Copy), the `ui/motion.rs` pulse discipline and `cx.reduce_motion()`;
- `convo::step_lines` and `line_calls`, the diff line runs (`ui/markdown_view.rs:569` `line_runs`, as the Changes tab
  uses), and `prefs.rs` plus `settings_pages.rs` for the two settings rows.

**Interfaces:**
- Consumes: `Opens<String>` (`outputs_open`).
- Produces: `Prefs::{expand_new_edits, expand_new_commands}`, app-settings keys `expandNewEdits` and `expandNewCommands`
  (default true).

**Depends-on:** Task 5

- [ ] **Step 1:** Add failing tests:
  - `a_new_edit_or_command_opens_inside_its_group_while_live`;
  - `turning_the_settings_off_keeps_them_collapsed`;
  - `the_expand_keys_round_trip_and_a_missing_key_is_on`.
  Run: `cargo test -p reno-app expand` — expect: fails.
- [ ] **Step 2:** Implement the defaults and the two Settings › General switches. Run: `cargo test -p reno-app` — expect:
  pass, including the tool-group guard tests.
- [ ] **Step 3:** Commit `tools: new edits and commands open in their group; settings to keep them closed`.

_Requirements: TRANSCRIPT-13.4, TRANSCRIPT-13.18_

---

### Task 24: Edit, command and read-file cards

**Files:**
- Modify: `screens/run/output.rs`, `vm/convo.rs`
- Test: `screens/run/changes_tests.rs`

**Reuse:** rung 2:
- `screens/run/transcript.rs` (`reasoning`, `worked`, the Thinking line), `run/output.rs` (`step_row`, output block,
  Copy), the `ui/motion.rs` pulse discipline and `cx.reduce_motion()`;
- `convo::step_lines` and `line_calls`, the diff line runs (`ui/markdown_view.rs:569` `line_runs`, as the Changes tab
  uses), and `prefs.rs` plus `settings_pages.rs` for the two settings rows.

**Interfaces:**
- Consumes: `markdown_view::line_runs`, `open_file_at`.
- Produces: selectors `edit-badge-<call>`, `edit-diff-<call>`, `patch-file-<call>-<n>`, `command-copy-<call>`,
  `read-numbered-<call>`, `raw-input-<call>`.

**Depends-on:** Task 23, Task 1

- [ ] **Step 1:** Add failing tests on `render2`:
  - `a_one_file_edit_shows_its_badge_go_to_file_and_inline_diff`;
  - `a_patch_shows_one_hunk_block_per_file`;
  - `a_command_shows_its_copy_elapsed_time_and_exit_status`;
  - `a_numbered_read_shows_as_a_code_block_and_raw_input_opens`.
  Run: `cargo test -p reno-app changes_tests` — expect: fails.
- [ ] **Step 2:** Implement the cards with kit components. Run: `cargo test -p reno-app` — expect: pass.
- [ ] **Step 3:** Commit `tools: edit, patch, command and read-file cards`.

_Requirements: TRANSCRIPT-13.5, TRANSCRIPT-13.6, TRANSCRIPT-13.7_

---

### Task 25: Error callout, hidden empty rows, resource links, and the guarded rows kept

**Files:**
- Modify: `screens/run/transcript.rs`, `screens/run/output.rs`
- Test: `screens/run/convo_tests.rs`, `activity_tests.rs`, `tests.rs`

**Reuse:** rung 2:
- `screens/run/transcript.rs` (`reasoning`, `worked`, the Thinking line), `run/output.rs` (`step_row`, output block,
  Copy), the `ui/motion.rs` pulse discipline and `cx.reduce_motion()`;
- `convo::step_lines` and `line_calls`, the diff line runs (`ui/markdown_view.rs:569` `line_runs`, as the Changes tab
  uses), and `prefs.rs` plus `settings_pages.rs` for the two settings rows.

**Interfaces:**
- Consumes: `open_file_at`.
- Produces: selectors `error-callout-<key>`, `error-copy-<key>`, `resource-link-<call>`.

**Depends-on:** Task 1

- [ ] **Step 1:** Add failing tests:
  - `an_error_turn_shows_a_callout_with_copy_error`;
  - `an_empty_reply_and_a_call_canceled_before_output_are_hidden`;
  - `a_resource_link_in_a_tool_result_opens_the_file`.
  Run: `cargo test -p reno-app convo_tests` — expect: fails.
- [ ] **Step 2:** Implement them. Run: `cargo test -p reno-app` — expect: pass, including the guard tests for the Thinking
  line, the inline approval line, the tool output block, the subagent chips, the reparse-from-the-last-block parser test
  and the tool grouping (13.10–13.15).
- [ ] **Step 3:** Commit `transcript: error callouts, hidden empty rows and resource links`.

_Requirements: TRANSCRIPT-13.8, TRANSCRIPT-13.9, TRANSCRIPT-13.10, TRANSCRIPT-13.11, TRANSCRIPT-13.12, TRANSCRIPT-13.13, TRANSCRIPT-13.14, TRANSCRIPT-13.15, TRANSCRIPT-13.16_

---

### Task 26: Every new control is reachable by keyboard and themed in both appearances

**Files:**
- Test: `screens/run/find_tests.rs`, `screens/run/select_tests.rs`, `renderers_tests.rs`

**Reuse:** rung 2 — `md/lru.rs` `Lru<K, V>` for every cache; `links::open` and its test guard; GPUI's `HttpClient` seam for
the zero-send test; `protocol_schema_matches_snapshot`; the persist and prefs tests; the kit focus ring; `cargo deny` and
`cargo machete`.

**Interfaces:**
- Consumes: the controls from Tasks 6, 9, 10, 13, 15 and 17.
- Produces: no new interface.

**Depends-on:** Task 6, Task 9, Task 10, Task 13, Task 17

- [ ] **Step 1:** Add the failing tests:
  - `every_new_control_is_reachable_and_operable_by_keyboard`: the find strip and its toggles, jump to latest, Copy and
    wrap, the diagram tabs and zoom reset, and the menu;
  - `highlights_callouts_and_diagrams_draw_from_theme_tokens_in_light_and_dark`: it asserts the token roles used, not
    pixel values.
  Run: `cargo test -p reno-app keyboard` — expect: any control without a focus path fails.
- [ ] **Step 2:** Give each failing control its focus path and label. Run: `cargo test -p reno-app` — expect: pass.
- [ ] **Step 3:** Commit `a11y: every new transcript control works from the keyboard`.

_Requirements: TRANSCRIPT-16.8, TRANSCRIPT-16.9_

---

### Task 27: The contracts and the safety checks hold at the end

**Files:**
- Test: `renderers_tests.rs`, `crates/reno-proto/tests/schema.rs`, `screens/run/persist_tests.rs`, `settings_pages_tests.rs`

**Reuse:** rung 2 — `md/lru.rs` `Lru<K, V>` for every cache; `links::open` and its test guard; GPUI's `HttpClient` seam for
the zero-send test; `protocol_schema_matches_snapshot`; the persist and prefs tests; the kit focus ring; `cargo deny` and
`cargo machete`.

**Interfaces:**
- Consumes: none.
- Produces: no new interface.

**Depends-on:** Task 8, Task 12, Task 20, Task 21, Task 23

- [ ] **Step 1:** Run these, each expected to pass:
  - `cargo test -p reno-proto protocol_schema` (16.10);
  - the zero-send test and the link-opener guard tests (16.5, 16.6);
  - the outside-the-worktree image test (16.7);
  - the persist and prefs round-trip tests, with a file from before this feature (16.11);
  - `cargo test -p reno-app settings_pages` (16.13).
- [ ] **Step 2:** Run `cargo deny check` and `cargo machete`, and confirm every new crate is pinned with `=` in the
  workspace manifest (16.12). Expect: pass.
- [ ] **Step 3:** Commit `checks: the contracts, the zero-network rule and the pinned dependencies hold`, if any test file
  changed; else record the results in the build ledger.

_Requirements: TRANSCRIPT-16.5, TRANSCRIPT-16.6, TRANSCRIPT-16.7, TRANSCRIPT-16.10, TRANSCRIPT-16.11, TRANSCRIPT-16.12, TRANSCRIPT-16.13_

---

### Task 28: The bench: one very long reply, the frame budget, and find while streaming

**Files:**
- Modify: `apps/renod/src/executors.rs` (`StreamMock` near :152), `apps/renod/src/main.rs` (`parse_stream_mock` near :30),
  `apps/renod/src/daemon.rs` (near :75), `xtask/src/app_bench.rs`, `xtask/src/app_stats.rs` (the row near :86; tests near
  :399, :724 and :731), `docs/BENCHMARKS.md`
- Test: `apps/renod/src/executors.rs` (tests module), `xtask/src/app_stats.rs` (tests module)

**Reuse:** rung 2 — `xtask/src/app_bench.rs` (`long_transcript`), `xtask/src/app_stats.rs` (the budget table, `fails` :349),
`apps/renod/src/executors.rs` `StreamMock`, `apps/reno-app/src/bench.rs` (`ScrollPlan`, `RENO_BENCH_SCROLL`).

**Interfaces:**
- Consumes: `renod --stream-mock`.
- Produces:
  - `StreamMock::LongReply` (word `longreply`);
  - the bench scenarios `long_reply` and `find_streaming`. The second streams the single-run 2,000-message burst while
    `RENO_BENCH_FIND=supervisor.rs` holds a query in the strip.
  - `long_frame_p95_ms` as a budget (`target: None`).

**Depends-on:** Task 3, Task 4, Task 6, Task 22

- [ ] **Step 1:** Add failing tests:
  - `the_long_reply_mock_streams_one_reply_with_code_and_math`, in renod;
  - `the_long_frame_row_fails_the_run_when_over_16_7_ms`, in xtask.
  Run: `cargo test -p renod long_reply` and `cargo test -p xtask long_frame` — expect: both fail.
- [ ] **Step 2:** Implement the mock, the two scenarios and the budget row. Rewrite BENCHMARKS.md's "a target, not a budget"
  line. Run both commands — expect: pass. The existing `Steady` and `Burst` tests stay unchanged (1.11).
- [ ] **Step 3:** Manual acceptance on a quiet Mac. Run `cargo xtask bench --app` — expect the long transcript, the long
  reply and find-while-streaming at p95 ≤ 16.7 ms each, the peak footprint ≤ 370 MiB, and the streaming, idle and cold-start
  budgets met. A miss on find-while-streaming moves the recount to an owned background task through `reroute-plan`; a miss
  on 16.1 from tree reparsing raises the LRU bound or parses off-frame.
- [ ] **Step 4:** Commit `bench: one very long reply, find while streaming, and the long-transcript frame budget`.

_Requirements: TRANSCRIPT-16.1, TRANSCRIPT-16.2, TRANSCRIPT-16.3_

---

### Task 29: Show the additions to the locked screen at a UI stop before landing

**Files:**
- Test: none (manual: the operator's review in the running app)

**Reuse:** rung 2 — the `draft-ui` stop as the charter runs it (`RUN/ui-pick.md`; window capture by id). The draft switch is
gone, so the app runs on `--fake` with no variant.

**Interfaces:**
- Consumes: the find toggles and error state (Task 6), the scrollbar (Task 10), the turn-end actions (Task 11).
- Produces: `RUN/ui-pick.md` rewritten for this stop; the operator's go, in `RUN/ui-reply.md`.

**Depends-on:** Task 6, Task 10, Task 11

- [ ] **Step 1:** Build release and launch `reno-app --fake` with `RENO_DATA_DIR` under a temp dir. Capture, by window id:
  the find strip with its toggles on and an invalid regex, the scrollbar on the handshake thread, and the turn-end
  actions. Open each capture.
- [ ] **Step 2:** Rewrite `RUN/ui-pick.md`: name the three additions, give their capture paths and the launch command, and
  leave the app running. Wait for the operator's go (relayed by the conductor). Apply any change and show it again.
- [ ] **Step 3:** On the go, update `./ui-brief.md`'s Find strip and Navigation surfaces and its Amendments to what the
  operator accepted, and commit `docs(specs): the ui brief takes the find toggles, the scrollbar and the turn-end actions`.

_Requirements: TRANSCRIPT-3.9, TRANSCRIPT-3.10, TRANSCRIPT-5.3, TRANSCRIPT-5.4_
