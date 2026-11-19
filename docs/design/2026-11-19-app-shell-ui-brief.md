# UI brief: the app shell, conversation and inbox (ROAD-30)

Status: Locked
Date: 2026-11-19
Screenshot: [2026-11-19-app-shell-ui-brief.png](./2026-11-19-app-shell-ui-brief.png)

## Decision

Direction **V3 "Pro"**, amended in three review rounds, with the conversation following the
conventions of current desktop coding-agent apps (researched and observed live on 2026-11-19).
The owner's words: "Tôi muốn đi theo hướng V3", "theo bạn đề xuất" (the conversation follows the
recommended reference app), then "Chốt".

## Grounding

Token source of record: a new `Theme` value set (below) replacing the deleted brief's palette.
Typefaces: Geist (UI), Geist Mono (data, paths, commands). Icons: Phosphor (existing assets).
Spacing unit: the panel gutter `panel::layout::GUTTER` (8 pt) is the one gap used between and
around canvases, and under the status bar.

| Token | Dark | Light |
|---|---|---|
| shell | `#080a0c` | `#e6edef` |
| canvas | `#0b0e11` | `#f4f7f8` |
| card | `#0e1216` | `#ffffff` |
| surface | `#141a1f` | `#ffffff` |
| raised | `#262c33` | `#d8e1e4` |
| text | `#dfe5ea` | `#16232b` |
| muted | `#98a3ad` | `#4b5b66` |
| faint | `#6f7a85` | `#6b7b86` |
| accent (live, primary) | `#2dd4bf` | `#0f766e` |
| warning (needs you) | `#fbbf24` | `#b45309` |
| danger | `#f87171` | `#dc2626` |
| success | `#34d399` | `#15803d` |

Text on an accent or semantic fill picks its colour by contrast (never white on light teal).

## Job (worked-in screens)

- **Question:** What is waiting on me, and from which run? Is this run going well?
- **Finding:** shown in words where it is read first: a thread row badge "Needs input" /
  "Needs approval"; in the run, the agent's own question or the harness's permission sentence.
- **Primary job:** answer or decide on that item, in the panel that takes the composer's place,
  one key or one click away.

## Surfaces

### Status bar (unified with the window title bar)

- **Layout:** 40 pt, full width, shell colour, no visible edge; controls 28 pt high, centred.
  Left after the traffic lights: sidebar toggle, back / forward, "…" menu (command palette,
  toggle theme, toggle side panel), the current-thread chip (project initial or icon + title,
  truncated, max 300 pt). Right: "▶ N live" (opens All runs), "⌘K" (opens the palette),
  "● renod" (dot coloured by connection), the agent-updates island, the panel toggle (the only
  panel toggle in the app).
