# RENO

A personal life-management hub. RENO doesn't rebuild your calendar, reminders or task apps: it connects to the ones you already use (Google Calendar, Apple Calendar and Reminders, and more over time) and lets AI agents such as Claude Code and Codex plan your day, week and month across all of them, in one place.

Under the hood RENO is a native Rust app (gpui UI) with a background engine that runs agents locally by default, plus optional multi-device sync and iOS/Android apps.

> Status: early. The codebase currently carries the agent-controller foundation it was derived from; the life-management layer (sources, planner, schedules, Today/Week/Month views) is being built on top of it.

## Building

```bash
cargo run -p reno --bin reno               # headed app (starts its background engine)
cargo run -p reno --bin reno -- headless   # engine only
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for how the engine, sync and UI fit together.

## Before a first release

Account-bound values are placeholders and must be replaced with your own:

| Placeholder | Where |
| --- | --- |
| `reno.example` | domains: edge, installer, updater, links |
| `TODO_RENO_WORKOS_CLIENT_ID`, `TODO_RENO_WORKOS_STAGING_CLIENT_ID` | `crates/cli/src/env.rs`, `crates/client/src/auth.rs`, iOS sign-in, `edge/wrangler*.jsonc` |
| `TODO_RENO_CLOUDFLARE_ACCOUNT_ID` | `edge/wrangler*.jsonc`, `edge/vault/wrangler.jsonc` |
| `TODO_RENO_APPLE_TEAM_ID` | `edge/src/env.ts` (APNs), `.github/workflows/testflight.yml`; iOS `DEVELOPMENT_TEAM` is empty |

## License

MIT — see [LICENSE](LICENSE) and [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

### Upstream notice

RENO contains substantial portions of software distributed under the following license:

```
MIT License

Copyright (c) 2026 Wing

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
