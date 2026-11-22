# ADR-0003: The Reno harness is its own executor, `reno`

Status: Accepted (the operator approved the ROAD-62 spec sheet at 02afd14, whose Needs-you item 3 read "keep, and accept ADR-0003": "approve", 2026-11-22)
Date: 2026-11-22

The Reno harness runs the same Pi binary as `pi`, but it is a second executor, `reno` (an agent-decided id, pending the owner), served by the Pi driver, rather than `pi` gated in some modes or `pi` turned into the gated harness. That way the owner's `pi` runs keep Pi's own login and permissions (GOAL-8), only a bot that lists `reno` gets the gate, Reno's Keychain key and Reno's policy, and no bot file that lists `pi` changes meaning. The gate reaches the kernel through a private per-run socket into the driver, which raises the same `NeedsApproval` as any harness that asks, so no wire method is added.
