# UI brief: the composer — where a task runs, completions, the message queue (ROAD-31)

Status: Locked
Date: 2026-11-20
Screenshot: [2026-11-20-composer-ui-brief.png](./2026-11-20-composer-ui-brief.png)
Inherits: [2026-11-19-app-shell-ui-brief.md](./2026-11-19-app-shell-ui-brief.md) (tokens, type, composer box and foot) and [2026-11-19-run-activity-ui-brief.md](./2026-11-19-run-activity-ui-brief.md) (the tray card above the composer)

## Decision

Hybrid **"Popovers + sentence"**. The base is direction A: grouped completion popovers and a queue card above the composer. The one change from A is that the new thread says where the task runs as direction C's sentence, and that sentence replaces A's chip strip. Direction B (the command sheet) and the rest of C (ghost text, colour-coded tokens, the "N queued" chip) were dropped.

The owner's words: "đồng ý với đề xuất" (A with C's sentence), then "chốt".

## Grounding

Token source of record is the app's `Theme` (the shipped default is now the Vercel theme). There are no new tokens. Typefaces: Geist for UI text; Geist Mono for paths, commands and keycaps. Icons: Phosphor.

## Job (worked-in screen)

- **Question:** Where will this message go, and what does it bring with it? On a live run: what is waiting to be sent?
- **Finding:**
  - New thread: the heading itself, "What should we build in reno on main in a new worktree?".
  - Live run: "2 messages wait for this turn to end".
- **Primary job:** write and send to the right place. Change the place with one click on the word that names it. Pull in a file, command or skill with one key. Fix the queue before it goes.

## Surfaces

### New thread: the place sentence

- **Layout:** the mark, then the heading "What should we build in [project] on [branch] in [checkout]?". Each bracket is underlined and is its own control. Clicking it opens a popover anchored to that word, which rises above it and never covers the composer box. The column sits lower (extra top room) while the sentence shows, so the popovers have space.
  - Project popover: search, Recent, All projects, Clone from URL….
  - Branch popover: search; the default branch is marked; branches checked out in a worktree say "in worktree …"; whole rows only; "Showing 20 of 34".
  - Checkout popover: New worktree (default) / This checkout / Existing worktree.
- **Components:**
  - rung 1: the composer `popover` helper, `kit::focus_ring`, picker list rows from `ui/composer/picker.rs`.
  - new (rung 7): the underlined place words, because no kit piece is an inline control inside a heading.
- **States:**
  - Each choice updates the sentence at once, and the send carries it.
  - Clone runs with a spinner. If it fails, the popover says "Could not clone: repository not found" and how to fix it, and keeps the URL to edit.
- **A11y:** each word is a button labelled with what it changes ("Project: reno"); the popovers are keyboard-navigable lists.

### Completions (`@` files, `/` commands, `$` skills)

- **Layout:** a popover rising from the composer's left edge, grouped under faint uppercase headers: "Files", "<Harness>" (its own commands), "Reno" (app commands), "Skills". Each row has an icon, a mono name and a muted path or description. A keycap footer reads "↑↓ move · ⏎ Tab insert · Esc close". It may cover the heading; it never covers the box.
- **Behaviour:**
  - Typing opens it; it never takes focus from the box, and typed text is never lost.
  - Esc closes it. Tab or Enter inserts. With no list open, Tab does what it did before.
  - An inserted mention is styled in the text (neutral wash, accent trigger character), not as an atomic chip, so undo cannot leave a stale token.
- **States:**
  - If the harness commands cannot be read, a warning line "<Harness>'s commands could not be read" shows with Try again, and the app commands still list.
  - App commands listed are only those that act: /model, /resume, /settings, and /stop on a live run.
- **Components:** rung 1 is the `popover` helper and the picker rows; new (rung 7) is the grouped completion list.

### Queue card (live run)

- **Layout:** a card above the composer, in the Plan tray's language. The header is the finding "N messages wait for this turn to end" with a "Queued · N" tag. Each row has a drag handle ⋮⋮, the message on one line, ✎ (edits in place), × and "Send now" (interrupts the turn). ⌥↵ queues the box's text.
- **States:**
  - Messages leave the card one per turn end, in order.
  - An add on an ended run shows "The run ended; send it as a new message instead" above the box and keeps the draft.
  - The card is absent when nothing waits.
- **Components:** rung 1 is `kit::icon_button`, `kit::tag`, `focus_ring`; rung 2 is the run-activity tray card.

### Real daemon vs. not-yet controls

The capabilities `harness_commands`, `message_queue` and `place_chooser` are false on the real daemon and true on the fake (`--fake`).

On the real daemon:
- What stays: the app `/` commands, and `@file` on a live run over the real `fs.search_files`.
- What is hidden: harness commands, skills, the queue and the place sentence. The new-thread page stays as it was.

## Contract

These live in `reno_client`, mocked by the fake daemon. The backend builds them to this shape.

- **`CompletionDesk`**
  - `project_files(project, query, limit) -> [ProjectFile { path, head }]`. An unknown project gives an empty list.
  - `harness_commands(harness) -> Result<[HarnessCommand { name, kind: Command|Skill, description, usage }], CommandsError::Unreadable>`.
- **`MessageQueue`**
  - Operations: `queue(run)`, `queue_add(run, text)`, `queue_edit(run, id, text)`, `queue_move(run, id, to)`, `queue_remove(run, id)`, `queue_send_now(run, id)`. Messages have stable ids.
  - Errors the screen handles: `RunEnded`, `UnknownRun`, `UnknownMessage`, `Empty`.
- **`PlaceDesk`**
  - Reads: `projects()`, `recent_projects()`, `branches(project, query, limit) -> BranchPage { branches, total }`, `worktrees(project)`.
  - Clone: `clone_project(url) -> Result<Project, CloneError::NotFound|Empty>`.
  - Place of a run: `record_place(run, place)` / `run_place(run)`.
- **Send:** the project goes to `task.run` as `cwd` today. The branch and the checkout choice need new `task.run` fields (base ref, checkout kind) on the daemon.

## Signature

The new thread reads as a sentence that says where the work will happen, and every word in it is the control that changes it.

## Amendments (owner, during review)

1. A as the base with C's sentence for the place, and B dropped. The owner agreed to the recommendation.
2. Agent-applied before lock, consistent with the app-shell rule "no control that does nothing":
   - /new, /diff, /files, /terminal, /rename and Browse… are not shown until they act;
   - B's sheet clipping fix (moot, since B was dropped).

## Code and tests

- **Code:**
  - client: `crates/reno-client/src/{completions,queue,places}.rs` and `fake/`;
  - app view-models: `apps/reno-app/src/vm/{completions,queue,places}.rs`;
  - views: `apps/reno-app/src/ui/composer/{completions,queue}.rs`, the sentence in `screens/home.rs`;
  - switch: the app's `--fake` mode, plus the three `Capabilities` flags.
- **Lock tests:** `apps/reno-app/src/composer_surface_tests.rs`, each seen failing once against a broken guard. The real new-thread page is held by `new_thread_tests` on REAL capabilities.
