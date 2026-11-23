# Tasks: Reading a conversation (transcript)

> **For agentic workers:** after plan approval, pick one execute skill:
> - `build` (wave mode): a continuous, dependency-aware scheduler;
> - `build` (story mode): human-gated story review units;
> - `build` (inline mode): the controller implements, with no implementer subagents.
>
> The chosen skill writes `Execution-mode:`. Steps use checkbox (`- [ ]`) syntax for tracking.

Feature code: TRANSCRIPT
Status: Approved
Approved: 2026-11-22, the operator's word "approve" (relayed by the conductor; RUN operator-reply.md)
Date: 2026-11-22
Execution-mode: continuous
Max-concurrency: auto
Requirements: ./requirements.md
Design: ./design.md
UI brief: ./ui-brief.md (Locked)

**Goal:** Finish ROAD-33 on the kept draft code, so the owner reads, finds, selects and copies anything in a conversation of
any length without a dropped frame, with typeset math, diagrams, images and agent-posted visuals.

**Architecture:**
- The nested `Row` model stays what the lock tests read. A flat projection (`vm/items.rs`) drives the GPUI list, and an
  incremental `TurnBuilder` replaces the per-event rebuild.
- Find, selection, the reading place and the renderers are modules already kept on `feat/transcript`. The tasks finish
  what the design scan marks partial or missing, and re-run the tests that hold what the draft built.
- Tasks that share a file are chained by `Depends-on`, so a wave never runs two of them at once.

**Tech Stack:**
- Rust 2024 (`rust-version` 1.98), GPUI (pinned git rev 596dfdc).
- `pulldown-cmark` 0.12.2 (`reno-markdown`), `ratex-*` `=0.1.16`, `merman` `=0.11.0`, `resvg`/`usvg` `=0.46.0`, `regex`
  `=1.13.1`.
- Tests: `cargo test` with `#[gpui::test]` on the in-process fake daemon (`reno-client` feature `fake`).
- Bench: `cargo xtask bench --app` (macOS, manual).

## Global Constraints

Every task follows these sources; none is copied here.
- **Engineering standards:** `docs/standards/INDEX.md` (sha256 `c9022b57a31d…`) and `docs/standards/testing.md`
  (sha256 `bfd06e83b4ca…`): test-first; no network, no real harness, no wall clock; a test never reaches the owner's
  machine (a browser, the Keychain, `launchctl`, `~/.reno`); dependencies declared once in the workspace manifest;
  `cargo deny` and `cargo machete` pass.
- **Architecture spine:** `docs/architecture/INDEX.md` (sha256 `e052f5366f23…`). Every task inherits ARCH-2, ARCH-10,
  ARCH-11, ARCH-12, ARCH-13, ARCH-16 and ARCH-17, in that file's own words. The design cites them per section.
- **Verify commands:** `docs/agents/project.md` (sha256 of the file as committed): `cargo check --workspace --all-targets`,
  `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, plus the
  task's own test command.
  - Known pre-existing failure, not this plan's: workspace clippy reports `clippy::expect_used` in
    `xtask/tests/wiring_diagram.rs` (lines 24, 37, 41, …). It is verified, and that file is on the base before this
    branch (79f2c92). No task fixes or adds to it.
- **Run boundaries** (charter): never touch `dev.reno.renod`, `~/Library/Application Support/Reno`, `~/.reno`,
  `~/.local/bin`, `launchctl` or the Keychain. A `--fake` run with `RENO_DATA_DIR` points under a temp dir.
- **Names:** tracked files name no outside project; the reference is "the reference agent panel".
- **Tests from before this branch** stay green and unedited, with exactly these exceptions, each required by a
  requirement:
  - Task 8 adds `place: None` to the `Descriptors { .. }` struct literals (compile only, no assertion changed).
  - Tasks 5 and 23 add keys to the `prefs.rs` `KEYS` array and any test that lists it.
  - Task 28 rewrites `the_long_transcript_frame_time_is_a_target_that_does_not_fail_the_run` (`xtask/src/app_stats.rs`
    near :731) and the target-row assertions near :399. TRANSCRIPT-16.1 makes that row a budget; the test's meaning is
    replaced, not weakened.
- The lock tests named in `./ui-brief.md` stay green.
- **Team:** Solo (`docs/agents/project.md` `## Team`).

## File Structure

Paths are under `apps/reno-app/src/` unless they say otherwise.

| File | Responsibility | Tasks |
|---|---|---|
| `crates/reno-client/src/fake/transcript.rs` | Seeded threads; `pub fn seeded_run_events(run: &str) -> Vec<(Seq, ExecEvent)>` for tests | 1 |
| `crates/reno-client/src/fake/files.rs` | Fake tree serves `docs/flow.svg` and the patched files | 1 |
| `crates/reno-client/src/fake/visuals.rs` | Fake desk; tool images | 1, 21 |
| `crates/reno-client/src/visuals.rs` | `VisualDesk::tool_images` | 21 |
| `crates/reno-client/tests/fake_transcript.rs` | Fake scene tests | 1 |
| `transcript.rs` | `TurnBuilder` and its call index beside `build_rows` (Row and ToolEntry unchanged) | 3 |
| `vm/items.rs` | Projection; diff over the changed tail | 3 |
| `screens/run.rs` | RunScreen: refresh, caches, Thinking, place, keys, scrollbar, turn-end actions, follow, menu, reveal | 3, 4, 5, 8, 9, 10, 11, 14, 17, 22 |
| `ui/markdown_view/lru.rs` | `Lru::pin` / `unpin` | 4 |
| `vm/find.rs` | `Query`, `Matcher`, hit cap | 4, 6 |
| `screens/run/find.rs` | Strip toggles, recount, bounded hit cache | 6 |
| `screens/run/links.rs` | Bounded directory index | 4 |
| `vm/selection.rs`, `vm/anchor.rs` | Click units; `source_of` | 7, 16 |
| `screens/run/select.rs` | Click counts, shift-extend, key bindings, menu key | 7, 9, 17 |
| `ui/markdown_view/select.rs` | Painted-text registry, word and line bounds | 7 |
| `panel/mod.rs`, `panel/persist.rs`, `screens/run/panel.rs`, `screens/run/terminal.rs`, `screens/run/terminal_tests.rs` | `Descriptors.place` and its builders | 8 |
| `ui/scrollbar.rs` (new), `ui/mod.rs` | Scrollbar element | 10 |
| `actions.rs` | Find toggles, paging, menu key | 6, 9, 17 |
| `ui/menu.rs` | Popup keyboard navigation (`MenuState`) | 17 |
| `screens/run/transcript.rs` | Reasoning modes, turn-end actions, reveal, error callout, hidden rows | 5, 11, 22, 25 |
| `screens/run/output.rs` | Tool images, auto-expand, edit, patch, command, read-file, raw input, resource link | 14, 21, 23, 24, 25 |
| `vm/convo.rs` | Edit and command shapes from `ToolCall.input` | 24 |
| `vm/capabilities.rs`, `store.rs` | `tool_images` capability and seam; the watched-call timing map | 21, 24 |
| `prefs.rs`, `screens/settings_pages.rs` | Thinking mode; expand settings | 5, 23 |
| `ui/markdown_view.rs`, `ui/markdown_view/chrome.rs`, `ui/markdown_view/mermaid.rs` | Marks, diagram tabs, zoom, footnotes, callouts, tables, front matter, task boxes | 6, 12, 13, 16, 18 |
| `ui/markdown_view/image.rs` | Worktree SVG through `no_files()` | 20 |
| `crates/reno-markdown/src/parser.rs` | Footnotes, metadata blocks, GFM block-quote kinds | 16 |
| `Cargo.toml`, `apps/reno-app/Cargo.toml` | `regex =1.13.1`; merman features | 6, 12 |
| `apps/renod/src/executors.rs`, `apps/renod/src/main.rs`, `apps/renod/src/daemon.rs` | `StreamMock::LongReply` | 28 |
| `bench.rs`, `app.rs` | `RENO_BENCH_FIND`: hold a query in the strip during the bench | 28 |
| `xtask/src/app_bench.rs`, `xtask/src/app_stats.rs`, `docs/BENCHMARKS.md` | Scenarios, budget, results | 2, 28 |
| `./ui-brief.md` | Additions accepted at the UI stop | 29 |
| Test files: `screens/run/{find,select,convo,changes,panel,persist,terminal}_tests.rs`, `tests.rs`, `thread_tests.rs`, `activity_tests.rs`, `sidebar_tests.rs`, `renderers_tests.rs`, `settings_pages_tests.rs` | Tests at the agreed seams | all |

