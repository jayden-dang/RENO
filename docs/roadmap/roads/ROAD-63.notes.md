# ROAD-63 per-bot-tool-and-domain-policy — notes

Research 2026-11-22 for the card's "dangerous only" stance. Vendor figures are self-reported.

- **Containment first, prompts last.** OS sandboxing in Claude Code cut permission prompts by 84%
  and fatigue set in "within weeks"; about 93% of manual prompts were approved, so most prompts were
  noise (https://anthropic.com/engineering/how-we-contain-claude, 2026-05-25). Codex runs everything
  inside its workspace sandbox without review and escalates only boundary crossings; with its
  reviewer, sessions stop for a human about 200 times less often and 99.1% of escalations are
  approved (https://openai.com/safety-alignment/auto-review/, 2026-04-30).
- **What both vendors' reviewers block:** exfiltration and secret exposure, irreversible deletion
  and destructive git (force push, hard reset), running downloaded code, weakening security or
  permissions, production deploys, acting on instructions found in untrusted content. Neither treats
  ordinary edits or local commands as risky
  (https://code.claude.com/docs/en/permission-modes; https://learn.chatgpt.com/docs/agent-approvals-security).
- **Reviewers miss things:** a classifier missed 17% of overeager actions in a 52-action sample
  (https://www.anthropic.com/engineering/claude-code-auto-mode, 2026-03-25). It must sit inside the
  VM, the worktree and the egress allowlist, never replace them.
- **Escalation trigger:** at most two of untrusted input, sensitive data, outbound action in one run
  (Agents Rule of Two, https://ai.meta.com/blog/practical-ai-agent-security/; the "lethal trifecta",
  https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/).
- **Gate at the irreversible edge:** a coding agent may push only its own branch and open draft pull
  requests; humans gate merges and workflow runs
  (https://docs.github.com/en/copilot/concepts/agents/coding-agent/risks-and-mitigations).
- **Command allowlists leak** (string matching is bypassed); protect the agent's own control plane
  (`.git`, the agent's config) even inside its workspace.
- **Unattended:** refuse with a reason, queue the request for the owner, keep working; stop after
  repeated refusals (one vendor uses 3 in a row or 20 in total).

Today's shipped default policy is the opposite of this card: `default = "ask"`, every network
action asks, every `git push` asks (`crates/reno-proto/src/defs/defaults/policy.toml`).
