# ROAD-51 gated-definition-improvement — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Baseline of record per (experiment, model), promoted only from a clean commit reachable from main, with history of superseded ones (`zed_eval/baseline.py`, outside the Rust workspace).
- `rejudge` reruns only the judge on a finished run; every run keeps its patch and build info so it can be reproduced.
- Maps to: a definition change is replayed N times against the baseline for the same task and harness+model, and reverts when its pass ratio drops (see ROAD-50 notes).