- **Components:** rung 1 — `ui/kit.rs` `icon_button`, `icon_chip`, `tip`, `focus_ring`; new
  (rung 7) — the updates island (overlapping executor logos in 22 pt boxes with a 2 px ring in
  the bar colour, overlap a third of a logo, plus a download icon) and its popover ("Agent
  updates", Update all, per agent "current → new" with Skip / Update), because no kit piece
  stacks logos.
- **States:** live count 0..n; renod connected / connecting / offline; island hidden when no
  agent has an update; Update shows a spinner then "Up to date".
- **Type & color:** Geist 13; the thread chip's initial on a tinted square; island ring = shell.
- **A11y:** every control has a label and the kit focus ring; back/forward and the chip are
  reachable by keyboard.

### Sidebar

- **Layout:** "＋ Start a task" at the top; "Threads" grouped by project, always (no View
  control, no thread search; search is ⌘K); each project header has a "+" (dim, bright on
  hover) that opens the new-thread state in that project; thread rows: status dot or spinner,
  title (truncated), a text badge, age. Footer: Settings as a compact 28 pt rounded row (gear +
  "Settings" + a tinted count of agents that cannot take runs), no divider line. Settings, when
  open, takes the sidebar's place (owner decision, unchanged).
- **Components:** rung 1 — `ui/sidebar.rs` rows, `kit::tag`, `spinner`, `focus_ring`; rung 2 —
  text badges "Needs input" (warning tint) and "Needs approval" (success tint) as `tag` variants.
- **States:** empty ("No threads yet"); running (spinner); needs input; needs approval; idle.
- **Type & color:** Geist 13 rows, Geist Mono for ages; badges tinted fills, never saturated discs.
- **A11y:** rows are buttons with labels that include the badge text.

### Conversation

- **Layout:** no run header inside the canvas (the title lives in the status bar; Stop lives in
  the composer). One centred column; a thin tick rail at its left edge (one tick per message).
- **Components:** rung 1 — `ui/markdown_view.rs`, `reno-markdown`, `reno-syntax`; rung 2 — user
  bubble and tool rows over the existing transcript rows; new (rung 7) — the "Worked for Ns ›"
  fold, because no current row folds a turn.
- **States:**
  - Agent text bare on the canvas: no card, no avatar, no name; quiet icons under the final
    message of a finished turn (copy, …).
  - User message: neutral raised bubble, right-aligned; timestamp + copy + edit on hover; longer
    than 320 characters collapses behind "Show more ▾".
  - Tool activity: one muted line with a leading icon (terminal "Ran …", search "Read …",
    pencil "Edited N files", globe "Fetched …"), in Geist (not Mono); a live step shimmers
    "Running …"; "Thinking" shimmers while the agent works.
  - Routing: one muted row "Routed to <executor> · <reason>" (keeps GOAL-1's reason).
  - A finished turn folds its interim text and steps into "Worked for Ns ›", expandable in place.
  - An answered question shows as a user bubble with the question muted above the answer.
  - A question that can no longer be answered (run ended, daemon restarted, the harness gave up) shows one muted line where it was asked: "Question closed: <reason>" (ROAD-1).
  - "Worked for Ns" uses the turn's real elapsed time from event timestamps; when a turn has no timing, the fold reads "Worked ›" with no number.
- **Type & color:** Geist 13.5 body (the transcript's current size); rows and metadata Geist 13; code, keycaps and times Geist Mono 12–12.5 on `surface`; muted rows in `muted`.
- **A11y:** the fold is a disclosure button; shimmer respects Reduce motion.

### Composer, and the panels that take its place

- **Layout:** one large rounded box at the bottom of the column; placeholder "Work with
  <executor>" ("Do anything" on a new thread); bottom row: "+" menu, the harness's own
  permission-mode chip, then right-aligned the model + effort chip and the send / stop button.
- **Components:** rung 1 — `ui/composer.rs`, `ui/text_area.rs`, `kit::icon_button`, `overlay`;
  new (rung 7) — the question panel, the approval panel, the effort popover with a 5-step slider,
  because none exist.
- **States:**
  - Send / stop morph: stop (square) while the run is live and the box is empty; ↑ send as soon
    as there is text.
  - "+" menu: Files and folders, Goal, Plan mode (more as harnesses offer them).
  - Permission chip: the harness's own modes with one-line subtitles (e.g. Ask for approval /
    Approve for me / Full access, the last in warning tone); shrinks to its icon while a panel is open.
  - Model chip opens a popover: effort title (warning tone), model name ›, 5-step slider.
  - **Question panel** attached above the composer: "? Question" + ×; the question; numbered
    options with keycaps (digit keys pick while the panel has focus); one inline line "✎ Or
    write your own response" (no box of its own) + Skip + Send; a single choice sends on click;
    multi-select / several questions use Send and page "1/2"; a refused answer is explained
    inline ("<Agent> takes only one of: …"); Esc closes it and a chip "Question waiting · Open"
    reopens it.
  - **Approval panel** replaces the composer: kind label + icon ("Terminal", "Network", …), a
    plain sentence ("May I …?"), the exact command or URL in a muted mono block, "Deny  Esc" and
    primary "Allow once  ⏎" with a ▾ split menu "Allow once" / "Allow similar commands" (the
    latter writes the rule to the harness's own settings, GOAL-6). With the real daemon the ▾ menu is hidden until that write exists (ROAD-5); only Deny and Allow once show.
  - New-thread state: centred mark, "What should we build in <project>?" (project underlined),
    a chip strip above the composer: "This Mac" · project · "…".
- **Type & color:** Geist 13.5 input; keycaps in Geist Mono on `raised`; primary button = accent
  fill with contrast-picked text.
- **Focus (mis-press guard):** a panel never steals focus. If the owner is typing, it opens beside the cursor without taking the keys; its digit, Enter and Esc keys work only after the owner clicks the panel or jumps to it (⌘J). This is the only guard on an answer that cannot be taken back.
- **A11y:** Esc / Enter shown on the buttons; every option is a labelled button; the panel announces itself when it opens.

### Side panel and All runs

- **Layout:** the side panel keeps its tabs (Session, Review, …) and has no hide button of its
  own; it does not repeat run status ("Needs input …", "Held for you") that the composer panels
  already show. All runs (opened from "N live"): one lane per live run, a status stripe at the
  lane's edge, a live one-line tail, an "asks" badge, a lead's children indented under it;
  status and time never wrap.
- **Components:** rung 1 — the existing panel views; rung 2 — lanes over `sidebar` row pieces.
- **Type & color:** Geist 13 titles, Geist Mono 12 tails and times, tabular numbers.

### Where today's screens go

- Home becomes the new-thread state ("Start a task").
- Runs becomes the All runs view (the "N live" pill, ⌘K).
- Triggers is reached from the status bar "…" menu and ⌘K until bots get their own place (MILE-7).
- Executors stays inside Settings (as shipped).

### Real daemon vs. not-yet controls

Controls whose daemon support is not built are hidden with the real daemon and shown on the fake
daemon (`--fake`) so the design stays testable: Plan mode, Goal, the harness-native permission-mode
chip (ROAD-4), "Allow similar commands" (ROAD-5). One exception: a question panel still appears
with the real daemon, read-only, saying "Answer it in <harness> for now", until answering lands
(ROAD-1). No control is ever shown that does nothing.

## Signature

Every live agent is visible at once (All runs and the sidebar badges), and anything an agent
needs from the owner arrives in the one place they already act: the composer slot.

## Amendments (owner, during review)

1. V3 chosen; V1 and V2 dropped.
2. Sidebar: no View control (always by project), no thread search, a "+" new-thread button per project.
3. The live / ⌘K / renod pills move into a new status bar with more information (thread chip,
   navigation, agent updates).
4. The gap under the status bar matches the other gaps; the Settings footer joins the design.
5. The conversation follows the researched reference app instead of a new design.
6. Agent-flagged at lock, consistent with 5, applied in the build: no repeated run status in the
   side panel; tool rows in Geist, not Mono; the write-your-own line without its own box.
7. After lock (owner, 2026-11-19): panels never steal focus; Home / Runs / Triggers fold into
   new-thread, All runs and the "…" menu; "Allow similar commands" hidden with the real daemon
   until ROAD-5; not-yet controls hidden with the real daemon except a read-only question panel.
