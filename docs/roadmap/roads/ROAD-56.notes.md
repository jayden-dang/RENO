# ROAD-56 bounded-queues-enforced — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

ARCH-11 reads "Every queue is bounded" with no scope, so it covers the app too; the five sites below already break it and nothing checks it. Wording and fixes are settled at `frame-change`.

- The chain is bounded up to the app: `crates/reno-rpc/src/client.rs:12-18` (a consumer 256 behind makes the client drop the subscription and yield `SubEvent::Gap`; the socket reader never blocks), `crates/reno-client/src/mirror.rs:24-36,204`. App pumps that drain into unbounded channels defeat it: the daemon never sees a slow reader and app memory grows.
- Sites: `apps/reno-app/src/ui/terminal_view.rs:496` (PTY: bounded + batch, Gap → re-attach from `after`); `apps/reno-app/src/screens/run/files.rs:411` (watch_fs: latest value / dirty flag); `apps/reno-app/src/store/signin.rs:105` (bounded); `apps/reno-app/src/lifecycle.rs:100` (sync OS callback `Native::on_click` at :82: bounded + `try_send`, drop when full); `crates/reno-harness/src/drivers/opencode/mod.rs:1129` (daemon: bound by in-flight tasks, or oneshot/JoinSet).
- Enforce like Zed's `clippy.toml` `disallowed-methods` (with `reason` and `replacement`): add `futures::channel::mpsc::unbounded`, `tokio::sync::mpsc::unbounded_channel` to Reno's existing `clippy.toml`; count the `#[allow(.., reason = "..")]` tests need.
- `docs/architecture/runtime.md` gets an app paragraph: GPUI tasks owned by an entity field (drop cancels), `.detach()` only for app-lifetime work; bounded or latest-value channels; pumps `send().await` so pressure reaches the daemon. The app's 39 non-test `.detach()` calls are not yet reviewed.
