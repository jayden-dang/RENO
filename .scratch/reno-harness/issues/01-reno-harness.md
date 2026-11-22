# ROAD-62 [RENOHARNESS] Bots run on Reno's own harness, with every tool call seen by Reno

Assignee: Jayden Đặng
Status: enhancement, status:building
Priority: high
Estimate: 5
Blocked by: none

## Problem

The requester's words: "/shape ROAD-62 --auto".

From the approved vision (GOAL-10): "A bot can run on Reno's own harness, with the owner's API keys and a computer of its own [...] every tool call checked by Reno's policy before it runs, and a full record of what it did. The harness decides when a run is done; nothing caps a run by default." The owner chose Pi as that harness after a spike.

Today a bot can only run on the owner's other agent CLIs, so Reno sees only the tool calls those CLIs choose to ask about, and the Pi runner it already has can only run with full access.

## Outcome

The owner lists the Reno harness, `reno`, in a bot. Its runs go to the installed Pi:
- on the API key the owner stored in Reno, never Pi's own login;
- with every tool call held until Reno's policy answers it, and recorded with its arguments before it runs.

A run never starts when Reno cannot confirm it is watching, and it goes on until Pi says it is done. The owner's own `pi` executor stays as it is.

## Acceptance

- [ ] A bot that lists `reno`, with a provider key stored in Reno under the provider's variable name, runs on Pi until Pi settles, with no limit added by Reno.
- [ ] Every tool call such a run makes is recorded with its arguments and Reno's decision before it runs. This covers file reads and edits, commands, MCP tools, several calls at once and nested script calls.
- [ ] A call the policy refuses never runs, and the bot is told why.
- [ ] A run whose gate cannot be confirmed live fails before its first prompt, with the reason, and never falls back to the ungated `pi`.
- [ ] When Reno cannot answer a call (its gate or the daemon is gone, or no answer comes), the call does not run, and a lost gate connection ends the run.
- [ ] The run uses only the bot's own key from Reno's Keychain items. It never uses Pi's own login, the daemon's environment, or cloud credential files in the home folder. A credential file planted in the run ends it.
- [ ] A bot whose secrets give Pi no usable model fails before any prompt, with a reason naming the bot's secret names and the variable names Pi reads.
- [ ] Neither an edit nor a recognised command in the run can change its gate, its own files or Reno's protected folders.
- [ ] A run resumed after a daemon restart continues the same Pi session. A run that succeeded or was cancelled leaves no session files behind.
- [ ] A bot that inherits the owner's own harness config, or asks for auto-accept edits, is not routed to `reno`, and the reason is named.
- [ ] The owner's `pi` executor and every other executor work exactly as before.
- [ ] The README no longer says Reno never runs a model, and says it has a harness of its own for bots on your API keys.

## Out of scope

- What the per-bot policy allows, asks or refuses, and per-bot tool and domain lists (ROAD-63). Until then, a supervised `reno` bot asks about every unlisted call.
- The default run time limit (45 minutes still applies) and cost or per-call usage display (ROAD-64).
- The bot's computer, its VM, network allowlist and browser (MILE-12).
- Runs the owner starts on other harnesses deciding their own permissions (ROAD-4), and harness questions in general (ROAD-1).
- Rewriting a call's arguments, auto-accept edits on `reno`, any new screen, and any change to the daemon's wire protocol or database.
- Writes the built-in protection cannot recognise (for example from a script), the provider key being readable from the run's environment, and secrets in Pi's own session files (accepted risks, named in the spec).

Requirements covered: RENOHARNESS-1.1–1.7, 2.1–2.9, 3.1–3.8, 4.1–4.4, 5.1–5.5, 6.1–6.6, 7.1–7.5, 8.1–8.5, 9.1–9.3, 10.1–10.10, 11.1–11.6
Plan: docs/specs/2026-11-22-reno-harness/tasks.md
Roadmap: ROAD-62 (MILE-11)

## Comments

**2026-11-22.** The spec is written and waits for Jayden's review and approval. Five choices need his word: how strict a supervised bot on the new harness should be before the per-bot policy lands, keeping each run's session files for resumes, the executor's name, and two gaps later roads close (per-call usage and the 45-minute cap). Nothing is built yet.

**2026-11-22.** Jayden approved the spec; it is ready to build.

**2026-11-22.** Building on `feat/reno-harness` (/go run).
