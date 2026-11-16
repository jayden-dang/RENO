#![cfg(test)]

//! Standing rules through the kernel (T1.6b): a person's answer kept for a run, a bot or
//! everything; the precedence against the policy file and repeat denial; expiry; revocation; and
//! the roadmap's acceptance bullet end to end on the mock executor.
//!
//! Every script is replayed by the mock for each run, so a later run asks what the first asked.

use reno_harness::exec::Decision;
use reno_proto::{
    AccessMode, ApprovalDecision, ApprovalOutcome, ApprovalStatus, ExecEvent, Remember,
    RememberScope, ResolvedBy, RunId, RunStatus, StandingEffect, StandingScope,
};

use super::approvals::{Rig, command, edit, resolved, rule, script};
use crate::KernelError;
use reno_proto::{ActionClass, ApprovalSubject};

fn edits(paths: &[&str]) -> ApprovalSubject {
    ApprovalSubject {
        action: ActionClass::Edit,
        tool: "write".into(),
        command: None,
        paths: paths.iter().map(Into::into).collect(),
        raw: serde_json::Value::Null,
    }
}

const ASK: &str = "default = \"ask\"\n";

fn remember(scope: RememberScope) -> Option<Remember> {
    Some(Remember {
        scope,
        expires_in_secs: None,
    })
}

/// The name a decision by the (only) standing rule carries.
async fn standing_name(rig: &Rig) -> String {
    let rules = rig.fx.kernel.list_rules(true).await.unwrap();
    assert_eq!(rules.len(), 1, "{rules:?}");
    format!("standing:{}", rules[0].id)
}

/// Answer the pending approval, remembering it as `scope`.
async fn answer(rig: &Rig, decision: ApprovalDecision, scope: Option<Remember>) {
    let pending = rig.pending().await;
    rig.fx
        .kernel
        .decide_remembering(&pending.id, decision, None, scope)
        .await
        .unwrap();
}

async fn finish(rig: &Rig, run: &RunId) {
    rig.fx.wait_status(run, RunStatus::Succeeded).await;
}

// ---- the acceptance bullet ----------------------------------------------------------------

/// ROADMAP T1.6b: "A Supervised task that tries a shell command creates an approval record;
/// denying it in the app makes the harness see a denial; the same subject asked again in that
/// run is denied unasked."
#[tokio::test(start_paused = true)]
async fn a_supervised_shell_command_is_held_denied_by_a_person_and_denied_again_unasked() {
    let rig = Rig::new(
        ASK,
        script(vec![
            ("r1", command("rm -rf build")),
            ("r2", command("rm -rf build")),
        ]),
    )
    .await;
    let run = rig.run_as("keeper").await;

    // The bot says nothing about access, so the run is Supervised, and the command is held.
    let held = rig.pending().await;
    assert_eq!(rig.mock.log().specs[0].access, AccessMode::Supervised);
    assert_eq!(held.subject, command("rm -rf build"));
    rig.fx.wait_status(&run, RunStatus::WaitingApproval).await;
    let record = rig.fx.store.get_approval(&held.id).await.unwrap().unwrap();
    assert_eq!(record.status, ApprovalStatus::Pending);
    assert_eq!(record.subject_digest, held.subject.digest());

    // Denying it in the app (a plain "once" answer) makes the harness see a denial.
    answer(&rig, ApprovalDecision::Deny, None).await;
    finish(&rig, &run).await;
    assert_eq!(rig.decisions(), [Decision::Deny, Decision::Deny]);

    // The second ask is the same subject: denied by repeat denial, and nobody was asked.
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
    let journal = rig.fx.journal_with_done(&run).await;
    assert_eq!(
        resolved(&journal),
        [
            (
                "r1".into(),
                ApprovalOutcome::Denied,
                ResolvedBy::Human,
                None
            ),
            (
                "r2".into(),
                ApprovalOutcome::Denied,
                ResolvedBy::RepeatDenial,
                None
            ),
        ]
    );
    // Each request has a record with the exact subject's digest.
    let history = rig.history(&run).await;
    assert_eq!(history.len(), 2);
    for record in &history {
        assert_eq!(record.subject, command("rm -rf build"));
        assert_eq!(
            record.subject_digest.as_deref(),
            Some(command("rm -rf build").digest().as_str())
        );
        assert_eq!(record.status, ApprovalStatus::Denied);
    }
}

