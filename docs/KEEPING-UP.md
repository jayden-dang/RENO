# Keeping up with the agent CLIs

Reno drives Claude Code, Codex, Pi and the ACP agents (Grok, Devin, Hermes and the registry's) through
their own command-line programs. Those programs update on their own schedule and Reno's drivers
read their wire formats directly, so the drivers can fall behind without any test of ours
noticing. Four things tell us when they have; this page is what to do about each, and the weekly
checklist that ties them together.

| Signal | What it catches | Where it shows |
|---|---|---|
| The live conformance suite | A behaviour the CLI no longer has (an approval that is not asked, a resume that forks, a sandbox that lets a write through) | `<data dir>/conformance/YYYY-MM-DD.{json,md}`, `cargo xtask conformance-live` |
| Unrecognised-frame counters | A new kind of message the driver drops because it has never seen it | `reno executor frames`, the daemon log, the live report |
| The Codex schema snapshot | A protocol message or field Codex added, removed or changed | `cargo xtask codex-schema-diff`, `cargo test -p reno-harness --test codex_schema` |
| The version gate | A CLI newer than the one a driver was verified on | `reno executor list` says `untested`; the live report prints the change that clears it |

`ExecutorDescriptor::cli_verified` is the claim a driver makes: the oldest CLI it supports (`min`)
and the newest one a green live run covered (`verified`). A CLI above `verified` still runs, and
is shown as `untested`. Nothing raises `verified` by itself: a person does, after a green live run
(see "Bump `cli_verified`").

Pi (`pi --mode rpc`, `cli_verified` 1.5.0) is covered by the unrecognised-frame counters and the
version gate, but not by the live conformance suite, which needs a paid model. A newer Pi runs and
shows `untested`. To clear it, read the Pi changelog for RPC changes, then run the live Pi test,
which plays a whole run against Pi's own mock provider in a throwaway agent directory (no model,
no network, nothing under `~/.pi`):

```
RENO_PI_LIVE=1 cargo test -p reno-harness --test pi -- --ignored --nocapture
```

and raise `VERIFIED_CLI_VERSION` in `crates/reno-harness/src/drivers/pi/mod.rs` by hand.

## The weekly checklist

Once a week, on a Monday say, with the CLIs up to date on the machine that runs the live suite:

1. **Read the last nightly reports.** `<data dir>/conformance/` (on macOS
   `~/Library/Application Support/Reno/conformance/`) holds one dated Markdown and JSON file per
   night. The newest `.md` opens with the verdict. Any regression in the week (a failing case, a
   new waiver, a new unrecognised-frame kind) is the first thing to look at. If the nightly job
   is not installed, run `cargo xtask conformance-live` by hand.
