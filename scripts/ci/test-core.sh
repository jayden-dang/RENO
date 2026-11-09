#!/usr/bin/env bash
# The Linux Rust tests that used to run as separate `cargo test` invocations in
# session-sync-regressions, cursor-compatibility and preview-tests, as ONE
# build and ONE nextest run (separate invocations recompiled the same
# dependency graph with different feature sets).
#
# Same coverage as the old invocations:
#   reno-harness  every test binary (cursor-compatibility ran `-p reno-harness`
#                  plus `--test pi_rpc` with native-fixture; native-fixture only
#                  adds the pi_rpc fixture binaries, so it is on for the whole build)
#   reno-preview  every test binary (preview-tests)
#   reno-doc      lib + persistence and memory regressions + attachments_roundtrip
#   reno-sync, reno-update   lib only (session-sync-regressions)
#   reno-cli, reno-launch, reno-notify   `reno`'s command line, the engine
#                  helper's start-up and the reno:// links, shared by the apps
#   reno-engine   lib + the integration tests named below. Engine has many other
#                  integration binaries (live agents etc.) that no workflow ran, so
#                  they are listed instead of globbed to avoid compiling them.
#   voice (moved from voice-tests.yml offline-linux, which ran them as 14 separate
#   `cargo test` builds): reno-voice-media, reno-voice-session and reno-orb
#                  lib, reno-proto lib `voice` tests, reno-mcp lib, reno-rpc
#                  device_room, engine voice_runtime and device_routing. They need no
#                  audio stack, and harness already depends on all of them.
# nextest does not run doctests; these crates have none (their doc comments
# contain no Rust code blocks).
# Extra arguments are passed to nextest (the nightly job adds `--release`).
# NEXTEST_PROFILE=ci (pull requests and main) leaves out the slow and soak tests
# that scripts/ci/nextest.toml lists; the nightly runs use the default profile.
set -euo pipefail
cd "$(dirname "$0")/../.."

# Test binaries that run nothing on Linux are left out: they would still be
# compiled and linked. Windows-only files (#![cfg(windows)]), and files whose
# every test is #[ignore]d because it needs a live service or a real CLI.
skip=" codex_availability windows_native managed_install muse_account_live pi_live real_acp_lifecycle real_quiet_survey coordinator "
tests=()
for f in crates/harness/tests/*.rs crates/preview/tests/*.rs crates/doc/tests/*.rs; do
  name="$(basename "$f" .rs)"
  [[ "$skip" == *" $name "* ]] || tests+=(--test "$name")
done
# `misc` (harness, engine and preview): the small suites, one binary each.
# The orb geometry tests live in reno-orb's `geometry`.
for t in misc geometry session_publication restart_resume side_chats message_queue voice_runtime device_routing device_room; do
  tests+=(--test "$t")
done

exec cargo nextest run --config-file scripts/ci/nextest.toml --locked --no-fail-fast \
  -p reno-harness -p reno-engine -p reno-sync -p reno-update -p reno-doc -p reno-preview \
  -p reno-mcp -p reno-proto -p reno-rpc -p reno-orb -p reno-voice-session -p reno-voice-media \
  -p reno-cli -p reno-launch -p reno-notify \
  --features reno-harness/native-fixture \
  --lib --bins "${tests[@]}" \
  -E 'not ((package(reno-proto) & not test(voice)) | (package(reno-rpc) & kind(lib)))' "$@"
