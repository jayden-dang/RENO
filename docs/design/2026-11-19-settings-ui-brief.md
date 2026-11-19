# Settings — UI brief (locked 2026-11-19)

Screenshot of the locked draft: `2026-11-19-settings-ui-brief.png` (account emails masked).

- **Decision:** variant A, "Takeover". Owner: "Chốt hướng A - implement nó luôn." Chosen over
  B (inset two-column ledger with a health strip) and C (modal sheet with colour tiles and
  drill-in pages).
- **Grounding:** token source of record is `apps/reno-app/src/ui/theme.rs` (`reno_light`,
  `reno_dark`). No token is added. Fonts are Geist / Geist Mono, as the app bundles them.
- **Copy and facts:** everything the screens say comes from the existing view-models
  (`vm::executors`, `vm::settings`, `vm::rules`, the notification prefs). The draft's
  invented lines (a "Models" fact, an auto-update sentence, the banner wording) do not ship.

## Surface 1 — Sidebar (app)

- **Layout:** nav rows are Home, Runs, Triggers. Executors and Settings leave the top list.
  The 52 px footer that held "L · Local · ● renod" is now one full-width **Settings** row:
  gear icon, the word "Settings", and on the right a danger count of executors that cannot
  take runs (hidden at 0).
- **Components:** rung 1 — `sidebar::nav_row` look (32 px, `row_selected` when on). The count
  is new (rung 7): a danger pill with a white number, because no kit badge carries a count
  on a danger fill.
- **States:** daemon connected → no extra mark. Connecting → a warning pip on the gear.
  Disconnected → a danger pip on the gear. The always-on "renod" word is gone. The daemon
  state is stated in Settings › Daemon, and as a banner on every Settings page while the
  daemon is unreachable.
- **A11y:** the row is a button with the id and selector `nav-Settings`. Its accessible label
  includes the count ("Settings, 2 executors need you").

## Surface 2 — Settings navigation (replaces the sidebar while Settings shows)

- **Layout:** same width (`sidebar::WIDTH`), same 40 px titlebar band (traffic lights), same
  `shell` ground. Top to bottom: "← Back to app"; the title "Settings" (18 px semibold); a
  search field; then the categories in groups:
  - **Agents:** Executors, Add from registry
  - **Automation:** Definitions, Policy
  - **App:** Notifications
  - **System:** Daemon
- **Components:** the search uses `ui::field::Field` (rung 1). Category rows reuse the nav row
  shape. The selected row is new (rung 7): an accent wash (accent at 11%) with accent icon and
  text. This is the one place this surface spends the accent.
- **Badges:** Executors shows the same danger count as the footer. Daemon shows a status dot
  (success / warning / danger).
- **Search (new behaviour):** it filters the categories. A category matches when its label or
  its keyword list contains the query. If the selected category is filtered out, the first
  match is selected. With no match the list says "Nothing matches".
- **Back to app:** returns to the screen that was showing before Settings opened (a run, a
  project, Runs, Triggers or Home). With none, it returns to Home.
- **⌘B:** hides the settings navigation like the sidebar; the titlebar toggle brings it back.

## Surface 3 — Settings content

- **Layout:** one scroll column, centred, max 720 px, padding 44 px top. Each page opens with
  a lede: a 22 px semibold title (danger when it states a blocking finding) and a muted line.
  Sections are a 14 px semibold heading, with an optional count and a right-aligned aside,
  over a card: `surface` fill, hairline border, 12 px radius, rows split by hairlines.
- **Components:** cards and card rows are new (rung 7) helpers in `ui::page`. Inside them:
  `kit::pill_button` / `line_button`, `kit::switch`, `kit::tag`, `kit::dot`, `kit::logo`,
  `page::banner`, `page::table_*` (rung 1).
- **Executors page (Job):**
  - Question: can every executor take runs?
  - Finding: "2 executors can't take runs" (danger lede) / "Muse Code is signed out ·
    Cursor needs an install".
  - Primary job: Sign in… / Install, the primary pill on that executor's row.
  - Groups Needs you / Ready / Not installed / Test executors are cards. A row is two lines,
    min 56 px: caret, status dot, logo, then the name over the status line, with controls on
    the right. A row expands in place, inside the card.
  - Check all, Update N and the job status stay beside the lede.
  - Selectors `line-*`, `executor-*`, `group-*`, `check-all`, `update-all` keep their names.
- **Add from registry page:** the existing ACP registry group (search, filter, rows, your own
  agent, keys) moves to its own category.
- **Definitions, Policy, Notifications, Daemon:** the same content as the old tabs, in the
  card language. Daemon facts are label/value rows in one card. The notification switches are
  card rows with their help text.
- **Type & color:** titles 22 / section heads 14 / rows 13.5 / help 12.5. Accent only on primary
  buttons and the selected category. Semantic colours only on status.
- **A11y:** every control keeps the kit's focus ring. Category rows are buttons with the
  selector `settings-<id>`.

## Signature

The sidebar turns into Settings: Back, search and grouped categories in the sidebar's place,
with grouped cards in the canvas.

## Amendments

None during review: the owner locked A as shown.

## Built differently from the draft (2026-11-19)

- **Add from registry:** the page keeps the registry group's existing layout (search, filters, flat
  lines) under a page title, not cards. New copy: the title "Add from the ACP registry" and the
  line "Agents Reno can add and drive, checked before it offers Add." (or, on a daemon without
  the registry, "This daemon has no registry. Update Reno to add agents from it.").
- **Add, then sign in:** an added agent leaves the registry page for Settings › Executors, so the
  toast says where its step is: "… added. One step needs you: sign in, in Settings ›
  Executors."
- **Policy:** no Defaults card; the default answer and the ask timeout stay in the title line,
  as the view-model words them.
- **Search:** the field has no magnifier glyph (the kit `Field` draws none). It is cleared when
  Settings opens from another screen.
- **Back to app:** a run comes back as the very screen that was showing (draft and panel kept).
