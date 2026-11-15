//! What the CLI prints: a transcript as plain text or as JSON lines, and one-line summaries.
//!
//! Human output is short and plain: assistant text as it arrives, then one line for each tool
//! call, result, question and message. JSON output is the protocol's own types, one object per
//! line for streams.

use std::io::{self, Write};

use reno_proto::{
    Approval, ApprovalOutcome, ApprovalRecord, DaemonStatusInfo, ExecEvent, ResolvedBy,
    RunEventItem, RunStatus, RunSummary, StopReason,
};
use serde::Serialize;

/// How long a tool call's input may be before it is cut.
const INPUT_WIDTH: usize = 100;

/// The wire name of an enum that serialises as a plain string (`succeeded`, `completed`).
pub(crate) fn wire_name<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        _ => "unknown".to_owned(),
    }
}

/// How a run ended, as the stream's `Done` told it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finished {
    pub(crate) stop: StopReason,
    pub(crate) diagnostic: Option<String>,
}

/// Writes a run's events to `W`.
#[derive(Debug)]
pub(crate) struct Transcript<W: Write> {
    out: W,
    json: bool,
    /// The last thing written was text with no newline after it.
    mid_line: bool,
}

impl<W: Write> Transcript<W> {
    pub(crate) fn new(out: W, json: bool) -> Self {
        Self {
            out,
            json,
            mid_line: false,
        }
    }

    /// Writes one event. Returns how the run ended when this was its `Done`.
    pub(crate) fn event(&mut self, item: &RunEventItem) -> io::Result<Option<Finished>> {
        let finished = match &item.event {
            ExecEvent::Done {
                stop, diagnostic, ..
            } => Some(Finished {
                stop: *stop,
                diagnostic: diagnostic.clone(),
            }),
            _ => None,
        };
        if self.json {
            serde_json::to_writer(&mut self.out, item).map_err(io::Error::other)?;
            writeln!(self.out)?;
        } else {
            self.human(&item.event)?;
        }
        self.out.flush()?;
        Ok(finished)
    }

    fn human(&mut self, event: &ExecEvent) -> io::Result<()> {
        if let ExecEvent::Text { text } = event {
            write!(self.out, "{text}")?;
            self.mid_line = !text.ends_with('\n');
            return Ok(());
        }
        // Not part of the plain transcript.
        if matches!(
            event,
            ExecEvent::Started { .. } | ExecEvent::Reasoning { .. } | ExecEvent::Usage { .. }
        ) {
            return Ok(());
        }
        self.end_line()?;
        match event {
            ExecEvent::Tool { call, .. } => {
                let input = call.input.to_string();
                if input == "null" || input == "{}" {
                    writeln!(self.out, "> {}", call.name)
                } else {
                    writeln!(self.out, "> {} {}", call.name, shorten(&input, INPUT_WIDTH))
                }
            }
            ExecEvent::ToolResult { ok, summary, .. } => {
                let outcome = if *ok { "ok" } else { "failed" };
                match summary {
                    Some(summary) => writeln!(self.out, "  {outcome}: {summary}"),
                    None => writeln!(self.out, "  {outcome}"),
                }
            }
            ExecEvent::NeedsApproval { subject, .. } => {
                let what = subject.command.as_deref().unwrap_or("");
                writeln!(self.out, "? approval needed: {} {what}", subject.tool)
            }
            ExecEvent::ApprovalResolved {
                decision,
                by,
                message,
                ..
            } => writeln!(
                self.out,
                "  {}",
                resolved_text(*decision, by, message.as_deref())
            ),
            ExecEvent::NeedsInput { questions, .. } => {
                for question in questions {
                    writeln!(self.out, "? the run asks: {}", question.prompt)?;
                }
                writeln!(self.out, "  (the CLI cannot answer questions yet)")
            }
            ExecEvent::UserMessage { text } => writeln!(self.out, "you: {text}"),
            ExecEvent::Done { stop, .. } => writeln!(self.out, "done ({})", wire_name(stop)),
            // Text and the quiet kinds returned above; a kind added later prints nothing.
            _ => Ok(()),
        }
    }

    fn end_line(&mut self) -> io::Result<()> {
        if self.mid_line {
            writeln!(self.out)?;
            self.mid_line = false;
        }
        Ok(())
    }

    /// How to answer the approvals a run is waiting on (human output only).
    pub(crate) fn approval_hint(&mut self, approvals: &[Approval]) -> io::Result<()> {
        if self.json {
            return Ok(());
        }
        for approval in approvals {
            writeln!(
                self.out,
                "  reno approve {id}   reno deny {id}",
                id = approval.id
            )?;
        }
        self.out.flush()
    }
}

/// `text` cut to `width` characters, with an ellipsis when it was longer.
fn shorten(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(width).collect();
    cut.push('…');
    cut
}

/// One run on one line: `id  status  executor  title`.
pub(crate) fn run_line(run: &RunSummary) -> String {
    format!(
        "{}  {:<16}  {:<12}  {}",
        run.run_id,
        status_name(run.status),
        run.executor.as_str(),
        run.title
    )
}

