# UI brief: the sidebar — what you can do with a thread (ROAD-32)

Status: Locked
Date: 2026-11-20
Screenshot: [2026-11-20-sidebar-ui-brief.png](./2026-11-20-sidebar-ui-brief.png)
Inherits: [2026-11-19-app-shell-ui-brief.md](./2026-11-19-app-shell-ui-brief.md). The sidebar layout is unchanged: "Threads" grouped by project, a "+" per project, row badges and the Settings footer.

## Decision

Direction **A "Menu"**, unchanged. B (hover actions with a second row line and a "Threads ▾" filter) and C (a details card, pinned chips and an Archived page) were dropped. The owner's words: "Tôi chọn A".

## Grounding

The token source of record is the app's `Theme`; no new tokens were added. Typefaces are Geist 13 for rows and menu items, and Geist Mono 11.5 for keycap hints and ages.

## Job (worked-in screen)

- **Question:** Which of my threads matter now, and how do I find the one I want?
- **Finding:** the order says it. Pinned threads come first, then the existing "Needs input" / "Needs approval" badges, then threads by project. What is out of sight is counted in words: "Archived · 4", "Show 6 more".
- **Primary job:** rename, pin, archive or close a thread from its own row, each at most two clicks away, without leaving the conversation.

## Surfaces

### Row menu

- **Layout:** right-clicking a row, or clicking its "…" (shown on hover), opens a menu next to the row. The entries, in order:
  1. Rename
  2. Pin / Unpin (⌘⇧P)
  3. Archive (⌘⇧A)
  4. Copy ▸, a submenu with Path, Thread ID and Harness session ID
  5. a divider
  6. "Close thread…" in the danger tone
- **Components:** new (rung 7) is `ui/menu.rs` (panel, popup, row, divider, submenu), because the kit had no context menu. It is the menu every later surface uses.
- **States:**
  - Esc backs out one step at a time: submenu, then menu, then rename.
  - Copy shows a toast.
  - Pin and archive apply at once, with no toast.
- **A11y:** rows and menu items are labelled buttons, keycap hints are shown, and the menu is keyboard-navigable.

### Rename

- **Layout:** inline. The row's title becomes a field; Enter saves and Esc cancels.
- **States:** an empty title is refused under the row with "A thread needs a title", and the field stays open.
- **Components:** rung 1, `ui/field.rs` with an opt-in rename look. It reports a blank submit.

### Pinned and Archived

- **Layout:**
  - A "Pinned" section sits above "Threads", most recently pinned first.
  - Archived threads leave their groups and collapse into one row at the bottom, "Archived · N ▸". It opens in place, and each archived row has an Unarchive button.
  - Archiving also unpins.
- **States:** archiving a thread with a live run is refused under the row with "Stop the run before archiving".

### Long groups, hover details, width

- **Show more:** a long project group ends in "Show N more", which expands in place.
- **Hover details:** hovering a row shows a tooltip with the full title, project, branch and harness.
- **Width:** the sidebar edge drags. The width is clamped to 200–380 pt, defaults to 248, and is remembered as the app-local pref `sidebarWidth`.

### Close thread

- **Behaviour:** "Close thread…" always asks first, using `vm::threads::close_question`, and says what the worktree keeps. Examples:
  - "reno/atlas01 with 3 commits not merged and uncommitted changes stays in the worktree: closing removes nothing."
  - "Closing releases the worktree; nothing in it is unmerged."
  - "This thread has no worktree: closing only ends it."
- **States:** on a thread with a live run, the confirm shows the busy line and the close button is disabled.
- **Daemon:** this uses the real `thread.close`.

### Real daemon vs. not-yet controls

`Capabilities.thread_edit` is false on the real daemon and true on the fake (`--fake`).

- **On the real daemon, the menu shows:** Copy ▸ and "Close thread…".
- **On the real daemon, these are hidden:** Rename, Pin and Archive, the Pinned section, and the Archived row.
- **On both:** hover details, "Show N more" and resize.

## Contract

The contract is `reno_client::thread_desk::ThreadDesk`, mocked by the fake daemon. On the wire it is meant to be `thread.update {threadId, title?, pinned?, archived?}` plus an archived filter on `thread.list`, with a CLI route for each (ARCH-2).

- **Methods:**
  - `marks() -> ThreadMarks { titles, pinned, archived }` (pinned and archived most recent first)
  - `archived() -> [ThreadId]`
  - `rename(thread, title)` (the title is trimmed)
  - `set_pinned(thread, bool)`
  - `set_archived(thread, bool)` (archiving unpins)
- **Errors:**
  - `NeedsTitle`: "A thread needs a title"
  - `LiveRun`: "Stop the run before archiving"
  - `Archived`: "Unarchive the thread before pinning it"
  - `UnknownThread`: "This thread is not known to the daemon"

## Signature

One quiet list and one menu. Everything you can do with a thread is a right-click away, and what the list hides is counted in words.

## Amendments

None. The owner chose A as shown.

## Known gaps

- Archived threads still appear in the ⌘K palette, which uses the old listing.
- Real mouse hover and drag were not driven in a live window. Both are covered by simulated events in tests.

## Code and tests

- Code:
  - the menu kit: `apps/reno-app/src/ui/menu.rs`;
  - the sidebar: `ui/sidebar.rs`, `ui/sidebar/{state,rows}.rs`;
  - view-models: `vm/{projects,threads}.rs`;
  - the client contract and its fake: `crates/reno-client/src/thread_desk.rs` and `fake/thread_desk.rs`.

  The fake is switched by `--fake` (`FakeConfig.sidebar_threads`, set in `start_fake`). The Capabilities flag `thread_edit` gates the controls.
- Lock tests: `apps/reno-app/src/sidebar_tests.rs`, 12 tests. Each was seen failing once against a broken guard.
