//! Decided approvals, as the Project page's History tab lists them: what was asked, the answer,
//! who gave it and when. Built from `approval.history`. (This was the Inbox's "Decided" list
//! before answers moved into each run, ui-brief 2026-11-17.)

use reno_client::State;
use reno_proto::{ApprovalRecord, ApprovalStatus, ResolvedBy, RunId};

use crate::summary;

/// How a decided approval ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Approved,
    Denied,
    Expired,
    /// Closed because its run ended ("withdrawn: the run was cancelled"), not because nobody
    /// answered.
    Withdrawn,
}

/// One decided approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryRow {
    pub(crate) run: RunId,
    pub(crate) bot: Option<String>,
    pub(crate) action: &'static str,
    pub(crate) line: String,
    pub(crate) verdict: Verdict,
    /// "you", "policy rule 8", "your rule", "nobody in time", "repeat of a denial"; for a
    /// withdrawn one the whole phrase, "withdrawn — the run was cancelled".
    pub(crate) by: String,
    pub(crate) when: String,
    /// The reason given with a denial.
    pub(crate) message: Option<String>,
}

/// The bot of `run`: its pin, or the bot its title names.
fn run_bot(state: &State, run: &RunId) -> Option<String> {
    let summary = state.run(run)?;
    summary
        .bot
        .as_ref()
        .map(|b| b.name.clone())
        .or_else(|| summary::parse_title(&summary.title).bot.map(str::to_owned))
}

fn who(by: Option<&ResolvedBy>, status: ApprovalStatus) -> String {
    match by {
        Some(ResolvedBy::Human) => "you".to_owned(),
        Some(ResolvedBy::Rule { name }) => summary::rule_words(name),
        Some(ResolvedBy::Expired) => "nobody in time".to_owned(),
        Some(ResolvedBy::RepeatDenial) => "repeat of an earlier denial".to_owned(),
        None if status == ApprovalStatus::Expired => "nobody in time".to_owned(),
        None => "the daemon".to_owned(),
    }
}

/// One decided approval as a row; `None` for a pending one.
pub(crate) fn row(state: &State, record: &ApprovalRecord, now_ms: u64) -> Option<HistoryRow> {
    let verdict = match record.status {
        ApprovalStatus::Approved => Verdict::Approved,
        ApprovalStatus::Denied => Verdict::Denied,
        ApprovalStatus::Expired => Verdict::Expired,
        // `approval.history` never returns pending ones.
        ApprovalStatus::Pending => return None,
    };
    let withdrawn = (verdict == Verdict::Expired)
        .then(|| summary::withdrawn_reason(record.message.as_deref()))
        .flatten();
    let decided = record.decided_at_ms.unwrap_or(record.created_at_ms);
    Some(HistoryRow {
        run: record.run_id.clone(),
        bot: run_bot(state, &record.run_id),
        action: summary::action_word(&record.subject),
        line: summary::subject_line(&record.subject),
        verdict: if withdrawn.is_some() {
            Verdict::Withdrawn
        } else {
            verdict
        },
        by: match withdrawn {
            Some(reason) => format!("withdrawn \u{2014} {reason}"),
            None => who(record.decided_by.as_ref(), record.status),
        },
        when: summary::ago_label(now_ms.saturating_sub(decided)),
        // A withdrawn approval's message is the reason shown in `by`, not one a person gave.
        message: record.message.clone().filter(|_| withdrawn.is_none()),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_client::Update;
    use reno_proto::{ActionClass, ApprovalId, ApprovalSubject};

    use super::*;

    const HOUR: u64 = 3_600_000;
    const NOW: u64 = 1_000 * HOUR;

    fn record(id: &str, status: ApprovalStatus, by: Option<ResolvedBy>) -> ApprovalRecord {
        ApprovalRecord {
            id: ApprovalId::from_suffix(id).unwrap(),
            run_id: RunId::from_suffix("r").unwrap(),
            request_id: "req".into(),
            subject: ApprovalSubject {
                action: ActionClass::Command,
                tool: "bash".into(),
                command: Some("cargo test".into()),
                paths: vec![],
                raw: serde_json::Value::Null,
            },
            subject_digest: None,
            status,
            decided_by: by,
            created_at_ms: NOW - 3 * HOUR,
            decided_at_ms: Some(NOW - 2 * HOUR),
            expires_at_ms: NOW,
            message: None,
        }
    }

    fn rows(records: Vec<ApprovalRecord>) -> Vec<HistoryRow> {
        let mut state = State::new();
        state.apply(Update::ApprovalHistory(records));
        state
            .approval_history()
            .iter()
            .filter_map(|r| row(&state, r, NOW))
            .collect()
    }

    #[test]
    fn history_names_who_decided() {
        let rows = rows(vec![
            record("a", ApprovalStatus::Approved, Some(ResolvedBy::Human)),
            record(
                "b",
                ApprovalStatus::Denied,
                Some(ResolvedBy::Rule {
                    name: "never reset hard".into(),
                }),
            ),
            record("c", ApprovalStatus::Expired, Some(ResolvedBy::Expired)),
            record("d", ApprovalStatus::Denied, Some(ResolvedBy::RepeatDenial)),
            record(
                "e",
                ApprovalStatus::Approved,
                Some(ResolvedBy::Rule {
                    name: "rules[7]".into(),
                }),
            ),
            record(
                "f",
                ApprovalStatus::Approved,
                Some(ResolvedBy::Rule {
                    name: "standing:rule_001abc".into(),
                }),
            ),
        ]);
        let summary: Vec<_> = rows.iter().map(|r| (r.verdict, r.by.as_str())).collect();
        assert_eq!(
            summary,
            [
                (Verdict::Approved, "you"),
                (Verdict::Denied, "rule never reset hard"),
                (Verdict::Expired, "nobody in time"),
                (Verdict::Denied, "repeat of an earlier denial"),
                (Verdict::Approved, "policy rule 8"),
                (Verdict::Approved, "your rule"),
            ]
        );
        assert_eq!(rows[0].when, "2h ago");
    }

    #[test]
    fn an_approval_closed_because_the_run_ended_is_withdrawn_not_expired() {
        let mut withdrawn = record("a", ApprovalStatus::Expired, Some(ResolvedBy::Expired));
        withdrawn.message = Some("withdrawn: the run was cancelled".into());
        let rows = rows(vec![
            withdrawn,
            record("b", ApprovalStatus::Expired, Some(ResolvedBy::Expired)),
        ]);
        assert_eq!(rows[0].verdict, Verdict::Withdrawn);
        assert_eq!(rows[0].by, "withdrawn \u{2014} the run was cancelled");
        assert_eq!(rows[0].message, None, "the reason is not shown twice");
        assert_eq!(rows[1].verdict, Verdict::Expired);
        assert_eq!(rows[1].by, "nobody in time");
    }

    #[test]
    fn a_denial_shows_the_reason_that_was_given() {
        let mut denied = record("a", ApprovalStatus::Denied, Some(ResolvedBy::Human));
        denied.message = Some("use the staging bucket".into());
        let rows = rows(vec![
            denied,
            record("b", ApprovalStatus::Denied, Some(ResolvedBy::Human)),
        ]);
        assert_eq!(rows[0].message.as_deref(), Some("use the staging bucket"));
        assert_eq!(rows[1].message, None);
    }
}
