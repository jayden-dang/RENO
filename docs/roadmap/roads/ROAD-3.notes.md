# ROAD-3 terminal-parity-config — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Login-shell environment capture: run `$SHELL -l -i -c 'cd <dir>; <own binary> --printenv'` so direnv and asdf hooks fire, read the result through a dedicated fd so rc noise on stdout cannot corrupt it, special-case csh/fish/nushell: `crates/util/src/shell_env.rs` `capture_unix` ~75 (Apache) → compare with `crates/reno-harness/src/proc/shell_env.rs`.
- MCP servers are passed to an ACP agent in `session/new|load|resume` `mcpServers`: Zed `crates/agent_servers/src/acp/v1.rs` ~10074 (GPL). Reno already does this (`crates/reno-harness/src/drivers/acp/session.rs:142`).
- Git and ssh prompts through the GUI: `GIT_ASKPASS`/`SSH_ASKPASS` point at a script that calls back over a unix socket; with no askpass, `GIT_TERMINAL_PROMPT=0`: `crates/askpass/src/askpass.rs`, `crates/git/src/repository.rs` ~3891-3955 (GPL). Reno sets only `SUDO_ASKPASS=/usr/bin/false` (checked in `proc/shell_runner.rs` only).
