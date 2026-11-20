# UI brief: Settings pages — General, Appearance, Accounts (ROAD-37)

Status: Locked
Date: 2026-11-19
Screenshot: [2026-11-19-settings-pages-ui-brief.png](./2026-11-19-settings-pages-ui-brief.png)

Amends: [2026-11-19-settings-ui-brief.md](./2026-11-19-settings-ui-brief.md) (its navigation list
gains three pages) and [2026-11-19-app-shell-ui-brief.md](./2026-11-19-app-shell-ui-brief.md) (its
token table becomes the Reno theme, one of two, with three accents).

## Decision

Direction **B "Fewest pages, compact"**, amended once. Three directions were drafted in the app on
the fake daemon: A (a page per topic, themes as tiny windows), B, and C (B with a live preview
beside the Appearance controls). The owner's words: "I think B - that's simple and easy to use",
then the advisor's recommendations on the open decisions, accepted with "apply them", then "lock,
implement it".

## Grounding

- Token source of record: `apps/reno-app/src/ui/theme.rs`. The Reno theme is today's palette,
  unchanged. Slate and the Blue and Violet accents are new values, listed below.
- Type and layout: the settings brief's column (max 720), card language, and scale 22 / 14 /
  13.5 / 12.5.
- Fonts: Geist and Geist Mono.

## Navigation

Settings gains three categories in the locked groups and order:

- **Agents:** Executors, **Accounts**, Add from registry
- **Automation:** Definitions, Policy
- **App:** **General**, **Appearance**, Notifications
- **System:** Daemon

Search keywords per page: General — send, enter, escape, stop, composer, wrap, hidden, files,
agents; Appearance — theme, dark, light, system, mode, accent, colour, reno, slate, motion,
reduce; Accounts — account, usage, plan, limit, session, week, signed in.

## Surfaces

### Appearance

- **Layout:** title "Appearance", muted line "How Reno looks on this Mac.", then one card of four
  rows, label on the left and control on the right:
  1. **Mode:** System / Light / Dark, with the help line "System follows macOS, day and night."
  2. **Theme:** Reno / Slate. Each segment shows its name and a 14 pt split circle: the theme's
     canvas on the left, its muted ink on the right.
  3. **Accent:** three 16 pt dots, Teal, Blue and Violet. The chosen one has a 2 pt ring in its
     own colour.
  4. **Reduce motion:** System / On / Off.
- **Components:**
  - rung 1 — `page::title`, `page::table_frame` and `table_row`, `kit::segmented` and `segment`.
  - new (rung 7) — the accent dot: a colour choice; no kit piece picks a colour.
  - new (rung 7) — the theme's split-circle swatch: no kit piece shows a palette.
