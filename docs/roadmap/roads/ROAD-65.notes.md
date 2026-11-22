# ROAD-65 bot-computer-spike — notes

Question: can a bot's computer be a Linux VM started with Apple's `container` tool on the Mac,
with a headless browser driven over CDP (accessibility tree, screenshots, screencast), only the
project mounted (not the home directory), and network only through a proxy `renod` controls with a
per-bot domain allowlist (ARCH-19)? It also measures boot time, idle RAM and image size.

Results land in the local spike notes (`docs/internal/spikes/S2-bot-computer.md`, untracked).
Setup convention to evaluate for ROAD-69: an executable `.agents/setup` at the repo root runs once
before the first turn and the machine is snapshotted afterwards; `.agents/prepare` runs per checkout.