Retrieval (load-subgraph, 2026-11-22):
- `blast_radius` on every mapped path returns no codes; `cluster(TRANSCRIPT)` returns TRANSCRIPT alone. owns_coverage is
  **0 of 10**: no catalog card has a `tasks.md` yet, so the result is empty for lack of coverage.
- Surface-root neighbours: MOCKDESK (Tasks 1, 21), SETTINGS (Tasks 5, 23), APPBENCH (Tasks 2, 28), RUNACTIVITY (Task 25
  guard).

---

### Task 1: Seed the fake threads every later story needs

**Files:**
- Modify: `crates/reno-client/src/fake/transcript.rs`, `crates/reno-client/src/fake/files.rs`,
  `crates/reno-client/src/fake/visuals.rs`
- Test: `crates/reno-client/tests/fake_transcript.rs`

**Reuse:** rung 2 — kept `crates/reno-client/src/fake/transcript.rs` and `fake/{script,data,files,world/threads}.rs`;
`FakeConfig::transcript_threads`, which the app's `--fake` turns on (`store.rs:1002`).

**Interfaces:**
- Consumes: `FakeConfig::transcript_threads`.
- Produces:
  - `pub fn seeded_run_events(run: &str) -> Vec<(Seq, ExecEvent)>` (feature `fake`), so app tests can feed one run event by
    event;
  - the run `render2` ("Render check, part 2"), in `render1`'s thread.
- `render2` holds, with wire shapes from `crates/reno-proto/src/exec.rs`:
  - an error turn (a run ending failed, with a final message);
  - an Edit call whose `input` has `file_path`, `old_string` and `new_string`;
  - a patch call whose `input` has a two-file `patch`;
  - a Bash call with `input.command` and a `ToolResult { ok: false, summary }`;
  - a file read whose output lines are numbered, and whose `input` is shown as raw input;
  - a `ToolResult` whose output names the worktree path `src/supervisor.rs`;
  - a `Tool` with no `ToolResult` in a turn whose `Done` is `StopReason::Interrupted`: a call canceled before output;
  - a `Tool` with no `ToolResult` in a turn that ended `Completed`, which stays Pending;
  - a reply with a `[!NOTE]` callout, a table wider than the column, two footnotes, YAML front matter, task boxes and an
    HTML comment;
  - a markdown image `docs/flow.svg`, served by the fake tree.

**Depends-on:** none

- [x] **Step 1:** Write the failing test `the_second_render_run_has_every_remaining_kind_of_content`. It asserts that each
  item above is present in `seeded_run_events("render2")`.
  Run: `cargo test -p reno-client --features fake the_second_render_run` — expect: fails, with no run `render2`.
- [x] **Step 2:** Seed `render2` and `docs/flow.svg` behind `FakeConfig::transcript_threads`, and expose
  `seeded_run_events`. Run the test — expect: pass.
- [x] **Step 3:** Run `cargo test -p reno-client --features fake` and `cargo test -p reno-app` — expect: pass, with no
  existing test changed (15.4).
- [x] **Step 4:** Commit `fake: a second render run with the content the remaining stories draw`.

_Requirements: TRANSCRIPT-15.1, TRANSCRIPT-15.2, TRANSCRIPT-15.3, TRANSCRIPT-15.4_

---

### Task 2: Measure the long transcript on the real bench before the row work (U1)

**Files:**
- Modify: `docs/BENCHMARKS.md` (results only)

**Reuse:** rung 2 — `xtask/src/app_bench.rs` (`long_transcript`), `xtask/src/app_stats.rs` (the budget table, `fails` :349),
`apps/renod/src/executors.rs` `StreamMock`, `apps/reno-app/src/bench.rs` (`ScrollPlan`, `RENO_BENCH_SCROLL`).

**Interfaces:**
- Consumes: `cargo xtask bench --app`.
- Produces: the branch head's long-transcript p95, p50 and peak footprint, recorded beside the 31.4 ms baseline.

**Depends-on:** none. It gates no task; Task 3 (per-event building) does not rest on the U1 cause. Run it first anyway:
its number is the U1 evidence.

- [ ] **Step 1:** Manual acceptance on a quiet Mac, with the window in front. Run `cargo xtask bench --app` and record the
  long-transcript rows.
- [ ] **Step 2:** If p95 ≤ 16.7 ms, record U1 as resolved in `BENCHMARKS.md`: the bench meets the target with one item per
  entry. That resolves U1; it does not prove the cause, which stays unprofiled. Otherwise take the new cause through
  `reroute-plan`. Task 28's final bench is the gate either way.
- [ ] **Step 3:** Commit `bench: long transcript measured on the item-per-entry list (ROAD-33 U1)`.

_Requirements: TRANSCRIPT-1.1_

---

### Task 3: Build the latest run incrementally instead of rebuilding it per event

**Files:**
- Modify: `transcript.rs`, `screens/run.rs` (`refresh`; `rebuild_rows` near :627), `vm/items.rs`
- Test: `transcript.rs` (tests module), `screens/run/find_tests.rs`, `tests.rs`, `thread_tests.rs`

**Reuse:** none — new code (rung 7) for the projection `vm/items.rs`, which is new on this branch and kept from the draft;
it builds on rung 2 (`transcript.rs` `build_rows`, `fold_turns`, `Key`) and rung 4 (GPUI `ListState::remeasure_items`).

**Interfaces:**
- Consumes: `Projection`, `diff`, `Plan` (`vm/items.rs`); `seeded_run_events` (Task 1).
- Produces:
  - `TurnBuilder::new(segment: usize)`;
  - `push(&mut self, seq: Seq, event: &ExecEvent) -> Change`;
  - `rows(&self) -> &[Row]`;
  - `call(&self, id: &str) -> Option<CallInfo<'_>>`, with `CallInfo { input: &ToolCall, result_seq: Option<Seq> }`;
  - `enum Change { Grew(Key), Added(Key), Folded { turn: Range<usize>, fold: Key } }`.

**Depends-on:** Task 1

- [x] **Step 1:** Add the failing unit tests `a_turn_builder_matches_build_rows_event_by_event` (on
  `seeded_run_events("acp3")`), `a_done_folds_the_turn_as_one_change` and `the_call_index_finds_each_tools_input`.
  Run: `cargo test -p reno-app turn_builder` — expect: fails to compile, with `TurnBuilder` undefined.
- [x] **Step 2:** Implement `TurnBuilder` with `build_rows`' and `fold_turns`' row rules. Run the tests above — expect:
  pass.
- [x] **Step 3:** Add failing GPUI tests:
  - `a_streamed_delta_rebuilds_nothing_and_diffs_only_the_changed_tail`, which counts `rebuild_rows` calls and the items
    `diff` compares;
  - `a_rows_height_is_held_until_its_render_settles`.
  Run: `cargo test -p reno-app a_streamed_delta` — expect: fails.
- [x] **Step 4:** Route `refresh` through `TurnBuilder` for the latest run, keeping `older_builds` for older runs. Diff only
  from the first changed item. Fold a turn in one splice that keeps the anchor, and hold a measured height until its render
  settles. Run: `cargo test -p reno-app` — expect: pass, including the kept tests for 1.1 and 1.5–1.11.
- [x] **Step 5:** Commit `transcript: the latest run is built event by event, and a finished turn folds in one splice`.

_Requirements: TRANSCRIPT-1.1, TRANSCRIPT-1.2, TRANSCRIPT-1.3, TRANSCRIPT-1.4, TRANSCRIPT-1.5, TRANSCRIPT-1.6, TRANSCRIPT-1.7, TRANSCRIPT-1.8, TRANSCRIPT-1.9, TRANSCRIPT-1.10, TRANSCRIPT-1.11_

---

### Task 4: Bound every map the run screen grows