2. **Check the CLI releases.** For each CLI, compare the installed version
   (`reno executor list`, or the CLI's own `--version`) with the latest release and read its
   changelog for: changes to permission and approval prompts, the sandbox, resume and session
   ids, MCP, and the streaming output format. Update the CLIs, then run
   `reno executor refresh` so the daemon looks again.
3. **Look at the unrecognised frames.** `reno executor frames` lists, per executor, each kind of
   message the driver did not recognise since the daemon started, how often, and when it was
   first seen under the current CLI version (the counts start again when the CLI version changes). The daemon also logs one warning the first time it
   sees each kind per CLI version. A kind that appears after a CLI update is a new message: decide
   whether the driver should handle it (add it to the wire layer, with a test) or drop it on
   purpose (add it to the list of frames the driver knows and ignores). Do not just clear the
   count.
4. **Regenerate the Codex schema and diff it** against the snapshot (next section).
5. **Run the live suite** (`cargo xtask conformance-live`, or `RENO_CONFORMANCE_LIVE=claude-code,codex
   cargo test -p reno-harness --test conformance_live -- --ignored --nocapture`). It makes
   about a dozen short model runs per CLI at the cheapest model and prints its estimate first;
   each case is capped at 120 s and each CLI at 900 s.
6. **If it is green on a newer CLI, bump `cli_verified`** (below), with the schema snapshot when
   it is Codex.
7. **If it is red, do not bump.** Fix the driver, or, when the CLI took away something Reno
   relied on, say so in the descriptor (never claim more than was verified; see ARCH-15 in
   `docs/INVARIANTS.md`) and narrow what the driver declares until it is fixed.

## Regenerate the Codex schema

The snapshot in `crates/reno-harness/tests/fixtures/codex/schema/` is what `codex app-server
generate-json-schema` produced for the version in its `VERSION` file. It keeps only what the driver
uses: the four union files (`ClientRequest`, `ClientNotification`, `ServerRequest`,
`ServerNotification`) and the replies the driver sends.

```
codex app-server generate-json-schema --out /tmp/codex-schema
cargo xtask codex-schema-diff /tmp/codex-schema
```

The diff ignores doc strings. It lists methods and definitions that are new, gone or changed, and
any other difference by JSON path, and exits 1 when anything differs. It reads files; it does not
run Codex and never touches the snapshot.

- **No difference:** nothing to do. If the CLI version is newer, you may still bump `VERSION`
  together with `cli_verified` once the live suite is green.
- **A difference:** first see whether the driver still works against the new schema without
  changing the snapshot:

  ```
  RENO_CODEX_SCHEMA_DIR=/tmp/codex-schema cargo test -p reno-harness --test codex_schema
  ```

  A failure there is a real mismatch (a field the driver sends or reads changed shape): fix the
  driver. When it passes, copy the changed files from `/tmp/codex-schema` over the snapshot (only
  the files the snapshot already holds), set the version in `VERSION`, and run the schema test
  again with no override. A *new* server notification or request that the driver ignores also
  shows up in `reno executor frames` the first time a run meets it; deciding what to do with it is
  step 3 of the checklist.

The schema check says nothing about behaviour: that is the live suite's job.

## Bump `cli_verified`

Only after a green live run on the new version, and for Codex only after the schema step. A green
run on a CLI newer than `cli_verified.verified` makes the live report print the exact change, for
example:

```
crates/reno-harness/src/drivers/codex/mod.rs: pub const VERIFIED_CLI_VERSION: &str = "0.169.0";  ->  "0.170.0";
```

Make that one-line change by hand (the tool never edits code), and for Codex set the same version
in `crates/reno-harness/tests/fixtures/codex/schema/VERSION`. Then run `cargo test --workspace`.
Do not touch `MIN_CLI_VERSION` unless the driver really stopped working on older CLIs.

## The nightly job

`cargo xtask conformance-live` runs the live suite for the CLIs that are installed and signed in
(`--clis claude-code,codex` insists on both), writes `<data dir>/conformance/YYYY-MM-DD.json` and
`.md`, compares with the latest earlier report, prints the Markdown and exits 1 on a regression.
The data directory is the daemon's (`RENO_DATA_DIR` or `--data-dir` override it); the date is the machine's local one.

It is a regression, and the exit code is non-zero, when:

- a case fails (new or still failing);
- a case is newly waived: it passed or was skipped the night before and is waived now, so a claim
  lost its proof;
- a case that passed no longer runs (skipped);
- an unrecognised-frame kind appears that the previous report did not have (with no previous
  report the counters are only noted);
- a CLI that was usable the night before is not installed or not signed in now.

A CLI version change, a count that rose for a kind already known, a waiver that was already there
and a CLI that was not usable the night before either are notes in the report, not regressions.

`cargo xtask conformance-live --print-plist [--hour H --minute M]` prints a LaunchAgent plist for
a nightly run (03:17 by default) and nothing else: it does not write the file or call `launchctl`.
The steps to install it are printed next to it. A nightly run costs model runs, so install it on
purpose.

## What the live suite proves, and what it does not

Beyond a plain tool-using turn, a steer, a cancel and a resume, the live suite runs:

- an approval that is allowed and one that is denied under `Supervised`;
- under `AutoAcceptEdits`, an edit that is accepted unasked and a shell command that asks (and a
  denial that leaves no trace);
- on Codex, a write under `ReadOnly` that must not land, and under `WorkspaceWrite` a write inside
  the worktree that lands next to one outside it that must not;
- an MCP call: a tiny MCP server built with the test (one tool that writes a marker file) is
  injected and the model is asked to call it.

It reports as `waived`, which is an unverified claim and never a pass: a deny message reaching the
model, a harness crash, an unrecognised frame (a real CLI cannot be asked to send one) and, on
Codex, a question. A waiver that appears where there was none is a regression.

## Recording new fixtures

The captured transcripts under `crates/reno-harness/tests/fixtures/{claude,codex}/` are named
`live-<cli version>-<what it shows>.jsonl` (other files there are hand-written to a known shape
and say so). Record one when a CLI starts doing
something the fakes do not (a new kind of message, a new way an approval arrives):

1. Run the scenario against the real CLI in a scratch repository, with the frames recorded. For
   Codex, `tests/fixtures/record-wire.sh` is a drop-in program that logs both directions of the
   JSON-RPC stream to `in.jsonl` and `out.jsonl` (`RENO_WIRE_TARGET` names the real `codex`,
   `RENO_WIRE_LOG` the directory). For Claude Code, keep the lines of its streaming output.
2. Remove what is personal or secret: account names and addresses, tokens, absolute paths and
   anything the model wrote that should not be published. Keep the shape of every frame.
3. Save the cleaned lines under the name above, and add a test next to the driver's other tests
   that replays it and asserts the events the driver produces. For Codex, the schema test also
   validates the file against the snapshot.
4. If a fake CLI should reproduce the behaviour, teach it the scenario, so the suite catches a
   regression without a model.

A captured fixture is evidence of what the CLI did at that version, not a contract: when a newer CLI
changes, add a new fixture beside the old one and keep both while the driver supports both.
