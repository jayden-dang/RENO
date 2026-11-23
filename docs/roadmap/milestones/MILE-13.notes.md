# MILE-13 Daily work in a thread

## Operator direction

Recorded on 2026-11-23 from this planning conversation:

> Đúng rồi, đưa comments thành tính năng cốt lõi của thread chat mới.

> Hãy bắt đầu lên requirements và thiết kế theo Delta, không mở rộng ROAD-33 và chỉ triển khai sau khi ROAD-33 hoàn thành.

> Okei, hãy giúp tôi plan milestone lại cho phần này - sau khi road-33 xong thì chúng ta sẽ frame change cho road này ngay sau đó

The latest instruction sets the next entry point: finish ROAD-33, then frame ROAD-71 immediately. Existing research and exploratory requirements/design outlines are inputs, not approved specs. This planning update writes no requirements, UI implementation, feature registration or implementation tasks.

## Why a separate milestone

Thread chat is the owner's most-used surface. Its outcome combines the thread workflow, comments as actionable feedback, explicit context and harness invocation, and terminal evidence. These are related but separately reviewable delivery units, represented by ROAD-71, the existing ROAD-60, ROAD-72 and ROAD-73.

This milestone has no whole-milestone dependency on MILE-10: ROAD-71 waits specifically for ROAD-33, not every remaining item in MILE-10. All other members wait on ROAD-71, so none starts before ROAD-33. Focus makes MILE-13 the next priority without inventing dependencies on unrelated work such as ROAD-55.

Comments and context can be shaped as separate slices once the common thread workflow is settled. Terminal-in-chat follows the explicit-context contract. These are planning boundaries to review during ROAD-71, not an instruction to silently split or combine approved feature specs later.

## Existing ownership to respect

- ROAD-33 remains the reading/rendering/selection substrate. Its scope and active worktree are unchanged.
- ROAD-31 remains the existing composer baseline; ROAD-17 owns native command/skill discovery. ROAD-72 must reuse or amend those capabilities rather than create a second discovery system.
- ROAD-60 keeps its identity and earlier comment intent. The copy already recorded on the transcript branch moves under this milestone in the canonical roadmap; it is not a new duplicate road.
- ROAD-24 retains side-chat/fork ownership. ROAD-59 retains deferred edit-past-prompt/regenerate ownership in MILE-6. This plan does not pull either into the first thread-chat delivery.
- ROAD-54 retains broad mock-to-real integration ownership. The frame must decide each slice's real versus mocked contract coverage and any demonstrated prerequisites without treating styled controls as delivered behavior.
- Background commands, plans, permission/questions and child events keep their existing harness/orchestration owners. This milestone should compose their thread surfaces, not replace their semantics.

## Evidence worth carrying forward

Delta 0.28.4 was exercised natively on 2026-11-23 in two no-project test threads. Observed: real sends; source-linked comments on Delta Agent and Claude Agent; two comments submitted together with separate answers; skill preview/insert/send; thread mention plus a tool-based read; inline terminal command/capture/reuse; and context recall after switching to Claude.

Three specific findings need decisions during the frame:

1. Escape canceled a nonempty pending comment in the tested state, whereas moving Down retained it. Reno's dismissal/cancellation behavior is not yet chosen.
2. Reading a referenced thread included a persisted unsent terminal draft in that experiment. Whether Reno includes drafts in mention context is not chosen.
3. A sent terminal card remained frozen when the same shell later printed another marker. The lifecycle and initial delivery scope for this interaction remain to be specified.

Native file/diff comments, duplicate quotes, reply chains, busy-run delivery, reconnect/retry/restart, background processes and human IME still need investigation. No claim of full Delta parity follows from the initial experiments.

Primary product references: [Threads](https://delta.dev/docs/agents/threads), [Comments](https://delta.dev/docs/agents/comments), [Terminals](https://delta.dev/docs/agents/terminals), [Skills](https://delta.dev/docs/agents/skills), [Claude Agent](https://delta.dev/docs/agents/claude-agent).

## Integration with the active transcript branch

The transcript branch already defines ROAD-60 in MILE-10 and defers ROAD-59 in MILE-6. This update preserves both IDs in the canonical roadmap and carries over their notes. When the branch is integrated, retain the single ROAD-60 definition in MILE-13 and the single ROAD-59 deferral; do not reintroduce a duplicate membership. This requires no change to ROAD-33's requirements/design/tasks.
