# ROAD-50 evaluations — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- `eval(iterations, expected_pass_ratio, ..)`: run a case N times (the first alone to warm caches, then up to 32 in parallel) and fail below the threshold: `crates/eval_utils/src/eval_utils.rs` (GPL) → `crates/reno-kernel/`.
- A headless runner on the real pipeline with exit codes 0 / 1 / 2 timeout / 3 interrupted, writing `result.json`, `thread.md`, `thread.json`: `crates/eval_cli` (GPL).
- Judge model configured apart from the agent model (`ZED_AGENT_MODEL`, `ZED_JUDGE_MODEL`); a judge template with per-case assertions (`crates/agent/src/tools/evals/`).
