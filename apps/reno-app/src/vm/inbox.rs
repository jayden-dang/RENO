//! The Inbox: what needs the human first (held actions, longest-waiting first), then what was
//! decided. Built from `approvals.watch` (pending) and `approval.history` (decided).

use reno_client::{Feed, State};
use reno_proto::{ActionClass, Approval, ApprovalRecord, ApprovalStatus, ResolvedBy, RunId};

use super::{Banner, Section, banner};
use crate::summary;

/// One held action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeldRow {
    pub(crate) approval: Approval,
    pub(crate) run: RunId,
    /// The bot, when the run's title names one.
    pub(crate) bot: Option<String>,
    pub(crate) repo: Option<String>,
    pub(crate) executor: String,
    /// "push", "command", "network request"...
    pub(crate) action: &'static str,
    pub(crate) tool: String,
    /// The command, or the request, on one line.
    pub(crate) line: String,
    pub(crate) paths: Vec<String>,
    /// How long it has waited: "6h".
    pub(crate) waited: String,
    pub(crate) network: bool,
}

/// How a decided approval ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Approved,
    Denied,
    Expired,
}

/// One decided approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryRow {
    pub(crate) run: RunId,
    pub(crate) bot: Option<String>,
    pub(crate) action: &'static str,
    pub(crate) line: String,
    pub(crate) verdict: Verdict,
    /// "you", "rule: never reset hard", "nobody in time", "repeat of a denial".
    pub(crate) by: String,
    pub(crate) when: String,
    /// The reason given with a denial.
    pub(crate) message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Inbox {
    pub(crate) banner: Option<Banner>,
    /// "2 things need you" or "Nothing needs you".
    pub(crate) lede: String,
    pub(crate) detail: String,
    pub(crate) held: Section<HeldRow>,
    pub(crate) history: Section<HistoryRow>,
    /// Whether the daemon takes a reason with a denial (`approval.message`): Deny then asks for
    /// one, and without it Deny is a single click.
    pub(crate) reasons: bool,
}

fn run_bot(state: &State, run: &RunId) -> (Option<String>, Option<String>, String) {
    let Some(summary) = state.run(run) else {
        return (None, None, String::new());
    };
    let parts = summary::parse_title(&summary.title);
    let bot = summary
        .bot
        .as_ref()
        .map(|b| b.name.clone())
        .or_else(|| parts.bot.map(str::to_owned));
    (
        bot,
        parts.repo.map(str::to_owned),
        summary.executor.as_str().to_owned(),
    )
}

fn held_row(state: &State, approval: &Approval, now_ms: u64) -> HeldRow {
    let (bot, repo, executor) = run_bot(state, &approval.run_id);
    HeldRow {
        run: approval.run_id.clone(),
        bot,
        repo,
        executor,
        action: summary::action_word(&approval.subject),
        tool: approval.subject.tool.clone(),
        line: summary::subject_line(&approval.subject),
        paths: approval
            .subject
            .paths
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        waited: summary::age_label(now_ms.saturating_sub(approval.requested_at_ms)),
        network: approval.subject.action == ActionClass::Network,
        approval: approval.clone(),
    }
}

fn who(by: Option<&ResolvedBy>, status: ApprovalStatus) -> String {
    match by {
        Some(ResolvedBy::Human) => "you".to_owned(),
        Some(ResolvedBy::Rule { name }) => format!("rule: {name}"),
        Some(ResolvedBy::Expired) => "nobody in time".to_owned(),
        Some(ResolvedBy::RepeatDenial) => "repeat of an earlier denial".to_owned(),
        None if status == ApprovalStatus::Expired => "nobody in time".to_owned(),
        None => "the daemon".to_owned(),
    }
}

