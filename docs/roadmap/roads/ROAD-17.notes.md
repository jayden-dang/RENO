# ROAD-17 commands-and-skills-discovery — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Deliberately narrow discovery: `~/.agents/skills` and `<worktree>/.agents/skills`, one level, no ancestor walk, no extra paths: `crates/agent_skills/README.md` (GPL).
- Strict validation (name `[a-z0-9-]{1,64}`, description 1-1024 chars) with a visible load error, not a lenient load.
- Catalog (name, description, location) and body are split; the catalog is capped at 50 KB and kept byte-stable when only a body changes, so the prompt cache stays warm; all values XML-escaped.
- Slash commands come from the agent's `available_commands` (ACP).
