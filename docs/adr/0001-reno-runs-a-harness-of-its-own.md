# ADR-0001: Reno runs a harness of its own for bots

Status: Accepted
Date: 2026-11-22

Reno only drove the owner's CLIs and added no permission layer, so a bot's guarantees were only as strong as the CLI it ran on and a bot had no computer of its own (README "It never runs a model itself"; vision GOAL-2 as of 2026-11-19). Reno now runs one harness of its own for bots: an existing agent harness (Pi or dsh, picked by a spike) that it drives with the owner's API keys and gates so every tool call passes Reno's policy before it runs, fail-closed (ARCH-18), with a computer per bot whose network goes only through `renod`'s allowlist (ARCH-19); owner-started runs on other harnesses still follow those harnesses' own permissions (GOAL-11), and no run is capped by default. Writing a loop from scratch was rejected as duplicate work behind the same gate, driving only CLIs as unable to enforce per-tool-call control or provide a computer, and adopting a whole third-party agent platform as giving up local-first and per-bot governance; see `docs/product/pivot-ledger.md`.