fn history_row(state: &State, record: &ApprovalRecord, now_ms: u64) -> Option<HistoryRow> {
    let verdict = match record.status {
        ApprovalStatus::Approved => Verdict::Approved,
        ApprovalStatus::Denied => Verdict::Denied,
        ApprovalStatus::Expired => Verdict::Expired,
        // `approval.history` never returns pending ones.
        ApprovalStatus::Pending => return None,
    };
    let (bot, _, _) = run_bot(state, &record.run_id);
    let decided = record.decided_at_ms.unwrap_or(record.created_at_ms);
    Some(HistoryRow {
        run: record.run_id.clone(),
        bot,
        action: summary::action_word(&record.subject),
        line: summary::subject_line(&record.subject),
        verdict,
        by: who(record.decided_by.as_ref(), record.status),
        when: summary::ago_label(now_ms.saturating_sub(decided)),
        message: record.message.clone(),
    })
}

/// What the Inbox shows.
pub(crate) fn build(state: &State, now_ms: u64) -> Inbox {
    let held: Vec<HeldRow> = state
        .approvals()
        .iter()
        .map(|a| held_row(state, a, now_ms))
        .collect();
    let history: Vec<HistoryRow> = state
        .approval_history()
        .iter()
        .filter_map(|r| history_row(state, r, now_ms))
        .collect();
    let need = held.len();
    let live = summary::attention(state).running;
    let (lede, detail) = match need {
        0 => (
            "Nothing needs you".to_owned(),
            match live {
                0 => "No runs are moving right now.".to_owned(),
                1 => "1 run is moving on its own.".to_owned(),
                n => format!("{n} runs are moving on their own."),
            },
        ),
        1 => (
            "1 thing needs you".to_owned(),
            "A held action keeps its run waiting until you decide. Unanswered actions are denied when they expire."
                .to_owned(),
        ),
        n => (
            format!("{n} things need you"),
            "Held actions keep their runs waiting until you decide. Unanswered actions are denied when they expire."
                .to_owned(),
        ),
    };
    Inbox {
        banner: banner(state),
        lede,
        detail,
        held: Section::of(state.feed(Feed::Approvals), held),
        history: Section::of(state.feed(Feed::ApprovalHistory), history),
        reasons: state.supports(reno_proto::FEATURE_APPROVAL_MESSAGE),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_client::{FeedState, Update};
    use reno_proto::{ApprovalId, ApprovalSubject};

    use super::*;

    const HOUR: u64 = 3_600_000;
    const NOW: u64 = 1_000 * HOUR;

    fn subject(command: &str) -> ApprovalSubject {
        ApprovalSubject {
            action: ActionClass::Command,
            tool: "bash".into(),
            command: Some(command.into()),
            paths: vec![],
            raw: serde_json::Value::Null,
        }
    }

    fn pending(id: &str, run: &str, age: u64) -> Approval {
        Approval {
            id: ApprovalId::from_suffix(id).unwrap(),
            run_id: RunId::from_suffix(run).unwrap(),
            subject: subject("git push -u origin x"),
            requested_at_ms: NOW - age,
        }
    }

    fn record(id: &str, status: ApprovalStatus, by: Option<ResolvedBy>) -> ApprovalRecord {
        ApprovalRecord {
            id: ApprovalId::from_suffix(id).unwrap(),
            run_id: RunId::from_suffix("r").unwrap(),
            request_id: "req".into(),
            subject: subject("cargo test"),
            status,
            decided_by: by,
            created_at_ms: NOW - 3 * HOUR,
            decided_at_ms: Some(NOW - 2 * HOUR),
            expires_at_ms: NOW,
            message: None,
        }
    }

    #[test]
    fn loading_before_anything_arrives() {
        let inbox = build(&State::new(), NOW);
        assert_eq!(inbox.held, Section::Loading);
        assert_eq!(inbox.history, Section::Loading);
        assert!(inbox.banner.unwrap().connecting);
        assert_eq!(inbox.lede, "Nothing needs you");
    }

    #[test]
    fn empty_says_nothing_needs_you() {
        let mut state = State::new();
        state.apply(Update::Approvals(vec![]));
        let inbox = build(&state, NOW);
        assert_eq!(inbox.held, Section::Empty);
        assert_eq!(inbox.lede, "Nothing needs you");
        assert_eq!(inbox.detail, "No runs are moving right now.");
    }

    #[test]
    fn held_actions_come_longest_waiting_first() {
        let mut state = State::new();
        state.apply(Update::Approvals(vec![
            pending("new", "r", HOUR),
            pending("old", "r", 6 * HOUR),
        ]));
        let inbox = build(&state, NOW);
        assert_eq!(inbox.lede, "2 things need you");
        let rows = inbox.held.rows();
        assert_eq!(rows[0].approval.id.to_string(), "appr_old");
        assert_eq!(rows[0].waited, "6h");
        assert_eq!(rows[0].line, "git push -u origin x");
        assert_eq!(rows[0].action, "push");
        assert_eq!(rows[1].waited, "1h");
    }

    #[test]
    fn a_decision_leaves_the_list_live() {
        let mut state = State::new();
        state.apply(Update::Approvals(vec![pending("a", "r", HOUR)]));
        assert_eq!(build(&state, NOW).held.rows().len(), 1);
        state.apply(Update::Approvals(vec![]));
        let inbox = build(&state, NOW);
        assert_eq!(inbox.held, Section::Empty);
        assert_eq!(inbox.lede, "Nothing needs you");
    }

    #[test]
    fn history_names_who_decided() {
        let mut state = State::new();
        state.apply(Update::ApprovalHistory(vec![
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
        ]));
        let inbox = build(&state, NOW);
        let rows = inbox.history.rows();
        let summary: Vec<_> = rows.iter().map(|r| (r.verdict, r.by.as_str())).collect();
        assert_eq!(
            summary,
            [
                (Verdict::Approved, "you"),
                (Verdict::Denied, "rule: never reset hard"),
                (Verdict::Expired, "nobody in time"),
                (Verdict::Denied, "repeat of an earlier denial"),
            ]
        );
        assert_eq!(rows[0].when, "2h ago");
    }

    #[test]
    fn a_denial_shows_the_reason_that_was_given() {
        let mut state = State::new();
        let mut denied = record("a", ApprovalStatus::Denied, Some(ResolvedBy::Human));
        denied.message = Some("use the staging bucket".into());
        state.apply(Update::ApprovalHistory(vec![
            denied,
            record("b", ApprovalStatus::Denied, Some(ResolvedBy::Human)),
        ]));
        let inbox = build(&state, NOW);
        let rows = inbox.history.rows();
        assert_eq!(rows[0].message.as_deref(), Some("use the staging bucket"));
        assert_eq!(rows[1].message, None);
    }

    #[test]
    fn a_reason_is_asked_for_only_when_the_daemon_takes_one() {
        use reno_proto::{HelloResult, PROTOCOL_VERSION};
        let hello = |features: &[&str]| reno_client::DaemonInfo {
            hello: HelloResult {
                server_version: "t".into(),
                protocol_version: PROTOCOL_VERSION,
                features: features.iter().map(|f| (*f).to_owned()).collect(),
                pid: 1,
            },
            status: None,
        };
        let mut state = State::new();
        assert!(!build(&state, NOW).reasons, "no daemon yet");
        state.apply(Update::Daemon(hello(&["approvals"])));
        assert!(
            !build(&state, NOW).reasons,
            "an older daemon: Deny is one click"
        );
        state.apply(Update::Daemon(hello(&["approvals", "approval.message"])));
        assert!(build(&state, NOW).reasons);
    }

    #[test]
    fn unavailable_and_failed_feeds_say_so() {
        let mut state = State::new();
        state.apply(Update::Feed(Feed::Approvals, FeedState::Unavailable));
        state.apply(Update::Feed(
            Feed::ApprovalHistory,
            FeedState::Failed("busy".into()),
        ));
        let inbox = build(&state, NOW);
        assert_eq!(inbox.held, Section::Unavailable);
        assert_eq!(inbox.history, Section::Failed("busy".into()));
    }
}
