# UI brief: the ⌘K palette (ROAD-34)

Status: Locked
Date: 2026-11-19
Screenshot: [2026-11-19-palette-ui-brief.png](./2026-11-19-palette-ui-brief.png)

## Decision

Direction **E "Compact tabs"**: the scope tabs and live counts of draft B, made compact (one-line
rows, underline tabs, no footer), with draft D's waiting detail shrunk to one line and kept only
for a held action. Five directions were drafted in the app on the fake daemon (A Spotlight list,
B Scoped tabs, C List + preview, D = A with C's waiting preview inline, E). The owner first
agreed to D, then asked for "one more option upgrade from B - but make it more compact, and
modern more", judged E "better", removed the question line ("the status dot is enough"), then
"remove it and lock".

## Grounding

Token source of record: the app-shell brief's `Theme` value set
([2026-11-19-app-shell-ui-brief.md](./2026-11-19-app-shell-ui-brief.md)). No token is added.
Typefaces: Geist (UI), Geist Mono (paths, lines, counts, ages, keycaps). Icons: the app's set
(`Search`, `File`, `List`, `Folder`, the nav icons) and `kit::logo` for harnesses.

The palette is the app's only search and its main way around: the sidebar has no thread search
("search is ⌘K"), and All runs and Triggers are reached through it (app-shell brief).

## Job

- **Question:** Where is the thing I am after, and is anything waiting on me?
- **Finding:** in words, first: the head of the first section reads "Needs you · **3 threads need
  you**" (warning), and those threads are the first rows.
- **Primary job:** ↵ opens the selected thread; with an empty query the first waiting thread is
  selected, so ⌘K ↵ goes straight to it. Answering stays in the thread's composer (app-shell
  signature); the palette never approves, denies or answers.

## Surfaces

### Sheet

- **Layout:** a 620 pt sheet, 12 pt radius, anchored 96 pt below the window top, centred, over a
  black 35% scrim (click on the scrim closes). Top to bottom: input row (46 pt), tab row (32 pt,
  hairline under it), result list (max 400 pt, scrolls; rows never shrink to fit).
- **Components:** rung 1 — `ui::field::Field` (needs a borderless look: no fill, border or
  padding, the parent's text size, the placeholder shown beside the caret while focused, text
  left-aligned; a small extension of `field::Look`); `ui::overlay` signal (unchanged).
- **States:** opening focuses the input; closing returns focus to the composer (as today).
- **Type & color:** `surface` fill, hairline border, the kit's large shadow.
- **A11y:** the input is labelled "Search"; Esc closes from anywhere in the sheet.

### Input row