**Files:**
- Modify: `ui/markdown_view/lru.rs`, `screens/run.rs` (`markdown`, `probed`), `screens/run/links.rs` (`file_index`),
  `vm/find.rs` (hit cap)
- Test: `ui/markdown_view/lru.rs` (tests module), `screens/run/find_tests.rs`, `vm/find.rs` (tests module)

**Reuse:** rung 2 — `md/lru.rs` `Lru<K, V>` for every cache; `links::open` and its test guard; GPUI's `HttpClient` seam for
the zero-send test; `protocol_schema_matches_snapshot`; the persist and prefs tests; the kit focus ring; `cargo deny` and
`cargo machete`.

**Interfaces:**
- Consumes: `Lru::new(max_entries, max_cost)`, `get`, `insert`, `remove` (`ui/markdown_view/lru.rs:28`).
- Produces:
  - `Lru::pin(&mut self, key)` and `unpin`; a pinned entry is never evicted;
  - `RunScreen.markdown: Lru<Key, Streaming>` (512, agent-decided);
  - `probed` and `file_index` dirs as `Lru` (256 each, agent-decided);
  - `vm::find::HIT_CAP = 10_000`, with "10,000+" from `count_label`.

**Depends-on:** Task 3

- [x] **Step 1:** Add failing tests:
  - `a_pinned_entry_is_never_evicted`;
  - `the_parsed_trees_stop_at_their_bound_and_keep_the_live_reply`;
  - `the_directory_probes_stop_at_their_bound`;
  - `hits_stop_at_ten_thousand_and_the_count_says_so`.
  Run: `cargo test -p reno-app bound` — expect: fails.
- [x] **Step 2:** Implement `pin` and the three bounded maps, and cap `find_all`. Run: `cargo test -p reno-app` — expect:
  pass.
- [x] **Step 3:** Commit `transcript: bounded parse, probe and directory caches; find stops at 10,000 hits`.

_Requirements: TRANSCRIPT-16.4_

---

### Task 5: Reasoning shows open while it streams, with a Thinking setting

**Files:**
- Modify: `screens/run/transcript.rs` (`reasoning`), `screens/run.rs`, `prefs.rs`, `screens/settings_pages.rs`
- Test: `screens/run/convo_tests.rs`, `settings_pages_tests.rs`, `prefs.rs` (tests module)

**Reuse:** rung 2:
- `screens/run/transcript.rs` (`reasoning`, `worked`, the Thinking line), `run/output.rs` (`step_row`, output block,
  Copy), the `ui/motion.rs` pulse discipline and `cx.reduce_motion()`;
- `convo::step_lines` and `line_calls`, the diff line runs (`ui/markdown_view.rs:569` `line_runs`, as the Changes tab uses),
and `prefs.rs`
  plus `settings_pages.rs` for the two settings rows.

**Interfaces:**
- Consumes: `Opens<Key>` (`vm/find.rs`).
- Produces:
  - `enum ThinkingMode { Auto, Preview, Open, Closed }`, under app-settings key `thinking` (default `auto`);
  - `Prefs::thinking(cx) -> ThinkingMode`;
  - `RunScreen.streaming_open: Option<Key>`, a third open source held in neither `Opens` set.

**Depends-on:** Task 4

- [x] **Step 1:** Add failing tests:
  - `in_auto_reasoning_is_open_while_it_streams_and_closes_when_it_ends`;
  - `an_owner_toggle_while_streaming_keeps_the_owners_choice`;
  - `preview_caps_streaming_reasoning_at_sixteen_lines`;
  - `the_thinking_setting_round_trips_and_a_missing_key_is_auto`.
  Run: `cargo test -p reno-app thinking` — expect: fails.
- [x] **Step 2:** Implement the four modes and the Settings › General row. Add the key to `KEYS`. Run:
  `cargo test -p reno-app` — expect: pass.
- [x] **Step 3:** Commit `transcript: reasoning opens while it streams; Thinking setting in General`.

_Requirements: TRANSCRIPT-13.3, TRANSCRIPT-13.17_

---

### Task 6: Find with match case, whole word and regex, and a bounded recount

**Files:**
- Modify: `vm/find.rs`, `screens/run/find.rs`, `ui/markdown_view.rs` (`marks.needle` near :630), `actions.rs`,
  `Cargo.toml` (`regex = "=1.13.1"`), `apps/reno-app/Cargo.toml`
- Test: `vm/find.rs` (tests module), `screens/run/find_tests.rs`

**Reuse:** none — new code (rung 7) for `vm/find.rs`, `screens/run/find.rs` and `ui/find.rs`, new on this branch and kept
from the draft. Rung 2: the kit field `ui/field.rs`. Rung 5: `regex` 1.13.1, already in `Cargo.lock` as a transitive
dependency; the plan adds it as a direct workspace dependency pinned `=1.13.1`, for 3.9's regex mode.

**Interfaces:**
- Consumes:
  - `HIT_CAP` (Task 4);
  - `RunScreen.streaming_open` (Task 5);
  - `Segment.signature` (`screens/run.rs:82`).
- Produces:
  - `Query { text: String, case: bool, word: bool, regex: bool }`;
  - `Query::compile(&self) -> Result<Matcher, String>` (`RegexBuilder`, Unicode, case-insensitive unless `case`,
    `size_limit(1 << 20)`);
  - `find_all(rows, &Matcher) -> Vec<Hit>`;
  - `hit_cache: Lru<(usize, u64, Query), Vec<Hit>>` (64, agent-decided);
  - actions `ToggleMatchCase`, `ToggleWholeWord` and `ToggleRegex` (⌥⌘C, ⌥⌘W, ⌥⌘X).

**Depends-on:** Task 5

- [x] **Step 1:** Add failing unit tests in `vm/find.rs`:
  - `match_case_whole_word_and_regex_each_change_the_count`;
  - `a_regex_that_does_not_parse_or_is_too_large_says_why`;
  - `a_recount_keeps_the_current_hit_once_one_is_gone_to`.
  Run: `cargo test -p reno-app vm::find` — expect: fails to compile, with `Query` undefined.
- [x] **Step 2:** Implement `Query` and `Matcher`, and move `find_all`, `matches` and the prose marks to them. Run:
  `cargo test -p reno-app vm::find` — expect: pass.
- [x] **Step 3:** Add failing GPUI tests:
  - `toggles_and_their_keys_recount_and_an_invalid_regex_shows_no_count`;
  - `a_recount_while_streaming_rescans_only_the_live_run`;
  - `the_hit_cache_stops_at_its_bound`;
  - `closing_search_leaves_streaming_reasoning_open_and_a_stream_ending_keeps_search_opened_rows`.
  Run: `cargo test -p reno-app find_tests` — expect: fails.
- [x] **Step 4:** Implement the toggles in the kit's toggle style, the error state, the bounded hit cache, and the
  streaming-open rule. Run: `cargo test -p reno-app` — expect: pass, including the kept find tests.
- [x] **Step 5:** Run `cargo deny check` and `cargo machete` — expect: pass. Commit
  `find: match case, whole word and regex; the recount rescans only the live run`.

_Requirements: TRANSCRIPT-3.1, TRANSCRIPT-3.2, TRANSCRIPT-3.3, TRANSCRIPT-3.4, TRANSCRIPT-3.5, TRANSCRIPT-3.6, TRANSCRIPT-3.7, TRANSCRIPT-3.8, TRANSCRIPT-3.9, TRANSCRIPT-3.10, TRANSCRIPT-3.11, TRANSCRIPT-3.12, TRANSCRIPT-3.13_

---

### Task 7: Word, line and block clicks, shift-click, and Copy as Markdown

**Files:**
- Modify: `screens/run/select.rs` (`select_down`, near :237), `vm/selection.rs`, `vm/anchor.rs`,
  `ui/markdown_view/select.rs`
- Test: `screens/run/select_tests.rs`, `vm/selection.rs` and `vm/anchor.rs` (tests modules)

**Reuse:** none — new code (rung 7) for `vm/selection.rs`, `vm/anchor.rs`, `ui/markdown_view/select.rs` and
`screens/run/select.rs`, new on this branch and kept from the draft; rung 4: GPUI `TextLayout::index_for_position`.

