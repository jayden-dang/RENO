# ROAD-54 replace-mocks-with-real-contracts — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- A connection status published to the UI: Connecting, Connected, ConnectionLost, Reconnecting, ReconnectionError{next_reconnection}, UpgradeRequired; backoff 500 ms doubling to 30 s: `crates/client/src/client.rs` ~87-89, ~278, ~695 (GPL) → `apps/reno-app/src/vm/daemon_restart.rs`, `crates/reno-client/src/mirror.rs`.
- A `version` subcommand and an exit code that means "server not running, spawn it" (`crates/remote/src/proxy.rs`, code 90).
- Every I/O boundary has a real implementation and a fake (`Fs`/`FakeFs`, `HttpClient`/`FakeHttpClient`, `Connection::in_memory`); `crates/http_client` is Apache.