/// "Approve once" is exact: the same subject asked again in the same run is held again.
#[tokio::test(start_paused = true)]
async fn approving_once_does_not_cover_the_same_subject_asked_again() {
    let rig = Rig::new(
        ASK,
        script(vec![
            ("r1", command("git push")),
            ("r2", command("git push")),
        ]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    let first = rig.pending().await;
    rig.fx
        .kernel
        .decide(&first.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    let second = rig.pending().await;
    assert_ne!(
        second.id, first.id,
        "asked again, not answered by the first"
    );
    assert_eq!(second.subject, first.subject);
    assert!(rig.fx.kernel.list_rules(true).await.unwrap().is_empty());
    rig.fx
        .kernel
        .decide(&second.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    finish(&rig, &run).await;
    let history = rig.history(&run).await;
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].subject_digest, history[1].subject_digest);
}

// ---- scopes -------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_run_rule_covers_the_rest_of_that_run_and_no_other_run() {
    let rig = Rig::new(
        ASK,
        script(vec![
            ("r1", command("git push")),
            ("r2", command("git push")),
            ("r3", command("git push --force")),
        ]),
    )
    .await;
    let first = rig.run_as("keeper").await;
    answer(
        &rig,
        ApprovalDecision::ApproveOnce,
        remember(RememberScope::Run),
    )
    .await;

    // r2 is the same subject: allowed unasked, by the rule. r3 is another subject: asked.
    let third = rig.pending().await;
    assert_eq!(third.subject, command("git push --force"));
    let name = standing_name(&rig).await;
    rig.fx
        .kernel
        .decide(&third.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    finish(&rig, &first).await;
    let journal = rig.fx.journal_with_done(&first).await;
    let by: Vec<_> = resolved(&journal).into_iter().map(|r| r.2).collect();
    assert_eq!(by, [ResolvedBy::Human, rule(&name), ResolvedBy::Human]);

    // A run's rule ends with its run: it is no longer listed as in force, only in the history.
    assert!(rig.fx.kernel.list_rules(false).await.unwrap().is_empty());
    let rules = rig.fx.kernel.list_rules(true).await.unwrap();
    assert_eq!(rules[0].scope, StandingScope::Run { run: first.clone() });
    assert_eq!(rules[0].effect, StandingEffect::Allow);
    assert!(rules[0].source_approval.is_some());

    // A later run asks again.
    let second = rig.run_as("keeper").await;
    let pending = rig.pending().await;
    assert_eq!(pending.run_id, second);
    assert_eq!(pending.subject, command("git push"));
    rig.fx
        .kernel
        .decide(&pending.id, ApprovalDecision::Deny)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_bot_rule_covers_every_run_of_that_bot_only() {
    let rig = Rig::new(ASK, script(vec![("r1", command("git push"))])).await;
    let first = rig.run_as("keeper").await;
    answer(
        &rig,
        ApprovalDecision::ApproveOnce,
        remember(RememberScope::Bot),
    )
    .await;
    finish(&rig, &first).await;
    let name = standing_name(&rig).await;

    // Another run of the bot: unasked.
    let again = rig.run_as("keeper").await;
    finish(&rig, &again).await;
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
    assert_eq!(rig.history(&again).await[0].decided_by, Some(rule(&name)));

    // Another bot asks.
    let scout = rig.run_as("scout").await;
    let pending = rig.pending().await;
    assert_eq!(pending.run_id, scout);
    rig.fx
        .kernel
        .decide(&pending.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    finish(&rig, &scout).await;
}

#[tokio::test(start_paused = true)]
async fn a_global_rule_covers_every_run_including_ones_with_no_bot() {
    let rig = Rig::new(ASK, script(vec![("r1", command("git push"))])).await;
    let first = rig.run_as("keeper").await;
    answer(
        &rig,
        ApprovalDecision::ApproveOnce,
        remember(RememberScope::Always),
    )
    .await;
    finish(&rig, &first).await;
    let name = standing_name(&rig).await;
    for run in [
        rig.run_as("scout").await,
        rig.fx
            .kernel
            .submit("go", super::id("mock"), "/work/tree".into(), None)
            .await
            .unwrap(),
    ] {
        finish(&rig, &run).await;
        assert_eq!(rig.history(&run).await[0].decided_by, Some(rule(&name)));
    }
}

#[tokio::test(start_paused = true)]
async fn a_bot_rule_for_a_run_with_no_bot_is_refused_and_decides_nothing() {
    let rig = Rig::new(ASK, script(vec![("r1", command("git push"))])).await;
    let run = rig
        .fx
        .kernel
        .submit("go", super::id("mock"), "/work/tree".into(), None)
        .await
        .unwrap();
    let pending = rig.pending().await;
    let refused = rig
        .fx
        .kernel
        .decide_remembering(
            &pending.id,
            ApprovalDecision::ApproveOnce,
            None,
            remember(RememberScope::Bot),
        )
        .await;
    assert!(
        matches!(refused, Err(KernelError::Invalid(_))),
        "{refused:?}"
    );
    assert!(rig.fx.kernel.list_rules(true).await.unwrap().is_empty());
    // Still held: the answer can be given again.
    assert_eq!(rig.pending().await.id, pending.id);
    for bad in [0, 366 * 24 * 60 * 60 + 1] {
        let refused = rig
            .fx
            .kernel
            .decide_remembering(
                &pending.id,
                ApprovalDecision::ApproveOnce,
                None,
                Some(Remember {
                    scope: RememberScope::Always,
                    expires_in_secs: Some(bad),
                }),
            )
            .await;
        assert!(matches!(refused, Err(KernelError::Invalid(_))), "{bad}");
    }
    rig.fx
        .kernel
        .decide(&pending.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    finish(&rig, &run).await;
}

// ---- deny rules ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_remembered_denial_refuses_unasked_in_later_runs_with_a_message_naming_the_rule() {
    let rig = Rig::new(ASK, script(vec![("r1", command("rm -rf build"))])).await;
    let first = rig.run_as("keeper").await;
    answer(&rig, ApprovalDecision::Deny, remember(RememberScope::Bot)).await;
    finish(&rig, &first).await;
    let name = standing_name(&rig).await;

    let second = rig.run_as("keeper").await;
    finish(&rig, &second).await;
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
    let journal = rig.fx.journal_with_done(&second).await;
    let [(_, outcome, by, message)] = &resolved(&journal)[..] else {
        panic!("one resolution expected");
    };
    assert_eq!((*outcome, by), (ApprovalOutcome::Denied, &rule(&name)));
    let message = message.as_deref().unwrap();
    assert!(message.contains("standing rule"), "{message}");
    assert!(
        !message.contains("revoke"),
        "the harness is not told how to undo a person's refusal: {message}"
    );
    assert_eq!(rig.decisions(), [Decision::Deny, Decision::Deny]);
}

#[tokio::test(start_paused = true)]
async fn a_standing_deny_beats_a_standing_allow_whatever_their_scopes() {
    // An allow for [a, b] needs every path to match, so [b, c] still asks; a deny of [b, c]
    // needs any path to match, so it also catches [a, b]. Both are then in force for [a, b].
    let rig = Rig::new(
        ASK,
        script(vec![("r1", edits(&["a", "b"])), ("r2", edits(&["b", "c"]))]),
    )
    .await;
    let first = rig.run_as("keeper").await;
    answer(
        &rig,
        ApprovalDecision::ApproveOnce,
        remember(RememberScope::Always),
    )
    .await;
    answer(
        &rig,
        ApprovalDecision::Deny,
        remember(RememberScope::Always),
    )
    .await;
    finish(&rig, &first).await;
    assert_eq!(rig.decisions(), [Decision::Allow, Decision::Deny]);
    assert_eq!(rig.fx.kernel.list_rules(false).await.unwrap().len(), 2);

    // The second run: r1 meets the always-allow (every path matches) and the always-deny (one
    // path matches). The deny wins, and nobody is asked.
    let second = rig.run_as("keeper").await;
    finish(&rig, &second).await;
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
    let history = rig.history(&second).await;
    assert_eq!(history.len(), 2);
    for record in &history {
        assert_eq!(record.status, ApprovalStatus::Denied, "{record:?}");
        assert!(
            matches!(&record.decided_by, Some(ResolvedBy::Rule { name }) if name.starts_with("standing:")),
            "{record:?}"
        );
    }
}

// ---- precedence against the policy file ----------------------------------------------------

#[tokio::test(start_paused = true)]
async fn an_explicit_deny_in_the_policy_file_beats_a_standing_allow() {
    let rig = Rig::new(ASK, script(vec![("r1", command("git push"))])).await;
    let first = rig.run_as("keeper").await;
    answer(
        &rig,
        ApprovalDecision::ApproveOnce,
        remember(RememberScope::Always),
    )
    .await;
    finish(&rig, &first).await;

    rig.reload(
        r#"
        default = "ask"
        [[rules]]
        note = "never push"
        effect = "deny"
        match = { action = "command", command = "git push*" }
        "#,
    )
    .await;
    let second = rig.run_as("keeper").await;
    finish(&rig, &second).await;
    assert_eq!(
        rig.history(&second).await[0].decided_by,
        Some(rule("never push"))
    );
    assert_eq!(rig.decisions(), [Decision::Allow, Decision::Deny]);
}

#[tokio::test(start_paused = true)]
async fn a_default_deny_in_the_policy_file_beats_a_standing_allow() {
    let rig = Rig::new(ASK, script(vec![("r1", command("git push"))])).await;
    let first = rig.run_as("keeper").await;
    answer(
        &rig,
        ApprovalDecision::ApproveOnce,
        remember(RememberScope::Always),
    )
    .await;
    finish(&rig, &first).await;
    rig.reload("default = \"deny\"\n").await;
    let second = rig.run_as("keeper").await;
    finish(&rig, &second).await;
    assert_eq!(rig.decisions(), [Decision::Allow, Decision::Deny]);
    assert_eq!(
        rig.history(&second).await[0].decided_by,
        Some(rule("default"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_standing_deny_beats_a_policy_allow_and_a_standing_allow_beats_a_policy_ask() {
    let rig = Rig::new(
        r#"
        default = "ask"
        [[rules]]
        note = "git is fine"
        effect = "allow"
        match = { action = "command", command = "git status" }
        [[rules]]
        note = "ask about push"
        effect = "ask"
        match = { action = "command", command = "git push" }
        "#,
        script(vec![
            ("r1", command("git status")),
            ("r2", command("git push")),
        ]),
    )
    .await;
    // The run is held on `git push` (an ask rule), after `git status` was allowed by policy.
    let first = rig.run_as("keeper").await;
    answer(
        &rig,
        ApprovalDecision::ApproveOnce,
        remember(RememberScope::Bot),
    )
    .await;
    finish(&rig, &first).await;
    let name = standing_name(&rig).await;

    // Next run: `git status` still by policy, and `git push` now by the standing allow.
    let second = rig.run_as("keeper").await;
    finish(&rig, &second).await;
    let by: Vec<_> = rig
        .history(&second)
        .await
        .into_iter()
        .rev()
        .map(|r| r.decided_by.unwrap())
        .collect();
    assert_eq!(by, [rule("git is fine"), rule(&name)]);
}

// ---- matcher safety, end to end --------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn always_allow_git_push_does_not_allow_force_push_or_a_chain() {
    let rig = Rig::new(
        ASK,
        script(vec![
            ("r1", command("git push")),
            ("r2", command("git push --force")),
            ("r3", command("git push && rm -rf ~")),
            ("r4", command("git push")),
        ]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    answer(
        &rig,
        ApprovalDecision::ApproveOnce,
        remember(RememberScope::Always),
    )
    .await;
    // r2 and r3 are held, each by a human; r4 is allowed unasked.
    for (expected, decision) in [
        ("git push --force", ApprovalDecision::Deny),
        ("git push && rm -rf ~", ApprovalDecision::Deny),
    ] {
        let pending = rig.pending().await;
        assert_eq!(pending.subject, command(expected));
        rig.fx.kernel.decide(&pending.id, decision).await.unwrap();
    }
    finish(&rig, &run).await;
    assert_eq!(
        rig.decisions(),
        [
            Decision::Allow,
            Decision::Deny,
            Decision::Deny,
            Decision::Allow
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn an_inside_path_rule_never_covers_a_path_outside_the_worktree() {
    let rig = Rig::new(
        ASK,
        script(vec![
            ("r1", edit("/work/tree/src/lib.rs")),
            ("r2", edit("src/lib.rs")),
            ("r3", edit("/etc/passwd")),
            ("r4", edit("../tree/../../etc/passwd")),
        ]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    answer(
        &rig,
        ApprovalDecision::ApproveOnce,
        remember(RememberScope::Bot),
    )
    .await;
    // r2 is the same file in relative spelling: allowed. r3 and r4 are outside: asked.
    for expected in [edit("/etc/passwd"), edit("../tree/../../etc/passwd")] {
        let pending = rig.pending().await;
        assert_eq!(pending.subject, expected);
        rig.fx
            .kernel
            .decide(&pending.id, ApprovalDecision::Deny)
            .await
            .unwrap();
    }
    finish(&rig, &run).await;
    assert_eq!(
        rig.decisions(),
        [
            Decision::Allow,
            Decision::Allow,
            Decision::Deny,
            Decision::Deny
        ]
    );
}

// ---- expiry and revocation -------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_rule_stops_applying_when_it_expires() {
    let rig = Rig::new(ASK, script(vec![("r1", command("git push"))])).await;
    let first = rig.run_as("keeper").await;
    answer(
        &rig,
        ApprovalDecision::ApproveOnce,
        Some(Remember {
            scope: RememberScope::Always,
            expires_in_secs: Some(600),
        }),
    )
    .await;
    finish(&rig, &first).await;
    let rules = rig.fx.kernel.list_rules(false).await.unwrap();
    assert_eq!(
        rules[0].expires_at_ms.unwrap(),
        rules[0].created_at_ms + 600_000
    );

    // Inside the window: unasked.
    rig.fx.clock.advance(599_000);
    let second = rig.run_as("keeper").await;
    finish(&rig, &second).await;
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());

    // Past it: asked again, and the list no longer shows it unless asked to.
    rig.fx.clock.advance(2_000);
    let third = rig.run_as("keeper").await;
    let pending = rig.pending().await;
    assert_eq!(pending.run_id, third);
    assert!(rig.fx.kernel.list_rules(false).await.unwrap().is_empty());
    assert_eq!(rig.fx.kernel.list_rules(true).await.unwrap().len(), 1);
    rig.fx
        .kernel
        .decide(&pending.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    finish(&rig, &third).await;
}

#[tokio::test(start_paused = true)]
async fn a_revoked_rule_stops_applying_and_an_unknown_one_is_not_found() {
    let rig = Rig::new(ASK, script(vec![("r1", command("git push"))])).await;
    let first = rig.run_as("keeper").await;
    answer(
        &rig,
        ApprovalDecision::ApproveOnce,
        remember(RememberScope::Always),
    )
    .await;
    finish(&rig, &first).await;
    let id = rig.fx.kernel.list_rules(false).await.unwrap()[0].id.clone();

    rig.fx.kernel.revoke_rule(&id).await.unwrap();
    rig.fx.kernel.revoke_rule(&id).await.unwrap();
    assert!(rig.fx.kernel.list_rules(false).await.unwrap().is_empty());
    let all = rig.fx.kernel.list_rules(true).await.unwrap();
    assert!(all[0].revoked_at_ms.is_some());
    let unknown = reno_proto::RuleId::from_suffix("nope").unwrap();
    assert!(matches!(
        rig.fx.kernel.revoke_rule(&unknown).await,
        Err(KernelError::RuleNotFound(_))
    ));

    let second = rig.run_as("keeper").await;
    let pending = rig.pending().await;
    assert_eq!(pending.run_id, second);
    rig.fx
        .kernel
        .decide(&pending.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    finish(&rig, &second).await;
}

#[tokio::test(start_paused = true)]
async fn the_decision_that_makes_a_rule_leaves_its_events_and_the_journal_stays_whole() {
    let rig = Rig::new(ASK, script(vec![("r1", command("git push"))])).await;
    let run = rig.run_as("keeper").await;
    answer(
        &rig,
        ApprovalDecision::ApproveOnce,
        remember(RememberScope::Bot),
    )
    .await;
    finish(&rig, &run).await;
    let kinds: Vec<String> = rig
        .fx
        .store
        .events_since(reno_proto::Seq(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .filter(|k| k.starts_with("rule.") || k.starts_with("approval."))
        .collect();
    assert_eq!(
        kinds,
        ["approval.requested", "approval.decided", "rule.created"]
    );
    let journal = rig.fx.journal_with_done(&run).await;
    let pairs = journal
        .iter()
        .filter(|(_, e)| {
            matches!(
                e,
                ExecEvent::NeedsApproval { .. } | ExecEvent::ApprovalResolved { .. }
            )
        })
        .count();
    assert_eq!(
        pairs, 2,
        "a rule made by a person is journaled like any answer"
    );
}
