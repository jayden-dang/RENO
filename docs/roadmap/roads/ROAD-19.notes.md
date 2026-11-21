# ROAD-19 executor-enable-signin-usage — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Installation leases: each version installs into its own directory and every process using it holds a shared file lock (`AgentInstallationLease`); old versions are collected only when no one holds them: `crates/project/src/agent_server_store.rs` ~1390 (GPL). Matters because Reno auto-updates CLIs while runs are live.
- npx installs take a per-directory async lock and repair half-extracted packages (`find_incomplete_npm_package`, ~1594).
- A registry version change sends on a watch channel and the UI shows "update available" (`reregister_agents` ~349).
- `AuthRequired` is a typed error the UI matches to open sign-in; a `Terminal` auth method runs the login command in a real terminal: `crates/acp_thread/src/connection.rs` ~196-213, ~652.
