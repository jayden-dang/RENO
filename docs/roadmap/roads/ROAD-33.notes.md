# ROAD-33 transcript — notes

## Frame-time target from ROAD-29 (2026-11-21)

`cargo xtask bench --app` measures a 2,000-message transcript at p50 29.7 ms, p95 31.4 ms per
frame while scrolling. The signed-off target is p95 ≤ 16.7 ms (one 60 Hz frame), and the bench
reports "over (ROAD-33)" until this item lands (`docs/BENCHMARKS.md`, App).

Corrected 2026-11-22. This was read from the code, not profiled:
- The scenario is one turn: 2,000 × (text, tool step).
- The bench opens every fold (`RENO_OPEN_FOLDS=1`).
- The open "Worked for" fold renders all ~4,000 child rows inside one list item
  (`screens/run/transcript.rs`, `worked()`).
- The list skips whole items, never the children of one item, so every frame lays them all out.

So the bench is evidence for one list item per entry, with the children of an open fold as items
of their own. It is not evidence for one row per markdown block. No bench measures a single very
long reply yet.

## Frame, 2026-11-22

The owner's card is [ROAD-33](ROAD-33.md). The frame record is on the ticket.