**Interfaces:**
- Consumes: `Selection`, `Anchor`, `Atom`.
- Produces:
  - `anchor::source_of(sel, entries, shown) -> String`;
  - `RunScreen::copy_markdown(&mut self, cx)`;
  - `enum SelectUnit { Char, Word, Line, Block }`.

**Depends-on:** none

- [x] **Step 1:** Add failing tests:
  - `double_triple_and_quadruple_click_select_a_word_a_line_and_a_block`;
  - `shift_click_extends_the_selection`;
  - `copy_as_markdown_gives_the_source_with_its_math_delimiters`.
  Run: `cargo test -p reno-app select` — expect: fails.
- [x] **Step 2:** Implement click units, shift-extend and `source_of`. Run: `cargo test -p reno-app` — expect: pass,
  including the kept selection tests.
- [x] **Step 3:** Commit `selection: word, line and block clicks, shift-click, and Copy as Markdown`.

_Requirements: TRANSCRIPT-2.1, TRANSCRIPT-2.2, TRANSCRIPT-2.3, TRANSCRIPT-2.4, TRANSCRIPT-2.5, TRANSCRIPT-2.6, TRANSCRIPT-2.7, TRANSCRIPT-2.8, TRANSCRIPT-2.9, TRANSCRIPT-2.10, TRANSCRIPT-2.11, TRANSCRIPT-2.12, TRANSCRIPT-2.13_

---

### Task 8: Save each thread's reading place and restore it, after a restart too

**Files:**
- Modify:
  - `panel/mod.rs` (`Descriptors`; `PanelState::snapshot` near :200; test literals near :361, :550, :556, :576, :590);
  - `panel/persist.rs` (drop-empty rule near :174; test literal near :360);
  - `screens/run/panel.rs` (`persist_panel` near :128);
  - `screens/run/terminal.rs` (`Terms::restore` and `to_saved` near :131 and :153);
  - `screens/run.rs` (`resolve_panel_key` near :906; the scroll handler);
  - `screens/run/terminal_tests.rs` (test literal near :839).
- Test: `screens/run/persist_tests.rs`, `screens/run/panel_tests.rs`, `panel/persist.rs` (tests module)

**Reuse:** rung 2 — `panel/persist.rs` (`PanelStore`; `Saver` with `SAVE_DELAY` 750 ms and `flush` at :334, which quit and
window close call through `Saved::flush`, `panel/saved.rs:58` and `app.rs:401`), `panel_key_for`, `migrated` and
`resolve_panel_key` (`run.rs:906-920`, the run-to-thread carry-over), and the projection's `item_of`.

**Interfaces:**
- Consumes: `Projection::item_of`.
- Produces:
  - `struct Place { key: u64, offset_pt: i32 }`;
  - `Descriptors.place: Option<Place>`, with `#[serde(default)]`;
  - `RunScreen::{saved_place, restore_place}`;
  - `Place::resolve(&Projection, &[Row]) -> ListOffset`.

**Depends-on:** Task 5

- [x] **Step 1:** Add failing tests:
  - `a_file_from_before_reads_with_no_place_and_version_stays_one`;
  - `a_key_with_only_a_place_is_kept`;
  - `the_place_survives_snapshot_persist_and_the_terminal_round_trip`;
  - `returning_to_a_thread_shows_the_saved_place_after_a_restart`;
  - `a_gone_or_folded_anchor_restores_to_its_fold_head_else_the_next_entry`;
  - `the_place_moves_with_the_run_to_thread_key`;
  - `a_view_at_the_tail_saves_no_place_and_reopens_following`.
  Run: `cargo test -p reno-app place` — expect: fails to compile, with `Place` undefined.
- [x] **Step 2:** Add `Place` and the field; carry it through every builder and `resolve_panel_key`. Add `place: None` to
  the test literals (compile only).
- [x] **Step 3:** Mark the place changed on each scroll event, let `Saver` coalesce, and restore after the first layout.
  Run: `cargo test -p reno-app` — expect: pass.
- [x] **Step 4:** Commit `transcript: each thread's reading place is saved with its panel and restored after a restart`.

_Requirements: TRANSCRIPT-4.1, TRANSCRIPT-4.2, TRANSCRIPT-4.3, TRANSCRIPT-4.4, TRANSCRIPT-4.5_

---

### Task 9: Page through the transcript by keyboard

**Files:**
- Modify: `actions.rs`, `screens/run/select.rs` (`bind_keys`), `screens/run.rs`
- Test: `screens/run/convo_tests.rs`

**Reuse:** rung 2 — the `Transcript` key context and focus node (`run/select.rs:33`), the tick rail (`run.rs` `tick_rail`,
`convo::tick_marks`/`row_starts`), `kit::icon_button`, the kept `jump_control`; rung 4 — GPUI list `scroll_by`/`scroll_to`
and its scrollbar hooks (`set_offset_from_scrollbar`, `max_offset_for_scrollbar`, `scrollbar_drag_started`); new (rung 7) —
`ui/scrollbar.rs`, a scrollbar element over those hooks, because neither GPUI's `list()` nor the app has one.

**Interfaces:**
- Consumes: the private `CONTEXT = "Transcript"` (`run/select.rs:33`), reached through `bind_keys`.
- Produces: actions `PageUp`, `PageDown`, `LineUp`, `LineDown`, `Home`, `End`, `PrevPrompt` and `NextPrompt`;
  `RunScreen::scroll_page(dir: i8)`.

**Depends-on:** Task 6, Task 7, Task 8

- [x] **Step 1:** Add failing tests:
  - `page_line_home_and_end_keys_scroll_the_transcript`;
  - `shift_page_keys_go_to_the_previous_and_next_prompt`.
  Run: `cargo test -p reno-app keys_scroll` — expect: fails.
- [x] **Step 2:** Bind the actions. A page is 90 % of the viewport and a line step is three lines (TRANSCRIPT-5.1). Run:
  `cargo test -p reno-app` — expect: pass, with the composer's keys unchanged.
- [x] **Step 3:** Commit `transcript: page, line, home, end and prompt-to-prompt keys`.

_Requirements: TRANSCRIPT-5.1, TRANSCRIPT-5.2_

---

### Task 10: A scrollbar beside the tick rail

**Files:**
- Create: `ui/scrollbar.rs`
- Modify: `ui/mod.rs`, `screens/run.rs`
- Test: `ui/scrollbar.rs` (tests module), `screens/run/convo_tests.rs`

**Reuse:** rung 2 — the `Transcript` key context and focus node (`run/select.rs:33`), the tick rail (`run.rs` `tick_rail`,
`convo::tick_marks`/`row_starts`), `kit::icon_button`, the kept `jump_control`; rung 4 — GPUI list `scroll_by`/`scroll_to`
and its scrollbar hooks (`set_offset_from_scrollbar`, `max_offset_for_scrollbar`, `scrollbar_drag_started`); new (rung 7) —
`ui/scrollbar.rs`, a scrollbar element over those hooks, because neither GPUI's `list()` nor the app has one.

**Interfaces:**
- Consumes: `ListState` and its `set_offset_from_scrollbar`, `max_offset_for_scrollbar` and `scrollbar_drag_started`.
- Produces: `scrollbar(list: &ListState, theme: &Theme) -> impl IntoElement` (selector `transcript-scrollbar`);
  `thumb_geometry(viewport: f32, content: f32, offset: f32) -> (f32, f32)`.

**Depends-on:** Task 9

- [x] **Step 1:** Add failing tests:
  - `thumb_geometry_tracks_the_offset_and_the_content`;
  - `a_tall_transcript_shows_a_scrollbar_beside_the_rail_and_a_drag_scrolls_it`.
  Run: `cargo test -p reno-app scrollbar` — expect: fails to compile.
- [x] **Step 2:** Implement the element. The thumb is `raised`, `faint` on hover. Run: `cargo test -p reno-app` —
  expect: pass.
- [x] **Step 3:** Commit `ui: a scrollbar for the transcript list`.

_Requirements: TRANSCRIPT-5.3_

---

### Task 11: Turn-end actions: Scroll to user message, and Scroll to top on the last turn

**Files:**
- Modify: `screens/run/transcript.rs`, `screens/run.rs`
- Test: `screens/run/convo_tests.rs`

