#!/bin/sh
# Runs a stdio JSON-RPC program (a fake app-server) and records both directions, one frame per
# line: what the driver wrote to it in $RENO_WIRE_LOG/in.jsonl and what it wrote back in
# out.jsonl. The wire-schema tests replay those files against the schema the real CLI emits.
#
#   RENO_WIRE_TARGET=<program> RENO_WIRE_LOG=<dir> record-wire.sh [args...]
#
# Both are read at launch, so the script is a drop-in `codex` for a driver whose environment
# carries them.
: "${RENO_WIRE_TARGET:?}" "${RENO_WIRE_LOG:?}"
tee -a "$RENO_WIRE_LOG/in.jsonl" | "$RENO_WIRE_TARGET" "$@" | tee -a "$RENO_WIRE_LOG/out.jsonl"
