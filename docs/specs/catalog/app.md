# App catalog

Recognized feature cards for this domain. Add a row **before** writing
`requirements.md` for a new feature. Status is one of
`Draft | Approved | In-progress | Implemented | Shipped | Recognized`.
`Owner` is the team or person a PR that changes this feature's spec or surface should
notify (`@org/team`). A blank cell falls back to the `Owner:` line below; no owner means
nobody is notified.

| Code | Feature | Spec | Status | Owner | Roadmap item | Match terms | Surface roots |
|---|---|---|---|---|---|---|---|
| APPBENCH | App budgets and profiling | — | Implemented |  | ROAD-29 | bench, budgets, frame time, idle wakeups | `xtask/src/bench.rs`, `docs/BENCHMARKS.md` |
| APPSHELL | App shell: rail, sidebar, canvas, conversation | — | Implemented |  | ROAD-30 | shell, rail, canvas, logo | `apps/reno-app/src/app.rs`, `apps/reno-app/assets/app-icon/` |
| COMPOSER | Composer: place sentence, completions, queue | — | Implemented |  | ROAD-31 | composer, completions, queue, place | `apps/reno-app/src/ui/composer/` |
| SIDEBAR | Sidebar: thread menu, Pinned, Archived, resize | — | Implemented |  | ROAD-32 | sidebar, pinned, archived, thread menu | `apps/reno-app/src/ui/sidebar/` |
| PALETTE | ⌘K command palette | — | Implemented |  | ROAD-34 | palette, command, ⌘K | `apps/reno-app/src/ui/palette.rs`, `apps/reno-app/src/vm/palette.rs` |
| RUNACTIVITY | Following a run: plan and background trays, subagent chips | — | Implemented |  | ROAD-35 | run activity, plan tray, subagent | `apps/reno-app/src/panel/`, `apps/reno-app/src/vm/activity.rs` |
| SETTINGS | Settings: General, Appearance, Accounts | — | In-progress |  | ROAD-37 | settings, appearance, accounts | `apps/reno-app/src/screens/settings_pages.rs`, `apps/reno-app/src/prefs.rs` |
| FIRSTRUN | First run: setup wizard, folder import, connecting gate | — | Implemented |  | ROAD-38 | first run, setup, wizard, onboarding | `apps/reno-app/src/ui/setup/` |
| MOCKDESK | Mocked daemon contracts (`--fake`) | — | In-progress |  | ROAD-52 | fake, mock, contract, desk | `crates/reno-client/src/fake/` |
| RENOHARNESS | Reno harness: a bot runs on gated Pi with the owner's API key, every tool call through Reno's policy before it runs | ./2026-11-22-reno-harness/ | Implemented |  | ROAD-62 | reno harness, gate, tool call, arming, nonce, executor, api key | `crates/reno-harness/src/drivers/pi/`, `crates/reno-kernel/src/supervisor/approval.rs`, `apps/renod/src/executors.rs` |
