# ROAD-4 harness-decides-permissions — notes

Scope after ADR-0001 (2026-11-22): this item covers only runs the owner starts on a harness Reno
drives (GOAL-8, GOAL-11). There the harness's own permission system decides and Reno adds no layer.
It does not remove the policy engine: that engine stays as the permission system of the Reno harness
(ARCH-18) and keeps gating unattended runs. See `docs/product/pivot-ledger.md` rows L2, L14 and L19.