- **Layout:** search icon (15 pt, faint), the field (Geist 14.5, placeholder "Search threads,
  files and commands"), right-aligned the project whose files are searched: folder icon + name,
  12 pt faint ("web-console"). The project is the focused thread's; on Home there is none and
  nothing is shown.
- **Components:** rung 1 — `Field`, `icon`.

### Tab row

- **Layout:** All · Threads · Files · Text · Commands, 8 pt apart, then right-aligned a keycap ⇥
  and a hint: with an empty query "> commands / files # text", while typing "scope".
- **Components:** new (rung 7) — an underline tab: text 12.5 (semibold and `text` when active,
  `muted` otherwise), a count in Geist Mono 11 after the label, a 2 pt `accent` underline on the
  active tab. `kit::segment` is a pill and does not carry a count or an underline.
- **States:**
  - Counts show while typing; with an empty query only Threads shows its count.
  - Files and Text show no count when no thread is focused.
  - While the project is indexing, Files shows "2+" and Text shows no count.
  - The active tab's count is `accent`; the others are `faint`.
- **Keys:** ⇥ / ⇧⇥ move between tabs; ⌘1–⌘5 pick one; a leading `>`, `/`, `#` in the query
  scopes to Commands, Files, Text.
- **A11y:** tabs are buttons with toggled state; the count is part of the label ("Files, 3").

### Result list

- **Layout:**
  - **All tab, empty query:** sections Needs you (only when something waits) · Recent (5 threads)
    · Commands (3, then "+4 commands · ⇥ to see all").
  - **All tab, typing:** Threads · Files · In files · Commands, each at most 4 rows, then
    "+N in files · ⇥ to see all".
  - **One tab:** that kind only, no section heads, the full list (scrolls).
  - **Section head:** 26 pt; title 11 pt faint medium; the Needs-you head adds the finding in
    `warning` semibold; the other heads carry their count right-aligned in Geist Mono.
- **Components:** rung 2 — section head over `panel::view::group_label`; the count on the right
  is new to it.
- **States:**
  - **No thread focused:** under Files "Open a thread to search its project's files"; under In
    files "Open a thread to search inside its files".
  - **Indexing:** the files found so far, then a note with a small spinner "Indexing
    web-console… 731 of 1,204 files"; under In files "Searches inside web-console once indexing
    finishes".
  - **Nothing matches:** "Nothing matches “qzx”" (13.5, `text`) over "Threads match their title or
    project; files match their path." (12.5, faint).
- **Ranking:**
  - Threads: waiting ones first, then most recently updated; a thread matches when every word
    of the query is in its title or project.
  - Files: a name containing the query first, then a path containing it, then shorter paths
    (fuzzy subsequence on the path).
  - Text: lines containing the query, case-insensitive, in file order.
  - Commands: the shipped `filter()` (prefix matches first).

### Rows (one line, 30 pt, 10 pt inset, 8 pt radius)

- **Thread:**
  - Left to right: the harness logo (14 pt), the title (Geist 13, `text`), the project (12,
    faint), then right-aligned the badge "Needs input" / "Needs approval" when it waits, and the
    age (Geist Mono 11, faint).
  - The title is the run title without its bot and repository prefix, as the sidebar shows it.
- **File:** file icon, the file name, then its folder in Geist Mono 11.5 faint ("src/cache").
- **In-file line:** list icon, the matching line trimmed in Geist Mono 12 (truncates), then
  right-aligned "cache.rs:6" in Geist Mono 11 faint.
- **Command:** its icon, the label, then right-aligned its shortcut keycap when it has one (⌘,
  for Settings, ⌘B for the sidebar).
- **Match:** the first occurrence of the query in a row's title, name or line is bold and
  `accent`.
- **Selected:** `raised` at 75% fill. Hover is the same fill.
- **Held action, selected:** the row grows a second line, indented to the title:
  - the bot's sentence, as Home's approval card words it, in `text` ("scout wants network
    access", "keeper wants to push to atlas-api");
  - the exact command in Geist Mono 11.5 `muted`, truncating;
  - right-aligned a ↵ keycap and "open".
- **Question, selected:** no second line. The badge says it.
- **Components:** rung 1 — `kit::logo`, `sidebar::badge_tag` (make it shared), `icon`, the panel's
  `keycap` (move it into `kit`); `summary::parse_title`, `summary::approval_bot`,
  `summary::action_word`, `summary::subject_line` for the held-action sentence and command.
- **Keys and clicks:** ↑ ↓ move (wrapping); ↵ or a click activates:
  - a thread opens it (`open_run` on its latest run);
  - a command runs it;
  - a file opens it in the side panel's Files tab;
  - a line opens that file at that line.
- **A11y:** each row is a button whose label includes the badge ("check upstream advisories,
  Needs approval"); the second line is part of the selected row's label.

## Type & color (summary)

Geist 14.5 input, 13 rows, 12.5 tabs, 11–12 meta; Geist Mono for counts, ages, paths, lines and
keycaps. `accent` only on the active tab's underline and count and on matched text; `warning`
only on the finding; the badges keep their sidebar tints. Light and dark both use the tokens
above; the light accent text on `surface` was checked in the draft (no white on light teal).

## Signature

The palette knows what is waiting: ⌘K opens on "3 threads need you" with the first one
selected, and the tabs count every kind of result before you switch.

## Amendments (owner, during review)

1. A chosen over B and C at first, with C's preview for waiting threads only (hybrid D).
2. Then an extra direction from B, "more compact, and modern more": E.
3. E chosen over D ("that better").
4. No second line for a question: "the status dot is enough".

## Open for the build and the contracts (not decided here)

- **Held-action line.** The owner removed the question line; the held-action line (with its
  command) was kept because it shows what the badge cannot. Confirm before the build if it
  should go too.
- **File search needs a run.** `fs.search_files` and `fs.search_content` take a `run_id`, so on
  Home there is nothing to search (the "Open a thread…" state). Searching a project with no
  thread focused needs a project-level method (ROAD-52 / backend).
- **No index today.** The indexing state assumes a project file index warmed when the palette
  opens (gap row B47). Today `fs.search_content` runs a `git grep` per call; if the real cost
  stays low the indexing state may never show.
- **Fake daemon data (ROAD-52).** Its file tree is a five-file test stub; the draft used a
  fixture shaped like the fake world (web-console's `src/cache.rs`, `src/headers.rs`). The
  thread list carries no question text, so a "Needs input" row can only say its badge; a
  thread's last message is not available either.
- **Keys.** ⌘1–⌘5 may collide with ROAD-32's planned 1–9 thread jumps; decide there. ⌘J is
  now the panel jump on main; the palette does not use it.
- **Not drafted:** the daemon connecting / offline (the thread list is the last one known), and
  more than 9 waiting threads.