pub(crate) fn status_name(status: RunStatus) -> String {
    wire_name(&status)
}

/// Who or what decided, as a short phrase: `you`, `rule cargo is fine`, `its time limit`.
pub(crate) fn by_text(by: &ResolvedBy) -> String {
    match by {
        ResolvedBy::Human => "you".to_owned(),
        ResolvedBy::Rule { name } => format!("rule {name}"),
        ResolvedBy::Expired => "the time limit".to_owned(),
        ResolvedBy::RepeatDenial => "the repeat-denial rule".to_owned(),
    }
}

/// The transcript line of a resolved approval: `approved by rule cargo is fine`.
pub(crate) fn resolved_text(
    decision: ApprovalOutcome,
    by: &ResolvedBy,
    message: Option<&str>,
) -> String {
    let outcome = match decision {
        ApprovalOutcome::Approved => "approved",
        ApprovalOutcome::Denied => "denied",
    };
    let line = match by {
        ResolvedBy::Expired => format!("{outcome}: nobody answered in time"),
        ResolvedBy::RepeatDenial => format!("{outcome}: it was denied earlier in this run"),
        by => format!("{outcome} by {}", by_text(by)),
    };
    match message {
        Some(message) => format!("{line} ({message})"),
        None => line,
    }
}

/// One decided approval on one line: `id  run  status  by  tool command`.
pub(crate) fn history_line(record: &ApprovalRecord) -> String {
    let subject = &record.subject;
    let what = subject.command.as_deref().unwrap_or("");
    let by = record
        .decided_by
        .as_ref()
        .map_or_else(|| "nobody yet".to_owned(), by_text);
    format!(
        "{}  run {}  {:<8}  by {by}  {} {what}",
        record.id,
        record.run_id,
        wire_name(&record.status),
        subject.tool
    )
}

/// One approval on one line.
pub(crate) fn approval_line(approval: &Approval) -> String {
    let subject = &approval.subject;
    let what = subject.command.as_deref().unwrap_or("");
    format!(
        "{}  run {}  {} {what}",
        approval.id, approval.run_id, subject.tool
    )
}

/// `73` as `1m 13s`.
pub(crate) fn duration(secs: u64) -> String {
    let (hours, minutes, seconds) = (secs / 3600, secs / 60 % 60, secs % 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m {seconds:02}s"),
        _ => format!("{hours}h {minutes:02}m"),
    }
}

/// The text of `reno daemon status` for a running daemon.
pub(crate) fn status_text(info: &DaemonStatusInfo, socket: &std::path::Path) -> String {
    let executors: Vec<_> = info.executors.iter().map(|e| e.as_str()).collect();
    format!(
        "running: pid {}, renod {}, up {}\nruns: {} active, {} queued\nexecutors: {}\nsocket: {}",
        info.pid,
        info.version,
        duration(info.uptime_secs),
        info.active_runs,
        info.queued_runs,
        if executors.is_empty() {
            "none".to_owned()
        } else {
            executors.join(", ")
        },
        socket.display()
    )
}

#[cfg(test)]
mod tests {
    use reno_proto::{ActionClass, Question, Seq, ToolCall};
    use serde_json::json;

    use super::*;

    fn item(seq: u64, event: ExecEvent) -> RunEventItem {
        RunEventItem {
            seq: Seq(seq),
            event,
        }
    }

    fn render(json: bool, events: Vec<ExecEvent>) -> (String, Option<Finished>) {
        let mut out = Vec::new();
        let mut last = None;
        let mut transcript = Transcript::new(&mut out, json);
        for (n, event) in events.into_iter().enumerate() {
            last = transcript
                .event(&item(n as u64 + 1, event))
                .unwrap()
                .or(last);
        }
        (String::from_utf8(out).unwrap(), last)
    }

    fn text(s: &str) -> ExecEvent {
        ExecEvent::Text { text: s.into() }
    }

    fn done(stop: StopReason) -> ExecEvent {
        ExecEvent::Done {
            stop,
            result: None,
            diagnostic: Some("boom".into()),
        }
    }

    #[test]
    fn text_streams_as_it_arrives_and_lines_start_on_a_fresh_line() {
        let (out, finished) = render(
            false,
            vec![
                ExecEvent::Started {
                    session: reno_proto::SessionRef("s".into()),
                    model: "m".into(),
                },
                text("Hello "),
                text("world"),
                ExecEvent::Tool {
                    id: "1".into(),
                    call: ToolCall::new("grep", json!({"pattern": "x"}), ActionClass::Read),
                },
                ExecEvent::ToolResult {
                    id: "1".into(),
                    ok: false,
                    summary: Some("no match".into()),
                },
                text("bye\n"),
                done(StopReason::Completed),
            ],
        );
        assert_eq!(
            out,
            "Hello world\n> grep {\"pattern\":\"x\"}\n  failed: no match\nbye\ndone (completed)\n"
        );
        assert_eq!(
            finished,
            Some(Finished {
                stop: StopReason::Completed,
                diagnostic: Some("boom".into())
            })
        );
    }

