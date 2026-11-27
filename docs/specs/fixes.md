# Fixes

Fix requirements for behaviour no feature spec owns. Each fix has a requirement for the
corrected behaviour and a `SHALL CONTINUE TO` guard for what must not regress.

## FIX-1 — A late `run.get` answer does not undo a newer run state (2026-11-19)

Proposition (accepted by the owner, confirmed_for_scope: fake daemon, `step: ZERO`): a
`run.get` reply that `Store::ensure_run` asked for when `RunStarted` opened a run before
`runs.watch` delivered it could arrive after a newer `runs.watch` snapshot, and
`State::upsert_run` replaced the run unconditionally, so the run stayed on the older status
(the watch does not resend an unchanged list). Seen as `runs_empty_and_live` timing out on
`Running`/`Evaluating`.

- **FIX-1.1** WHEN a run summary arrives whose `updated_at_ms` is older than the summary the
  client mirror holds for that run, THE client mirror SHALL keep the summary it holds.
- **FIX-1.2** THE client mirror SHALL CONTINUE TO replace a run's summary with one whose
  `updated_at_ms` is equal or newer, and to insert a run it does not hold.

Tests: `reno-client` `state::tests::a_run_get_answer_older_than_the_watch_does_not_undo_it`
(FIX-1.1, FIX-1.2), `state::tests::run_upsert_inserts_then_replaces` (FIX-1.2); reproduction
loop `reno-app` `screen_tests::runs_empty_and_live` (was 6/40 red, 0/100 after).

## FIX-2 — Approval feedback after a lost response (2026-11-27)

- **FIX-2.1** WHILE the app is sending an approval decision or its outcome is
  uncertain, THE Approval panel SHALL show that state and keep decision actions
  disabled.
- **FIX-2.2** WHEN the RPC fails, THE app SHALL reconcile the same approval ID
  using current pending approvals and the run's decision history. An unavailable
  read or an ID missing from both sources leaves the outcome uncertain.
- **FIX-2.3** WHEN the app reconciles the ID, THE app SHALL report the recorded
  outcome if present or show the error and reopen the decision actions if the
  request is still pending. Retrying the decision requires a new user action.
- **FIX-2.4** THE Approval panel SHALL CONTINUE TO leave the composer's
  keystrokes alone until the user claims the panel, and to send decisions through
  the existing approval RPC and durable daemon store.

Verification seam: drive the native GPUI panel against the existing fake at the
RPC boundary. Hold a decision before answering, drop its connection, then let
the normal client reconnect and read the daemon's pending list and history.
