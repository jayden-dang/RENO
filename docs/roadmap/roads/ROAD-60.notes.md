# ROAD-60 comments-on-the-transcript — notes

Added 2026-11-22 on the operator's answer to a scope card raised while ROAD-33 was being specified. The source proposal is
the internal note `docs/internal/2026-11-22-bots-own-loop-proposal.md`, §11 (gitignored).

What the item covers, in the operator's chosen option:
1. Select a passage in any message (a prompt, a reply, a plan step, thinking, a diff line) and attach a comment anchored to
   that passage.
2. Comments wait for the next send in a composer tray, "N comments waiting". They go with a message or alone, packaged as a
   follow-up that quotes the passage, so they work with any harness.
3. The agent answers each comment, and each answer links back to its passage. Sending a comment to a fork waits for fork
   support.
4. Answer an agent's question in place in the transcript; the inbox stays for unattended runs.
5. Typed highlights (todo, review, rationale, decision) that turn into comments.

It goes through `draft-ui`. ROAD-33 builds no comment code. It does resolve every selection to source anchors (the entry, and
the character range of its source text), so this item anchors to a selection without rework (TRANSCRIPT-2.13).
