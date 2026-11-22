# ROAD-65 bot-computer-spike — notes

Question: can a bot's computer be a Linux VM started with Apple's `container` tool on the Mac,
with a headless browser driven over CDP (accessibility tree, screenshots, screencast), only the
project mounted (not the home directory), and network only through a proxy `renod` controls with a
per-bot domain allowlist (ARCH-19)? It also measures boot time, idle RAM and image size.

Results land in the local spike notes (`docs/internal/spikes/S2-bot-computer.md`, untracked).
Setup convention to evaluate for ROAD-69: an executable `.agents/setup` at the repo root runs once
before the first turn and the machine is snapshotted afterwards; `.agents/prepare` runs per checkout.

## Findings (2026-11-22)

- Install: `brew install container` (1.7.1) and `container system start --enable-kernel-install`; no sudo.
- Cold start to a browser answering over CDP: about 0.75 s; warm restart about 0.6 s. Image 304 MB.
- Idle RAM with Chromium: about 1.2 GiB host RSS per active bot; a bare VM about 374 MiB.
- Accessibility tree 4-6 ms, screenshot 27-54 ms, screencast 47-60 fps: enough for live view.
- `container` mounts nothing by default; mount only the project (`--mount type=bind,...`).
- Egress: an `--internal` network per bot plus a host proxy bound to the gateway IP worked; direct
  routes failed. Open for ROAD-67: host services listening on the gateway or `0.0.0.0` are reachable
  from the bot unless a pf rule (owner action, sudo) blocks them or host services bind to loopback;
  the gateway IP exists only after a container attaches; container IPs change on each start.