    #[test]
    fn a_resolved_approval_says_who_decided_and_why() {
        let resolved = |decision, by, message: Option<&str>| ExecEvent::ApprovalResolved {
            request_id: "r".into(),
            decision,
            by,
            message: message.map(str::to_owned),
        };
        let (out, _) = render(
            false,
            vec![
                resolved(
                    ApprovalOutcome::Approved,
                    ResolvedBy::Rule {
                        name: "cargo is fine".into(),
                    },
                    None,
                ),
                resolved(
                    ApprovalOutcome::Denied,
                    ResolvedBy::Rule {
                        name: "force push".into(),
                    },
                    Some("denied by policy: force push"),
                ),
                resolved(ApprovalOutcome::Approved, ResolvedBy::Human, None),
                resolved(ApprovalOutcome::Denied, ResolvedBy::Expired, None),
                resolved(ApprovalOutcome::Denied, ResolvedBy::RepeatDenial, None),
            ],
        );
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            [
                "  approved by rule cargo is fine",
                "  denied by rule force push (denied by policy: force push)",
                "  approved by you",
                "  denied: nobody answered in time",
                "  denied: it was denied earlier in this run",
            ]
        );
        // JSON output is the event as it is.
        let (out, _) = render(
            true,
            vec![resolved(ApprovalOutcome::Denied, ResolvedBy::Expired, None)],
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&out).unwrap(),
            json!({"seq": 1, "event": {
                "type": "approval_resolved", "requestId": "r",
                "decision": "denied", "by": {"type": "expired"}
            }})
        );
    }

    #[test]
    fn a_history_line_names_the_decider_and_the_rule() {
        let record = |status, decided_by| ApprovalRecord {
            id: "appr_a1".parse().unwrap(),
            run_id: "run_r1".parse().unwrap(),
            request_id: "q".into(),
            subject: reno_proto::ApprovalSubject {
                action: ActionClass::Command,
                tool: "bash".into(),
                command: Some("cargo test".into()),
                paths: vec![],
                raw: json!(null),
            },
            status,
            decided_by,
            created_at_ms: 1,
            decided_at_ms: Some(2),
            expires_at_ms: 3,
        };
        assert_eq!(
            history_line(&record(
                reno_proto::ApprovalStatus::Approved,
                Some(ResolvedBy::Rule {
                    name: "cargo is fine".into()
                })
            )),
            "appr_a1  run run_r1  approved  by rule cargo is fine  bash cargo test"
        );
        assert_eq!(
            history_line(&record(
                reno_proto::ApprovalStatus::Denied,
                Some(ResolvedBy::Human)
            )),
            "appr_a1  run run_r1  denied    by you  bash cargo test"
        );
        assert_eq!(
            history_line(&record(
                reno_proto::ApprovalStatus::Expired,
                Some(ResolvedBy::Expired)
            )),
            "appr_a1  run run_r1  expired   by the time limit  bash cargo test"
        );
        assert_eq!(
            history_line(&record(
                reno_proto::ApprovalStatus::Denied,
                Some(ResolvedBy::RepeatDenial)
            )),
            "appr_a1  run run_r1  denied    by the repeat-denial rule  bash cargo test"
        );
    }

    #[test]
    fn questions_and_messages_are_one_line_each() {
        let (out, _) = render(
            false,
            vec![
                ExecEvent::NeedsInput {
                    request_id: "r".into(),
                    questions: vec![Question {
                        id: "q".into(),
                        prompt: "Which branch?".into(),
                        options: vec![],
                        multi_select: false,
                    }],
                },
                ExecEvent::UserMessage {
                    text: "go on".into(),
                },
            ],
        );
        assert!(out.contains("? the run asks: Which branch?\n"));
        assert!(out.ends_with("you: go on\n"));
    }

    #[test]
    fn json_is_one_item_per_line() {
        let (out, finished) = render(true, vec![text("hi"), done(StopReason::Interrupted)]);
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(lines[0]).unwrap(),
            json!({"seq": 1, "event": {"type": "text", "text": "hi"}})
        );
        assert_eq!(finished.unwrap().stop, StopReason::Interrupted);
    }

    #[test]
    fn long_tool_input_is_cut() {
        let long = "x".repeat(300);
        let (out, _) = render(
            false,
            vec![ExecEvent::Tool {
                id: "1".into(),
                call: ToolCall::new("t", json!(long), ActionClass::Other),
            }],
        );
        assert!(out.chars().count() < 130, "{out}");
        assert!(out.trim_end().ends_with('…'));
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(5), "5s");
        assert_eq!(duration(73), "1m 13s");
        assert_eq!(duration(3_700), "1h 01m");
    }

    #[test]
    fn status_names_are_the_wire_names() {
        assert_eq!(status_name(RunStatus::WaitingApproval), "waiting_approval");
        assert_eq!(wire_name(&StopReason::MaxTokens), "max_tokens");
    }
}
