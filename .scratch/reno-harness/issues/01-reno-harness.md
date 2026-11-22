# ROAD-62 Bots run on Reno's own harness, with every tool call seen by Reno

Assignee: Jayden Đặng
Status: enhancement, status:shaping
Priority: high
Estimate: 5
Blocked by: none

## Problem

The requester's words: "/shape ROAD-62 --auto".

From the approved vision (GOAL-10): "A bot can run on Reno's own harness, with the owner's API keys and a computer of its own [...] every tool call checked by Reno's policy before it runs, and a full record of what it did. The harness decides when a run is done; nothing caps a run by default." The owner chose Pi as that harness after a spike.

Today a bot can only run on the owner's other agent CLIs, so Reno sees only the tool calls those CLIs choose to ask about, and the Pi runner it already has can only run with full access.

## Outcome

The owner points a bot at Reno's own harness: it runs on the owner's API key, Reno sees and records every tool call before it runs and can refuse it, a run never starts when Reno cannot watch it, and the run goes on until the harness says it is done.

## Acceptance

Not decided yet

## Out of scope

- What the per-bot policy allows or refuses (its own road)
- The bot's computer and its network allowlist (a later milestone)

## Comments

**2026-11-22.** The spec is written and waits for Jayden's review and approval. Five choices need his word: how strict a supervised bot on the new harness should be before the per-bot policy lands, keeping each run's session files for resumes, the executor's name, and two gaps later roads close (per-call usage and the 45-minute cap). Nothing is built yet.
