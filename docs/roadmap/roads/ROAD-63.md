# ROAD-63 per-bot-tool-and-domain-policy

<!--
The operator's intent for one road. `/shape --auto` reads every slot as their standing word;
agents decide everything else. At most 30 non-blank lines.
-->

**Outcome:** A bot works on its own and stops only for what is truly dangerous: anything inside its worktree, its computer and its allowed domains runs without a question, and the owner hears about an action only when it cannot be undone or leaves those bounds.

**Examples:**
1. Jayden's `docs-bot` runs at 02:00. In its worktree it installs packages from the lockfile, runs the tests, rewrites 40 files, commits, and pushes its own branch `reno/run-812`. In the morning Jayden finds a draft pull request and an empty inbox.
2. The same bot reads a web page, then tries to send a value from `.env` to `paste.example.net`, which is not in its domain list. The call never runs, the bot is told why and carries on, and Jayden finds one entry in the inbox.
3. `release-bot` tries `git push --force origin main`. It is refused without asking anyone, and the bot gets the reason.
4. Jayden's `research-bot` asks to add `arxiv.org` to its domains. The run keeps going on other work while the request waits in the inbox.

**Must keep:** Reno sees and records every tool call with its arguments; a call Reno cannot judge does not run; nothing inside a run can change Reno's definitions, data dir, the bot's own policy or its gate; no answer is never taken as yes.
**Off limits:** limits on money, tokens or time per run; questions about ordinary reads, edits or commands inside the bot's bounds; command allowlists as the main line of safety.
**Decided:** "chỉ chặn những thứ nguy hiểm"; autonomy and UX come first; the harness decides when a run is done; a refused call tells the bot why, so it can find another way. The owner is asked only to widen a bot's bounds (a new domain, a path outside its worktree), for actions done as the owner that cannot be undone (sending messages, spending money, merging to the main branch, deploying), and when secrets would leave to an unlisted place after the run read untrusted content.
**Delegated:** the exact patterns that count as dangerous and how commands are parsed; whether an automatic reviewer screens a case before the owner sees it; when repeated refusals end a run.
**Ask me:** a new kind of question to the owner; anything moved from "refused" to "asked".
**UI:** reuse — existing inbox and bot settings, adjusted where this road needs it
