# ROAD-61 reno-harness-spike — notes

Question: which existing harness becomes the Reno harness, Pi or dsh? Both are MIT and share one
provider layer. The spike runs the same matrix on each, with a scripted model and no real keys:
- every tool kind reaches Reno's gate before it runs: read, shell, write and edit, MCP, parallel
  and nested calls, subagents;
- fail-closed when the host or the gate is missing, and how the driver checks the gate loaded
  before the first prompt;
- whether the gate can rewrite arguments;
- latency per gated call, RSS and startup;
- how a long run decides to stop.

Results land in the local spike notes (`docs/internal/spikes/S1-*.md`, untracked). The pick is
recorded in ADR-0001's follow-up and in ROAD-62's spec. Pi's driver note "full access only" is wrong
(pivot ledger L23) and is fixed in ROAD-62.

## Answer (2026-11-22)

Pi, recorded in ADR-0002. For ROAD-62: load the gate extension last; arm only after both the gate's
command appears in `get_commands` and a nonce hello arrives on the socket; name Pi's built-in MCP and
codemode back with `-e` when `--no-extensions` is used; the host RPC `bash` command bypasses the hook
and must never be reachable by a model; the gate is not a sandbox (parse shell, resolve paths, guard
the harness's own config and extension directories).