**Reuse:** rung 2 — the `Transcript` key context and focus node (`run/select.rs:33`), the tick rail (`run.rs` `tick_rail`,
`convo::tick_marks`/`row_starts`), `kit::icon_button`, the kept `jump_control`; rung 4 — GPUI list `scroll_by`/`scroll_to`
and its scrollbar hooks (`set_offset_from_scrollbar`, `max_offset_for_scrollbar`, `scrollbar_drag_started`); new (rung 7) —
`ui/scrollbar.rs`, a scrollbar element over those hooks, because neither GPUI's `list()` nor the app has one.

**Interfaces:**
- Consumes: `Projection::item_of_top`.
- Produces: selectors `turn-to-prompt-<key>` and `turn-to-top`.

**Depends-on:** Task 10

- [x] **Step 1:** Add the failing test `a_finished_turn_offers_scroll_to_its_prompt_and_the_last_turn_scroll_to_top`.
  Run: `cargo test -p reno-app turn_offers` — expect: fails.
- [x] **Step 2:** Implement the hover actions. Run: `cargo test -p reno-app` — expect: pass, including the tick-rail lock
  tests (5.5).
- [x] **Step 3:** Commit `transcript: scroll to a turn's prompt, and to the top from the last turn`.

_Requirements: TRANSCRIPT-5.4, TRANSCRIPT-5.5_

---

### Task 12: Every diagram type, Preview and Code tabs, and the previous drawing kept while redrawing

**Files:**
- Modify: `Cargo.toml` (`merman` features `diagram-class`, `diagram-state`, `diagram-er`, `diagram-gantt`, `diagram-pie`),
  `ui/markdown_view/mermaid.rs`, `ui/markdown_view/chrome.rs`, `ui/markdown_view.rs`
- Test: `ui/markdown_view/mermaid.rs` (tests module), `renderers_tests.rs`

**Reuse:** rung 5 — `merman =0.11.0` (added, agent-decided; `cssparser` MPL-2.0 is allowed by deny.toml:108); the kept
`md/mermaid.rs` and its `no_files()` resolver (:282); `md/chrome.rs` frame.

**Interfaces:**
- Consumes: `mermaid::drawing`, `chrome::card`.
- Produces: `enum DiagramTab { Preview, Code }`, per card key; `Drawing::Rendering { previous: Option<Rc<Raster>> }`.

**Depends-on:** Task 6

- [x] **Step 1:** Add failing tests:
  - `class_state_er_gantt_and_pie_each_draw`, table-driven with one row per type;
  - `a_changed_source_keeps_the_previous_drawing_until_the_new_one_lands`;
  - `the_code_tab_shows_the_source_and_preview_the_drawing`.
  Run: `cargo test -p reno-app mermaid` — expect: fails.
- [x] **Step 2:** Enable the features and implement the tabs and the previous-drawing state. A type that cannot draw goes
  through `reroute-plan`. Run: `cargo test -p reno-app` — expect: pass.
- [x] **Step 3:** Run `cargo deny check` and `cargo machete` — expect: pass. Commit
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
- Consumes: `DiagramTab` (Task 12).
- Produces: `Zoom { factor: f32 }`, clamped from fit-to-width to 2.0; selector `diagram-zoom-reset`;
  `RERASTER_AFTER = 300 ms` (agent-decided), an owned one-shot deadline.

**Depends-on:** Task 12

- [x] **Step 1:** Add failing tests:
  - `cmd_scroll_zooms_between_fit_and_two_hundred_percent_and_reset_returns`;
  - `the_re_raster_fires_once_after_the_last_zoom_step`, on the test clock.
  Run: `cargo test -p reno-app zoom` — expect: fails.
- [x] **Step 2:** Implement zoom, the readout, the reset, sideways scroll and the re-raster. Run: `cargo test -p reno-app`
  — expect: pass.
- [x] **Step 3:** Commit `diagrams: cmd-scroll zoom with a readout and reset`.

_Requirements: TRANSCRIPT-10.4_

---

### Task 14: Content the owner grows stops following the tail, diagram zoom included

**Files:**
- Modify: `screens/run.rs` (`toggle`, `keep_in_view`), `screens/run/output.rs` (output open and full)
- Test: `screens/run/find_tests.rs`

**Reuse:** rung 2 — the `Transcript` key context and focus node (`run/select.rs:33`), the tick rail (`run.rs` `tick_rail`,
`convo::tick_marks`/`row_starts`), `kit::icon_button`, the kept `jump_control`; rung 4 — GPUI list `scroll_by`/`scroll_to`
and its scrollbar hooks (`set_offset_from_scrollbar`, `max_offset_for_scrollbar`, `scrollbar_drag_started`); new (rung 7) —
`ui/scrollbar.rs`, a scrollbar element over those hooks, because neither GPUI's `list()` nor the app has one.

**Interfaces:**
- Consumes: `Zoom` (Task 13), `jump_control`.
- Produces: `RunScreen::pause_following(&mut self, cx)`, called by every UI-grown change.

**Depends-on:** Task 11, Task 13, Task 25

- [x] **Step 1:** Add the failing test `zooming_a_diagram_or_opening_an_output_while_following_stops_following`.
  Run: `cargo test -p reno-app stops_following` — expect: fails.
- [x] **Step 2:** Route fold, reasoning, output and zoom growth through `pause_following`. Run: `cargo test -p reno-app` —
  expect: pass, including the kept jump-to-latest tests.
- [x] **Step 3:** Commit `transcript: any content the owner grows stops following the tail`.

_Requirements: TRANSCRIPT-6.1, TRANSCRIPT-6.2_

---

### Task 15: Confirm code-block Copy and wrap on the kept renderer

**Files:**
- Test: `renderers_tests.rs`

**Reuse:** rung 2 — `ui/markdown_view.rs` (`Host`, `blocks_with`), `md/chrome.rs`, `md/link.rs` and `run/links.rs` (new on
this branch, kept); `ui/menu.rs` `popup(at: Point)` for the context menu, extended with ↑/↓/Enter/Esc keyboard navigation
(16.8); `links::open` and `open_file_at` (`run/files.rs:496`). Rung 5 — pulldown-cmark 0.12.2 options the parser does not
enable today (`crates/reno-markdown/src/parser.rs:143-147` sets only tables, strikethrough, tasklists and math):
`ENABLE_FOOTNOTES`, `ENABLE_YAML_STYLE_METADATA_BLOCKS` (front matter) and `ENABLE_GFM` (`BlockQuoteKind` for
`[!NOTE]`-style callouts).

**Interfaces:**
- Consumes: `chrome::card`, `chrome::toggle_wrap`.
- Produces: no new interface.

**Depends-on:** none

- [x] **Step 1:** Add the test `wrap_toggles_long_lines_and_unwrapped_lines_scroll_sideways` beside the kept
  `a_code_block_has_copy_and_wrap_in_its_frame_and_no_header`. Watch it fail once by breaking `toggle_wrap`, then restore.
  Run: `cargo test -p reno-app code_block` — expect: pass after restoring.
- [x] **Step 2:** Commit `renderer: wrap is locked by a test`.

_Requirements: TRANSCRIPT-7.1, TRANSCRIPT-7.2_

---

### Task 16: Footnotes from parser to click; metadata blocks and callout kinds parsed

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
- Consumes: `Options::{ENABLE_FOOTNOTES, ENABLE_YAML_STYLE_METADATA_BLOCKS, ENABLE_GFM}` (pulldown-cmark 0.12.2).
- Produces:
  - `Block::Footnote { label: String, blocks: Vec<Block> }`;
  - `Block::FrontMatter { pairs: Vec<(String, String)> }`;
  - `Block::BlockQuote { kind: Option<Callout>, .. }`, with `enum Callout { Note, Tip, Important, Warning, Caution }`;
  - `InlineStyle.footnote_ref: Option<String>`.

**Depends-on:** Task 1, Task 7, Task 13

- [x] **Step 1:** Add failing parser tests:
  - `footnotes_parse_as_references_and_definitions`;
  - `yaml_front_matter_parses_as_pairs`;
  - `gfm_callouts_carry_their_kind`;
  - `html_comments_are_dropped`.
  Run: `cargo test -p reno-markdown` — expect: the new tests fail; the 100 existing ones pass.
