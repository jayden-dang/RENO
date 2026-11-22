# Pivot ledger: Reno's own harness for bots

Status: Confirmed
Date: 2026-11-22

## The new direction (owner decisions, 2026-11-22)

- **D1** Reno gets its own harness for bots: an LLM loop run by Reno with the owner's API keys.
  Which harness (Pi or dsh) is decided by a spike.
- **D2** Whose permissions decide:
  - A run the owner starts on a harness: that harness decides, like a terminal (GOAL-8).
  - A bot on Reno's own harness: Reno's policy *is* that harness's permission system, and every
    tool call passes it before it runs.
  - An unattended bot on another harness: isolated from the owner's config, and trusted no further
    than its executor declares (ARCH-15).
- **D4** Each bot can have a computer: a browser first, in a Linux VM on the Mac. Its network
  goes only through a per-bot domain allowlist enforced by `renod`.
- **D5** Reno's own harness uses API keys, not subscription logins.
- **Limits:** no dollar budget and no per-run usage cap. The harness decides when to stop, so long
  runs are possible. Usage is shown, never capped by default.

## Disposition ledger

Dispositions were proposed by the agent. The owner confirmed every row on 2026-11-22: "xác nhận tất cả".

How the confirmed rows land in the vision: GOAL-2 is cited by the roadmap, so it is struck and replaced by GOAL-11. GOAL-6 and GOAL-7 already hold as written (API keys are a login the owner has; Reno's own harness is a harness), so their clarifications go into the scope boundaries and their text is unchanged.

| # | Item | Today | Proposed disposition | Entrenchment |
|---|---|---|---|---|
| L1 | **GOAL-1** routing with reasons and fallback | Shipped (router) | **Keep.** Reno's own harness is one more executor in a bot's list. | Shipped code |
| L2 | **GOAL-2** one inbox; "Reno adds no permission layer of its own" | Approved 2026-11-19 | **Adapt.** Keep the one inbox. The no-layer rule holds for runs on other harnesses; on Reno's own harness, Reno's policy is the harness's permission system (D2). | Vision only; the policy engine is still shipped |
| L3 | **GOAL-3** bots in parallel worktrees, boards, a flow survives restart | Planned | **Keep** | None |
| L4 | **GOAL-4** checked work: tests, a verifier on another harness, the owner's verdict | Planned | **Keep.** The verifier may run on Reno's harness. | None |
| L5 | **GOAL-5** gated definition improvement | Planned | **Keep** | None |
| L6 | **GOAL-6** local; logins the owner already has; no Reno account; no cloud service | Approved | **Adapt.** Add: Reno's own harness calls model providers with the owner's API keys, kept in the Keychain as Reno's own items. Still no Reno account and no Reno cloud service. Never touching a *harness's* auth is unchanged. | Vision; Keychain secrets shipped (T1.13) |
| L7 | **GOAL-7** a lead hands work to harnesses; the lead can be any harness | Approved | **Adapt.** The lead can also be a bot on Reno's own harness. | Vision only |
| L8 | **GOAL-8** owner-started runs match the terminal | Approved | **Keep.** Scoped to external harnesses; Reno's own harness has no terminal twin. | Vision; drivers partly shipped |
| L9 | **GOAL-9** bots by chat, applied as an accepted diff | Planned | **Keep** | None |
| L10 | *New goal*: a bot on Reno's own harness, with its own computer and deep per-bot control (domain and tool allowlists, every tool call through policy, full audit) | — | **Add** as GOAL-10 | None |
| L11 | Non-goal "Reno does not replace a harness's own loop, tools or permission system" | Approved | **Adapt.** Reno never wraps or overrides a harness it drives; it adds one harness of its own. | Vision only |
| L12 | Non-goal "No browser or computer-use tooling of Reno's own" | Approved | **Retire.** Reversed by D4. | Vision only |
| L13 | Non-goal "No multi-device sync, cloud execution, mobile, voice, plugin marketplace" | Approved | **Keep.** The bot's VM runs on the owner's Mac. | Vision only |
| L14 | Scope "Reno adds no permission layer; guards only its definitions repo and data dir" | Approved | **Adapt** to D2. Guarded paths unchanged. | Vision; policy engine shipped |
| L15 | Scope "unattended use under subscription terms is open; API keys are the documented path" | Approved | **Adapt.** Reno's own harness always uses API keys (D5). The open question stays for other harnesses. | Vision only |
| L16 | Scope "how the GOAL-7 lead is built stays open until a spike" | Approved | **Adapt.** Reno has its own harness. The spike picks Pi or dsh; the lead may be a bot on it. | Vision; ROAD-25 |
| L17 | Scope "kernel keeps no macOS-only code outside `renod` lifecycle and the Keychain" | Approved | **Keep.** The VM backend and the egress proxy live in `renod` behind ports; the kernel stays portable. | Architecture |
| L18 | README: "It never runs a model itself." | Shipped (public README) | **Retire.** Rewrite: Reno runs your CLIs, and has a harness of its own for bots that uses your API keys. | Public text |
| L19 | Shipped policy engine, approval ladder, standing rules (`supervisor/approval.rs`, `policy.toml`) | Shipped in Phase 1. The 2026-11-19 direction was to remove it. | **Keep**, rescoped to Reno's own harness and unattended runs (D2). Removal for owner-started runs on other harnesses stays with ROAD-4. | Shipped code and data schema |
| L20 | Bot `max_wall_time` default `45m` (`crates/reno-proto/src/defs/bot.rs:155,173`) | Shipped default | **Adapt.** No wall-time limit by default; the field stays optional. | Shipped schema; changing the default changes behaviour |
| L21 | Bot `daily_token_ceiling` (optional, unset by default) | Shipped | **Keep.** Optional and off by default; usage is shown, not capped. | Shipped schema |
| L22 | Roadmap T3.13 "budget enforcement by cost" (`docs/ROADMAP.md:271`) | Planned | **Adapt.** Keep cost accounting for display; drop enforcement. | Doc only |
| L23 | Pi driver note: "honours full access only" (`crates/reno-harness/src/drivers/pi/mod.rs`) | Shipped | **Adapt.** Wrong about Pi: Pi can block each tool call from an extension. Lands with the harness spike. | Code comment and access modes |
| L24 | **ARCH-15** no executor trusted beyond what it declares | Approved | **Keep**, and apply it to Reno's own harness too. | Architecture |
| L25 | *New invariant*: on Reno's own harness, every tool call passes Reno's policy before it runs, fail-closed | — | **Add** (fresh ARCH ID) | None |
| L26 | *New invariant*: a bot's computer reaches the network only through `renod`'s egress allowlist | — | **Add** (fresh ARCH ID) | None |

## After confirmation

1. `/define-project` (update): rewrite `docs/product/vision.md` per the confirmed rows. Goals keep
   their IDs; a retired non-goal is struck through; new goals and invariants take fresh IDs.
2. `define-domain`: ADR for the reversal (Reno runs a harness of its own; D2 permission scope).
3. Spikes: S1 harness (Pi vs dsh) on ROAD-25; S2 the bot's computer.
4. `/plan-milestones`: new milestone(s) and the adapted ROAD-4, ROAD-25 and T3.13.
5. Code rows (L20, L23) and the README line (L18) become ROAD work; nothing is changed here.