- **States:**
  - Every choice applies to the whole app at once and is saved.
  - Mode is the one source of the appearance:
    - System follows macOS.
    - The Toggle theme command (⌘K, the bar's "…" menu) writes Light or Dark into Mode, so the
      page always says what is in force.
    - `RENO_APPEARANCE` stays a launch-only preview that is never saved; it shows only while
      Mode is System.
  - Reduce motion: On and Off pin it, from the first frame of a launch. System will follow
    macOS once gap D11 is built (the app does not read the macOS setting yet, so System means
    motion on, or `RENO_REDUCE_MOTION`). There is no background-pause switch; today animations
    keep running while Reno is in the background (found while building: the motion engine
    starts with background pause off).
- **Type & color:** the swatches and dots spend each theme's own colours; nothing else on the
  page uses the accent except the selected category.
- **A11y:** segments are toggle buttons; each accent dot is a button labelled with its name
  ("Teal accent") and its selected state.

### General

- **Layout:** title "General", muted line "How the composer, the Files tab and the agent list
  behave." Three sections, each a heading over one card:
  1. **Conversation:**
     - "Send messages with": Enter / ⌘ Enter. The help line says the other key: "⇧ Enter
       starts a new line." or "Enter starts a new line; ⌘ Enter sends."
     - "Escape stops the agent": a switch. On, Escape stops a live run exactly when the
       Stop button shows (the run is live and the message box is empty), so a half-typed
       message is never lost to it. A deny note keeps Enter and Escape as they are.
  2. **Files tab:** "Show hidden files" (switch). "Wrap long lines" is not shown: the file
     viewer draws fixed-height, virtualised lines sized to the longest one, and wrapping needs
     a variable-height viewer first (found while building; no control is shown that does
     nothing).
  3. **In the composer:** one row per built-in agent that is installed: logo, name, and a switch.
     A switched-off agent leaves the composer's agent picker. A run already going keeps its
     agent.
- **Components:** rung 1 — `kit::switch`, `kit::segmented` and `segment`, `kit::logo`, the page
  card rows.
- **States:** the defaults are today's behaviour:
  - Enter sends;
  - Escape does not stop a run;
  - dotfiles are listed;
  - every agent is offered.
- This page closes ROAD-34's A42 (send behaviour) and A43 (Escape stops the agent).

### Accounts (worked in)

- **Job:**
  - Question: will any agent run out soon?
  - Finding: the tightest meter in words, as the page title ("Claude Code is 54% into its
    session"), then "It resets in 2h." and who is signed out ("Codex and Cursor are signed out;
    sign in from Executors.").
  - Primary job: none on this page. It shows usage only; accounts are switched in each harness
    (ruling R3), and Executors owns sign-in and its count.
- **Layout:** title is the finding; then one card, a row per account:
  - left: logo, the name over the account (an email, "Signed in" or "Signed out");
  - middle: the plan tag when the provider names one ("ChatGPT Plus");
  - right: the tightest meter (an 80 pt bar, then "Session 54% · resets in 2h").
  - Order: accounts with meters first, then the rest signed in, then the signed out.
  - A signed-out account has no meter and no button.
- **Components:**
  - rung 1 — `kit::logo`, `kit::tag`, the page card rows.
  - new (rung 7) — the meter bar: no kit piece draws a fill fraction.
- **States:**
  - The bar is `accent` below 80% and `warning` from 80%.
  - With no usage anywhere, the title is "No usage to show".
  - With nothing signed in, the page still lists the agents, each "Signed out".
- **A11y:** the bar's label is "Session, 54% used, resets in 2 hours".

## Themes and accents (shared content)

**Default (owner, 2026-11-20): the Vercel theme with the Blue accent.** "I want to use the
color scheme of the vercel - I think is match with me (for default theme)". Its values are
Vercel's Geist tokens, read from vercel.com/geist/colors on 2026-11-20:

| Token | Vercel dark | Vercel light | Geist token |
|---|---|---|---|
| shell | `#000000` | `#fafafa` | background-200 |
| canvas | `#0a0a0a` | `#ffffff` | background-100 |
| card | `#0a0a0a` | `#ffffff` | background-100 |
| surface | `#1a1a1a` | `#ffffff` | gray-100 / background-100 |
| raised | `#292929` | `#ebebeb` | gray-300 / gray-200 |
| text | `#ededed` | `#171717` | gray-1000 |
| muted | `#a1a1a1` | `#4c4c4c` | gray-900 |
| faint | `#7d7d7d` | `#7d7d7d` | gray-800 |
| warning | `#ff9300` | `#aa4d00` | amber-900 |
| danger | `#ff565f` | `#d8001b` | red-900 |
| success | `#00ca50` | `#107d32` | green-900 |

Unlike Reno and Slate, the Vercel theme brings Geist's own semantic colours. The Blue accent is
Geist's blue in every theme: `#47a8ff` dark (blue-900), `#006bff` light (blue-700). The theme
choice lists Vercel, Reno, Slate; the accents list Blue, Teal, Violet. A saved choice is kept:
a file that says `"theme": "reno"` still shows Reno.

The Reno and Slate tables below are unchanged.

| Token | Reno dark | Reno light | Slate dark | Slate light |
|---|---|---|---|---|
| shell | `#080a0c` | `#e6edef` | `#0a0d14` | `#e2e8f0` |
| canvas | `#0b0e11` | `#f4f7f8` | `#0e121b` | `#f1f5f9` |
| card | `#0e1216` | `#ffffff` | `#121828` | `#ffffff` |
| surface | `#141a1f` | `#ffffff` | `#172033` | `#ffffff` |
| raised | `#262c33` | `#d8e1e4` | `#283247` | `#d3dbe6` |
| text | `#dfe5ea` | `#16232b` | `#e2e8f0` | `#0f172a` |
| muted | `#98a3ad` | `#4b5b66` | `#94a3b8` | `#475569` |
| faint | `#6f7a85` | `#6b7b86` | `#6f7d93` | `#64748b` |

Semantic colours and the terminal's ANSI set are the same in both themes.

| Accent | Dark | Light |
|---|---|---|
| Blue (default) | `#47a8ff` | `#006bff` |
| Teal | `#2dd4bf` | `#0f766e` |
| Violet | `#a78bfa` | `#6d28d9` |

Contrast, checked on every theme and mode:
- text on canvas: 14.9–16.3:1;
- muted on canvas: at least 6.5:1;
- faint on canvas: 4.1–4.5:1, the same as today;
- every accent on surface: at least 5.4:1.

## Where each choice is saved

- **`app-settings.json`** (beside the socket, with the notification switches): `mode`,
  `theme`, `accent`, `reduceMotion`, `sendWith`, `escapeStops`, `hiddenFiles`. One store for
  the whole app; the notifier reads its switches from it.
- **`app-composer.json`:** `notOffered`, the agents switched off in the composer.
- **With `--fake`:** both live in memory only, as today.

## Signature

Every page fits in one look: Appearance is four rows, and Accounts states the tightest limit as
its title.

## Amendments (owner, during review)

1. B chosen over A and C ("simple and easy to use").
2. The advisor's recommendations, accepted ("apply them"):
   - Graphite and Pink are cut. Graphite was too close to Reno; Pink sat next to danger.
   - Mode is the single source; Toggle theme writes it.
   - Glass windows and theme import are out.
   - Smooth scrolling and overscroll are out; they need the GPUI fork.
   - Motion is one row in Appearance; the background-pause switch is gone.
   - General's order is Conversation, Files tab, In the composer.
   - The composer switches are real.
   - Every default is today's behaviour.
   - Accounts shows usage only.

## Amendments after the build

- 2026-11-20 (owner): the Vercel theme (Geist tokens) is the default, with Geist blue as the
  default accent. See "Themes and accents".

## Built differently from the lock (2026-11-19)

- No "Wrap long lines" switch yet: the viewer cannot wrap (see General).
- No background-pause switch, and animations keep running in the background as today: the
  advisor's premise that pausing "already happens" was wrong.
- Escape stops a run only when the Stop button shows.

## Not in this brief

- **Archived threads:** waits on ROAD-32's archive.
- **The shared menu, popover and dialog kit (D14):** needs a fresh inventory after slices 3 and
  4.
- **Build items with nothing to pick:** fonts, file-type icons, the tokens page, sounds.