- [x] **Step 2:** Enable the options and map their events. Run: `cargo test -p reno-markdown` — expect: all pass.
- [x] **Step 3:** Add the failing GPUI test `footnotes_draw_at_the_reply_end_and_a_reference_click_scrolls_to_its_note`, on
  `render2`. Draw the footnote list and wire the click. Run: `cargo test -p reno-app` — expect: pass, including the kept
  8.1, 8.2, 8.5 and 8.6 tests.
- [x] **Step 4:** Commit `markdown: footnotes, front matter and callout kinds; a footnote reference scrolls to its note`.

_Requirements: TRANSCRIPT-8.1, TRANSCRIPT-8.2, TRANSCRIPT-8.4, TRANSCRIPT-8.5, TRANSCRIPT-8.6_

---

### Task 17: The transcript's context menu, reachable by keyboard

**Files:**
- Modify: `ui/menu.rs`, `screens/run.rs`, `screens/run/select.rs`, `actions.rs`
- Test: `screens/run/select_tests.rs`, `sidebar_tests.rs` (existing menu tests near :107, :161, :479 stay green)

**Reuse:** rung 2 — `ui/markdown_view.rs` (`Host`, `blocks_with`), `md/chrome.rs`, `md/link.rs` and `run/links.rs` (new on
this branch, kept); `ui/menu.rs` `popup(at: Point)` for the context menu, extended with ↑/↓/Enter/Esc keyboard navigation
(16.8); `links::open` and `open_file_at` (`run/files.rs:496`). Rung 5 — pulldown-cmark 0.12.2 options the parser does not
enable today (`crates/reno-markdown/src/parser.rs:143-147` sets only tables, strikethrough, tasklists and math):
`ENABLE_FOOTNOTES`, `ENABLE_YAML_STYLE_METADATA_BLOCKS` (front matter) and `ENABLE_GFM` (`BlockQuoteKind` for
`[!NOTE]`-style callouts).

**Interfaces:**
- Consumes: `RunScreen::copy_markdown` (Task 7), `links::open`, `open_file_at`, `menu::popup(at: Point)`.
- Produces:
  - `TranscriptMenu { link: Option<String>, has_selection: bool, reply: Option<Key> }`;
  - `menu::MenuState { highlighted: Option<usize> }`, for ↑/↓/Enter/Esc;
  - the action `OpenMenu` on ⇧F10 in `Transcript`.

**Depends-on:** Task 14, Task 16

- [x] **Step 1:** Add failing tests:
  - `right_click_offers_open_copy_link_copy_copy_as_markdown_copy_message_and_scroll_as_they_apply`;
  - `shift_f10_opens_it_and_arrows_and_enter_pick`.
  Run: `cargo test -p reno-app menu` — expect: fails.
- [x] **Step 2:** Implement the menu, and `MenuState` in `ui/menu.rs`. Run: `cargo test -p reno-app` — expect: pass,
  with the sidebar menu tests unchanged.
- [x] **Step 3:** Commit `transcript: a context menu with Open, Copy Link, Copy, Copy as Markdown and Copy Message`.

_Requirements: TRANSCRIPT-8.3_

---

### Task 18: Callouts, wide tables, front matter, task boxes and hidden comments drawn

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
- Consumes: `Block::{FrontMatter, BlockQuote { kind }}` (Task 16).
- Produces: selectors `md-callout-<kind>`, `md-table-scroll`, `md-front-matter`, `md-task-box`.

**Depends-on:** Task 16

- [x] **Step 1:** Add failing tests on the `render2` reply:
  - `a_callout_draws_with_its_kind_label`;
  - `a_wide_table_scrolls_sideways`;
  - `front_matter_draws_as_pairs_and_comments_are_hidden`;
  - `task_items_draw_a_box_that_does_not_toggle`.
  Run: `cargo test -p reno-app renderers` — expect: fails.
- [x] **Step 2:** Implement them with theme tokens: note `accent`, tip `success`, warning `warning`, caution `danger`. Run:
  `cargo test -p reno-app` — expect: pass, with the Files tab preview tests unchanged.
- [x] **Step 3:** Commit `markdown: callouts, wide tables, front matter and task boxes`.

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

- [x] **Step 1:** Run `cargo test -p reno-app math` and `cargo test -p reno-app settings_pages` — expect: pass.
- [x] **Step 2:** For any of 9.1–9.6 with no test shown in that run, add one. Watch it fail by breaking the guarded line,
  then restore. Commit `math: every math requirement held by a test`.

_Requirements: TRANSCRIPT-9.1, TRANSCRIPT-9.2, TRANSCRIPT-9.3, TRANSCRIPT-9.4, TRANSCRIPT-9.5, TRANSCRIPT-9.6_

---

### Task 20: Worktree SVG images, drawn by the app's own rasteriser

**Files:**
- Modify: `ui/markdown_view/image.rs`
- Test: `renderers_tests.rs`, `ui/markdown_view/image.rs` (tests module)

**Reuse:** rung 2 — kept `md/image.rs` (`ImageSource`, `classify`, `resolve`, an LRU of 64 / 48 MiB and an 8 MiB cap, both
agent-decided), the
`fs.read_image` client call (`crates/reno-client/src/files.rs` `fs_read_image_all`), and the `no_files()` rasteriser for
SVG.

**Interfaces:**
- Consumes: `mermaid::no_files()`, `image::resolve`.
- Produces: `image::rasterize_svg(bytes: &[u8], scale: f32) -> Option<Raster>`, used for **markdown images**: worktree
  SVGs (11.1) and `data:` SVGs (11.2, "the same way"). The branch-added test that pinned `data:` SVG as refused is
  rewritten to expect it drawn; it was added on this branch, not before it. A posted visual's SVG still shows "Visual not
  loaded", as the locked brief's Contract says (`ui-brief.md`, Contract 1).

**Depends-on:** Task 1

- [x] **Step 1:** Add failing tests:
  - `a_worktree_svg_draws` (`docs/flow.svg`);
  - `a_data_svg_draws_the_same_way`;
  - `an_svg_whose_image_href_names_a_file_outside_the_worktree_paints_nothing`;
  - `an_image_wrapped_in_a_link_follows_the_link`.
  Run: `cargo test -p reno-app svg` — expect: the first fails ("Image not loaded").
- [x] **Step 2:** Rasterise worktree SVGs from `fs.read_image`, and `data:` SVGs, through `usvg` with `no_files()`. Run:
  `cargo test -p reno-app` — expect: pass, including the kept 11.2–11.4 tests and the zero-send test.
- [x] **Step 3:** Commit `images: worktree SVGs drawn by the app's own rasteriser, which follows no paths`.

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
- Produces: `VisualDesk::tool_images(&RunId, call: &str) -> Vec<VisualImage>`; `Store::tool_images`;
  `Capabilities.tool_images` (`REAL` false, `FAKE` true).

**Depends-on:** Task 1

- [x] **Step 1:** Add failing tests:
  - `a_tool_results_image_shows_under_its_call_at_most_384_square_with_go_to_file`;
  - `on_the_real_capabilities_no_tool_image_or_placeholder_shows`.
  Run: `cargo test -p reno-app tool_image` — expect: fails.
- [x] **Step 2:** Implement the desk method, its fake seed, the capability and the drawing. Run: `cargo test -p reno-app`
  and `cargo test -p reno-client --features fake` — expect: pass, including the kept 12.1 and 12.3–12.5 tests and
  `protocol_schema_matches_snapshot`.
- [x] **Step 3:** Commit `visuals: tool-result images on the mocked desk`.

_Requirements: TRANSCRIPT-12.1, TRANSCRIPT-12.2, TRANSCRIPT-12.3, TRANSCRIPT-12.4, TRANSCRIPT-12.5_

---

### Task 22: Reveal streamed text in 16 ms steps

**Files:**
- Modify: `screens/run.rs`, `screens/run/transcript.rs`
- Test: `tests.rs`

**Reuse:** rung 2:
- `screens/run/transcript.rs` (`reasoning`, `worked`, the Thinking line), `run/output.rs` (`step_row`, output block,
  Copy), the `ui/motion.rs` pulse discipline and `cx.reduce_motion()`;
