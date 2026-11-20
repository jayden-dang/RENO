# UI brief: first run, the connecting gate, restarting renod (ROAD-38)

Status: Locked
Date: 2026-11-20
Screenshot: [2026-11-20-first-run-ui-brief.png](./2026-11-20-first-run-ui-brief.png)
Inherits: [2026-11-19-app-shell-ui-brief.md](./2026-11-19-app-shell-ui-brief.md) (status bar, tokens, type) and [2026-11-20-composer-ui-brief.md](./2026-11-20-composer-ui-brief.md) (places, clone).

## Decision

Direction **A "Wizard"**, amended: the first-project step imports a folder from this Mac. Two directions were dropped: B "Checklist" (a setup card on Home, a skeleton gate and a sidebar restart banner) and C "One page" (a setup page, a checklist splash and restart inside the renod popover).

The owner's words: "I want A is better, and the first project should be import from folder, or import from computer is better", then "chốt - implement nó".

## Grounding

The token source of record is the app's `Theme`; no new tokens were added. Typefaces: Geist for step titles (24 semibold) and body text (14); Geist Mono for versions. Icons: Phosphor plus the executor logos.

## Job (worked-in screen)

- **Question:** can I start working, and if not, what is missing?
- **Finding:** computed from the executors. Example: "3 of 5 agents can take runs on this Mac: Claude Code, Codex and Pi. Grok needs signing in; Cursor is not installed." The order is by what needs the person most: sign-ins first, then installs.
- **Primary job:** fix what blocks the first run (Sign in or Install on the agent named), bring in a first project, or skip and start.

## Surfaces

### Setup wizard (first run only)

- **When it shows:** only when no settings file exists. A settings file written before setup existed counts as done, so a person already using the app never sees it.
- **Layout:** a full-window centred flow with progress dots, plus Back / Continue and "Skip setup" at the bottom left. The steps are:
  1. **Welcome:** the mark, "Welcome to Reno", one line on what Reno does, then the finding sentence and a row for each blocked agent with its state and one action (Sign in, Install).
  2. **Look:** mode, theme and accent, using the existing Settings › Appearance prefs.
  3. **Agents:** every agent with its version, state and action.
  4. **Bring in your first project:**
     - "Pick a folder on this Mac. Your first thread starts there."
     - Primary: **Import from this Mac…**, which opens the native folder panel (directories only, single selection). Secondary: **Clone from URL…**.
     - "Or pick one Reno already knows" appears only when projects are known.
     - After a pick, one line says what Reno found: "reno · a git repository on main · runs get their own worktree", or "notes · not a git repository · runs work in the folder itself".
  5. **Done:** "You're all set", then a summary of Look, Agents and First project, and **Start building**.
- **States:**
  - Skip ends setup at any step, leaves nothing half-done, and setup never shows again.
  - An install whose recipe needs review ends setup and opens Settings › Executors on that agent's review.
  - Import errors show inline: "That is not a folder", "Reno cannot read that folder". Clone failure: "Could not clone: repository not found".
  - On the real daemon there is no add-project path yet. The project step is left out and the wizard has 4 steps (4 dots).
- **Components:**
  - rung 1: `kit` buttons, tags, `focus_ring`; the executor actions (sign-in, install jobs); the places clone.
  - new (rung 7): the step frame with progress dots, because no kit piece is a full-window flow; `folder_picker.rs`, the single seam for the native folder panel.
- **A11y:** steps are announced; Back and Continue are keyboard reachable; the found line is text, not an icon.

### Connecting gate

- **Layout:** the centred mark and one line, "Connecting to renod…" (it reads "Loading your threads…" once connected). The status bar paints at once with its "connecting" pill.
- **Behaviour:**
  - It never delays the first paint and has no minimum time.
  - It lifts when the store reports connected, then fades over 500 ms.
  - An offline daemon is never hidden behind it: offline shows the existing offline reasons.

### Restart renod

- **Pill:** "Restart renod" in the status bar next to "● renod", in the warning tint. It shows only when connected and when the installed version is newer than the running one.
- **Confirm:** a popover reading "Restart renod 0.4.1 → 0.4.2?", then "N live runs keep going; the app reconnects", with Not now / Restart.
- **States:** while restarting, the pill shows a spinner. On success the pill goes away. On failure the popover says "renod did not come back; run `reno daemon start`".

### Shared items (always on)

- **Agent updates island:** each agent has **Skip this version**. A skipped version stays hidden until a newer one appears. It is stored as the app-local pref `skippedUpdates`.
- **Row menu:** **Copy link** copies `reno://t/<thread id>`. `open_link(url)` focuses that thread. The URL scheme is not registered yet, because that needs the app bundle, so a copied link cannot yet be opened from outside the app.
- **⌘K:** archived threads are absent from the thread lists.

### Real daemon vs. not-yet controls

| Daemon | What it shows |
|---|---|
| Real (`daemon_restart` false, no `place_desk`) | No restart pill. No project step. Setup still shows on a true first run. |
| Fake (`--fake`) | Everything. |

## Contract

`reno_client`, mocked by the fake daemon.

- **`DaemonDesk`:**
  - `running_version()`, `installed_version()`, `versions()`
  - `restart_duration_ms()`
  - `restart() -> Result<String, RestartError>`
    - `RestartError::UpToDate`: "renod is already the installed version"
    - `RestartError::DidNotComeBack`: "renod did not come back; run `reno daemon start`"
  - The real version needs a daemon method, or `daemon.shutdown` plus a launchd relaunch, and the CLI route `reno daemon restart` (ARCH-2).
- **`PlaceDesk` additions:**
  - `add_project(path) -> Result<Project, AddProjectError::NotAFolder | NotReadable>`
  - `project_facts(path) -> ProjectFacts { git, branch }`

## Signature

A guided first five minutes, one thing per screen, and the first screen already says what blocks you, with the fix beside it.

## Amendments

1. The owner chose A.
2. The first-project step imports a folder from this Mac ("Import from this Mac…"), with Clone from URL as the secondary action and known projects listed only when they exist.
3. Agent-applied at lock:
   - the project step is left out on the real daemon until `add_project` exists;
   - the gate has no minimum time;
   - an existing settings file counts as onboarded.

## Code and tests

- **Code:**
  - `apps/reno-app/src/ui/setup/{mod,flow,parts,wizard}.rs`
  - `ui/gate.rs`, `ui/restart.rs`
  - `vm/{first_run,daemon_restart,thread_link}.rs`
  - `folder_picker.rs`
  - `crates/reno-client/src/daemon_desk.rs`, plus `places.rs` and its fakes
  - Switches: the `--fake` mode, `Capabilities.daemon_restart`, and the app-local prefs `onboarded` and `skippedUpdates`.
- **Lock tests:**
  - `apps/reno-app/src/setup_tests.rs`, about 21 tests, each seen failing once against a broken guard;
  - the `onboarded` rule in `prefs.rs`.
