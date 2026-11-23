# ROAD-59 edit-a-past-prompt-and-regenerate — notes

Split from ROAD-33 on 2026-11-22, while framing it (fork 4 of the frame interview).

ROAD-33 brings in everything the reference agent panel ships. One piece writes rather than reads: the owner clicks a past
prompt, edits it, and the thread restarts from that point. ROAD-33 stays a reading surface, so this piece waits here.

Why it is not in ROAD-33:
- No daemon call exists for it: `crates/reno-proto/src` has no rewind, fork-from-point or regenerate method, so it is a wire
  change (ARCH-13).
- The later runs' journals would change, and ROAD-33 keeps how the daemon keeps run journals off limits.
- Restarting a session from a point is the harness's own loop. Reno does not replace a harness's loop (vision non-goal 1).

Open questions for its frame:
- Which executors expose a rewind or edit-previous-message call over their protocols? Unverified.
- What state is the thread's worktree in at that point? If Reno reset it, that would be a file action of Reno's own, which
  ROAD-33's frame already declined for "discard an interrupted edit".