- `convo::step_lines` and `line_calls`, the diff line runs (`ui/markdown_view.rs:569` `line_runs`, as the Changes tab uses),
and `prefs.rs`
  plus `settings_pages.rs` for the two settings rows.

**Interfaces:**
- Consumes: `Change` (Task 3).
- Produces: `StreamReveal { shown: usize }`; `RunScreen::reveal_tick(&mut self, cx)`, a bounded 16 ms tick that lives only
  while shown text is behind received text.

**Depends-on:** Task 17

- [x] **Step 1:** Add failing tests:
  - `streamed_text_catches_up_within_200_ms_and_the_tick_stops`, on the test clock;
  - `with_reduce_motion_text_shows_at_once`;
  - `an_idle_screen_arms_no_reveal_tick`.
  Run: `cargo test -p reno-app reveal` — expect: fails.
- [x] **Step 2:** Implement it. Run: `cargo test -p reno-app` — expect: pass.
- [x] **Step 3:** Commit `transcript: streamed text is revealed smoothly and the tick stops when it catches up`.

_Requirements: TRANSCRIPT-13.1, TRANSCRIPT-13.2_

---

### Task 23: New edits and commands open in their group, with settings to turn that off

**Files:**
- Modify: `screens/run/output.rs`, `screens/run/transcript.rs`, `prefs.rs`, `screens/settings_pages.rs`
- Test: `screens/run/changes_tests.rs`, `settings_pages_tests.rs`

**Reuse:** rung 2:
- `screens/run/transcript.rs` (`reasoning`, `worked`, the Thinking line), `run/output.rs` (`step_row`, output block,
  Copy), the `ui/motion.rs` pulse discipline and `cx.reduce_motion()`;
- `convo::step_lines` and `line_calls`, the diff line runs (`ui/markdown_view.rs:569` `line_runs`, as the Changes tab uses),
and `prefs.rs`
  plus `settings_pages.rs` for the two settings rows.

**Interfaces:**
- Consumes: `Opens<String>` (`outputs_open`), `ThinkingMode`'s settings row (Task 5).
- Produces: `Prefs::{expand_new_edits, expand_new_commands}`, keys `expandNewEdits` and `expandNewCommands` (default
  true).

**Depends-on:** Task 5, Task 21

- [x] **Step 1:** Add failing tests:
  - `a_new_edit_or_command_opens_inside_its_group_while_live`;
  - `turning_the_settings_off_keeps_them_collapsed`;
  - `the_expand_keys_round_trip_and_a_missing_key_is_on`.
  Run: `cargo test -p reno-app expand` — expect: fails.
- [x] **Step 2:** Implement them, and add the keys to `KEYS`. Run: `cargo test -p reno-app` — expect: pass, including the
  tool-group guard.
- [x] **Step 3:** Commit `tools: new edits and commands open in their group; settings to keep them closed`.

_Requirements: TRANSCRIPT-13.4, TRANSCRIPT-13.18_

---

### Task 24: Edit, patch, command and read-file cards, from the call's own input

**Files:**
- Modify: `screens/run/output.rs`, `vm/convo.rs`, `store.rs` (the live-event apply path; the watched map)
- Test: `screens/run/changes_tests.rs`, `vm/convo.rs` (tests module)

**Reuse:** rung 2:
- `screens/run/transcript.rs` (`reasoning`, `worked`, the Thinking line), `run/output.rs` (`step_row`, output block,
  Copy), the `ui/motion.rs` pulse discipline and `cx.reduce_motion()`;
- `convo::step_lines` and `line_calls`, the diff line runs (`ui/markdown_view.rs:569` `line_runs`, as the Changes tab uses),
and `prefs.rs`
  plus `settings_pages.rs` for the two settings rows.

**Interfaces:**
- Consumes:
  - `TurnBuilder::call(id) -> CallInfo { input: &ToolCall, .. }` (Task 3);
  - `ToolCall { name, input, class }` and `ToolResult { ok, summary, output }` (`crates/reno-proto/src/exec.rs:269-281`,
    `:445-457`);
  - `markdown_view::line_runs` (`ui/markdown_view.rs:569`);
  - `open_file_at`.
- Produces:
  - `convo::edit_of(&ToolCall) -> Option<Edit { path, old, new }>`;
  - `convo::patch_of(&ToolCall) -> Option<Vec<FilePatch>>`;
  - `convo::command_of(&ToolCall) -> Option<String>`;
  - `Store::watched(&RunId, call: &str) -> Option<(Instant, Option<Instant>)>`, backed by
    `Lru<(RunId, String), (Instant, Option<Instant>)>` (1,024, agent-decided) in the store, recorded from GPUI's
    executor clock when the store applies a live `Tool` or `ToolResult`. It survives reopening the screen;
  - selectors `edit-badge-<call>`, `edit-diff-<call>`, `patch-file-<call>-<n>`, `command-copy-<call>`,
    `command-status-<call>`, `read-numbered-<call>`, `raw-input-<call>`.

**Depends-on:** Task 3, Task 23

- [x] **Step 1:** Add failing unit tests: `an_edit_input_yields_its_path_and_both_texts`, `a_patch_input_yields_one_entry_per_file`
  and `a_bash_input_yields_its_command`.
  Run: `cargo test -p reno-app vm::convo` — expect: fails to compile.
- [x] **Step 2:** Add failing GPUI tests on `render2`:
  - `a_one_file_edit_shows_its_badge_go_to_file_and_inline_diff`;
  - `a_patch_shows_one_hunk_block_per_file`;
  - `a_command_shows_copy_and_status_and_a_watched_call_its_elapsed_time`, on the test clock;
  - `the_elapsed_time_survives_leaving_and_reopening_the_thread`;
  - `a_restored_call_shows_no_elapsed_time`;
  - `a_numbered_read_shows_as_a_code_block_and_raw_input_opens`.
  Run: `cargo test -p reno-app changes_tests` — expect: fails.
- [x] **Step 3:** Implement the shapes and the cards with kit components. An unknown input shape falls back to raw input.
  Run: `cargo test -p reno-app` — expect: pass.
- [x] **Step 4:** Commit `tools: edit, patch, command and read-file cards from the call's own input`.

_Requirements: TRANSCRIPT-13.5, TRANSCRIPT-13.6, TRANSCRIPT-13.7_

---

### Task 25: Error callout, hidden empty rows, resource links, and the guarded rows kept

**Files:**
- Modify: `screens/run/transcript.rs`, `screens/run/output.rs`
- Test: `screens/run/convo_tests.rs`, `activity_tests.rs`, `tests.rs`

**Reuse:** rung 2:
- `screens/run/transcript.rs` (`reasoning`, `worked`, the Thinking line), `run/output.rs` (`step_row`, output block,
  Copy), the `ui/motion.rs` pulse discipline and `cx.reduce_motion()`;
- `convo::step_lines` and `line_calls`, the diff line runs (`ui/markdown_view.rs:569` `line_runs`, as the Changes tab uses),
and `prefs.rs`
  plus `settings_pages.rs` for the two settings rows.

**Interfaces:**
- Consumes: `TurnBuilder::call` (Task 3), for a `Tool` with no `ToolResult` at its turn's `Done`; the file-link probe
  (`run/links.rs`); `open_file_at`.
- Produces: selectors `error-callout-<key>`, `error-copy-<key>` and `resource-link-<call>`.

**Depends-on:** Task 11, Task 24

- [x] **Step 1:** Add failing tests:
  - `an_error_turn_shows_a_callout_with_copy_error`;
  - `an_empty_reply_and_a_call_canceled_before_output_are_hidden`, where canceled means no result in an `Interrupted`
    turn;
  - `a_call_with_no_result_in_a_completed_turn_stays_pending_in_its_group`, which holds guard 13.15;
  - `a_worktree_path_in_a_tool_result_is_a_button_that_opens_the_file`.
  Run: `cargo test -p reno-app convo_tests` — expect: fails.
- [x] **Step 2:** Implement them. Run: `cargo test -p reno-app` — expect: pass, including the guard tests for 13.10–13.15
  (the Thinking line, the inline approval line, the output block, chips, the parser's reparse, tool grouping).
- [x] **Step 3:** Commit `transcript: error callouts, hidden empty rows and resource links`.

_Requirements: TRANSCRIPT-13.8, TRANSCRIPT-13.9, TRANSCRIPT-13.10, TRANSCRIPT-13.11, TRANSCRIPT-13.12, TRANSCRIPT-13.13, TRANSCRIPT-13.14, TRANSCRIPT-13.15, TRANSCRIPT-13.16_

---

### Task 26: Every new control reachable by keyboard and themed in both appearances

**Files:**
- Test: `screens/run/find_tests.rs`, `screens/run/select_tests.rs`, `renderers_tests.rs`

**Reuse:** rung 2 — `md/lru.rs` `Lru<K, V>` for every cache; `links::open` and its test guard; GPUI's `HttpClient` seam for
the zero-send test; `protocol_schema_matches_snapshot`; the persist and prefs tests; the kit focus ring; `cargo deny` and
`cargo machete`.

**Interfaces:**
- Consumes: the controls from Tasks 6, 9, 10, 13, 15, 17 and 18.
- Produces: no new interface.

**Depends-on:** Task 18, Task 22

- [x] **Step 1:** Add the failing tests:
  - `every_new_control_is_reachable_and_operable_by_keyboard`;
  - `highlights_callouts_and_diagrams_draw_from_theme_tokens_in_light_and_dark`, which asserts the token roles used, not
    pixels.
  Run: `cargo test -p reno-app keyboard` — expect: a control with no focus path fails.
- [x] **Step 2:** Give each failing control its focus path and label. Run: `cargo test -p reno-app` — expect: pass.
- [x] **Step 3:** Commit `a11y: every new transcript control works from the keyboard`.

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

**Depends-on:** Task 20, Task 26

- [x] **Step 1:** Run these, each expected to pass:
  - `cargo test -p reno-proto protocol_schema` (16.10);
  - the zero-send test and the link-opener guards (16.5, 16.6);
  - the outside-the-worktree image test (16.7);
  - the persist and prefs round trips, with a file from before this feature (16.11);
  - `cargo test -p reno-app settings_pages` (16.13).
- [x] **Step 2:** Run `cargo deny check` and `cargo machete`, and confirm every new crate is pinned with `=` (16.12).
- [x] **Step 3:** Record the results in the build ledger. Commit only if a test file changed.

_Requirements: TRANSCRIPT-16.5, TRANSCRIPT-16.6, TRANSCRIPT-16.7, TRANSCRIPT-16.10, TRANSCRIPT-16.11, TRANSCRIPT-16.12, TRANSCRIPT-16.13_

---

### Task 28: The bench: one very long reply, find while streaming, and the frame budget

**Files:**
- Modify:
  - `apps/renod/src/executors.rs` (`StreamMock` near :129);
  - `apps/renod/src/main.rs` (`parse_stream_mock` near :30);
  - `apps/renod/src/daemon.rs` (near :75);
  - `bench.rs`, `app.rs` (`RENO_BENCH_FIND`);
  - `xtask/src/app_bench.rs`;
  - `xtask/src/app_stats.rs` (the row near :86; `fails` near :348; tests near :399 and :731);
  - `docs/BENCHMARKS.md`.
- Test: `apps/renod/src/executors.rs` (tests module), `xtask/src/app_stats.rs` (tests module), `bench.rs` (tests module)

**Reuse:** rung 2 — `xtask/src/app_bench.rs` (`long_transcript`), `xtask/src/app_stats.rs` (the budget table, `fails` :349),
`apps/renod/src/executors.rs` `StreamMock`, `apps/reno-app/src/bench.rs` (`ScrollPlan`, `RENO_BENCH_SCROLL`).

**Interfaces:**
- Consumes: `renod --stream-mock`; `RunScreen::open_find` (Task 6).
- Produces:
  - `StreamMock::LongReply` (word `longreply`);
  - `RENO_BENCH_FIND=<query>`, which opens the find strip with that query once the run opens;
  - the scenarios `long_reply` and `find_streaming`. The second streams the single-run 2,000-message burst with
    `RENO_BENCH_FIND="looked at"`, which every burst message contains.
  - `long_frame_p95_ms` as a budget (`target: None`).

**Depends-on:** Task 4, Task 6, Task 27

- [ ] **Step 1:** Add failing tests:
  - `the_long_reply_mock_streams_one_reply_with_code_and_math` (renod);
  - `the_long_frame_row_fails_the_run_when_over_16_7_ms`, which replaces the target test near :731 (xtask);
  - `reno_bench_find_opens_the_strip_with_the_query` (app).
  Run: `cargo test -p renod long_reply`, `cargo test -p xtask long_frame` and `cargo test -p reno-app bench_find` — expect:
  each fails.
- [x] **Step 2:** Implement the mock, the hook, both scenarios and the budget row. Rewrite BENCHMARKS.md's "a target, not a
  budget" line. Run the three commands — expect: pass. The `Steady` and `Burst` tests are unchanged (1.11).
- [ ] **Step 3:** Manual acceptance on a quiet Mac: `cargo xtask bench --app`. Expect:
  - long transcript, long reply and find while streaming each at p95 ≤ 16.7 ms;
  - peak footprint ≤ 370 MiB;
  - the streaming, idle and cold-start budgets met.
  A miss on find while streaming moves the recount to an owned background task. A miss on 16.1 from reparsing raises the
  tree bound or parses off-frame. Both go through `reroute-plan`.
- [x] **Step 4:** Commit `bench: one very long reply, find while streaming, and the long-transcript frame budget`.

_Requirements: TRANSCRIPT-16.1, TRANSCRIPT-16.2, TRANSCRIPT-16.3_

---

### Task 29: Show every addition to the locked screen at a UI stop, before landing

**Files:**
- Modify: `./ui-brief.md` (after the operator's go)
- Test: none. This is a manual acceptance step: the operator's review in the running app.

**Reuse:** none — new code (rung 7) for `vm/find.rs`, `screens/run/find.rs` and `ui/find.rs`, new on this branch and kept
from the draft. Rung 2: the kit field `ui/field.rs`. Rung 5: `regex` 1.13.1, already in `Cargo.lock` as a transitive
dependency; the plan adds it as a direct workspace dependency pinned `=1.13.1`, for 3.9's regex mode.

**Interfaces:**
- Consumes: the find toggles and error state (Task 6), the scrollbar (Task 10), the turn-end actions (Task 11), the diagram
  tabs and zoom (Tasks 12, 13), the context menu (Task 17), the rich-markdown drawing (Task 18), the tool cards and callouts
  (Tasks 23–25), and the Thinking and expand settings rows (Tasks 5, 23).
- Produces: the operator's go on each addition; `./ui-brief.md` updated to what they accept.

**Depends-on:** Task 26. This task must finish before `land-branch`.

- [ ] **Step 1:** Build release and launch `reno-app --fake` with `RENO_DATA_DIR` under a temp dir. Capture each addition by
  window id on the seeded threads (`render2` and the handshake thread), and open each capture.
- [ ] **Step 2:** Hand the captures and the launch command to the operator the way the charter's UI stop does, leave the app
  running, and wait for the operator's explicit go. Apply any change and show it again.
- [ ] **Step 2b:** U3 check at the same stop. The operator copies `$$\int_0^1 x^2\,dx = \tfrac13$$` with ⌘C and pastes it
  into a tool of their choice. Record whether the TeX with its `$$` delimiters (TRANSCRIPT-2.6) works there, and close or
  reopen U3 in requirements' Open Questions.
- [ ] **Step 3:** On the go, update `./ui-brief.md` (the Find strip, Navigation, Rich blocks and Settings surfaces, and the
  Amendments) and commit `docs(specs): the ui brief takes the additions the operator accepted`.

_Requirements: TRANSCRIPT-2.6, TRANSCRIPT-3.9, TRANSCRIPT-3.10, TRANSCRIPT-5.3, TRANSCRIPT-5.4, TRANSCRIPT-8.3, TRANSCRIPT-10.1, TRANSCRIPT-10.4, TRANSCRIPT-13.5, TRANSCRIPT-13.6, TRANSCRIPT-13.8, TRANSCRIPT-13.17, TRANSCRIPT-13.18, TRANSCRIPT-14.1, TRANSCRIPT-14.2, TRANSCRIPT-14.3, TRANSCRIPT-14.4_
