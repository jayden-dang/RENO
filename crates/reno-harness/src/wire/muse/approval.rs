//! Muse's approvals and questions as Reno's.
//!
//! # Approvals
//!
//! Muse asks (`approval/request`, and the same payload as the `approval/requested` notification
//! the run acts on) before what its permission profile does not already allow. The request names a
//! `subject`, the `availableChoices` it will take, and the `currentRequirementId`: a shell command
//! with several stages (`mkdir out && touch out/x` is two) is approved stage by stage, each its own
//! requirement, announced again by `approval/updated` as the previous one is decided.
//!
//! Reno raises one `NeedsApproval` per requirement, `request_id` `<approvalId>#<sourceIndex>`,
//! and answers it with the choice the host itself offered, never one it built (`approval/decide`
//! says so):
//!
//! - allow: the choice whose `decision` is `approved` and whose `scope` is `once`. A choice that
//!   would grant more (`approvedForSession`, `approvedPolicyAmendment`, `localPersistent`: a rule
//!   saved in Muse's own policy store) is never taken, so an allow can neither outlive the
//!   request nor write Muse's configuration. If no single-use allow is offered, the answer is a
//!   denial.
//! - deny: the choice `denied`/`once`, else `abort`/`once`; the reason goes as `feedback` when the
//!   choice takes it. If no refusal is offered the driver cannot answer, and ends the run.
//!
//! # Subjects
//!
//! | `subject.kind` | `ApprovalSubject` |
//! |---|---|
//! | `shell` | `Command`; the command is the current stage's argv when the command has several and the stage is fully parsed, else the whole command |
//! | `fileAccess` | `Edit` for `write`, `Read` for `read`; the path |
//! | `network` | `Network`; `host:port` in the raw request |
//! | `tool` | `McpTool` for a `<server>__<tool>` name, else `Other` |
//! | anything else | `Other`, never allowed by default (unknown kinds are rendered generically) |
//!
//! # Questions
//!
//! `userInput/requested` carries questions with labelled options and a `single` or `multiple`
//! selection; each becomes a [`Question`] and the answers go back as `userInput/answer` (or
//! `userInput/cancel` when the person declined).

use std::path::PathBuf;

use reno_proto::{ActionClass, ApprovalSubject, Question};
use serde_json::{Value, json};

use super::frames::string;
use crate::exec::InputAnswer;

/// One choice the host offers for an approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Choice {
    /// The id `approval/decide` takes.
    pub(crate) id: String,
    /// `approved`, `approvedForSession`, `approvedPolicyAmendment`, `denied`, `abort`, ...
    pub(crate) decision: String,
    /// `once`, `session` or `localPersistent`.
    pub(crate) scope: String,
    /// Whether the choice takes feedback text.
    pub(crate) accepts_feedback: bool,
}

/// An approval requirement the run has to decide.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Requirement {
    /// Muse's id for the approval.
    pub(crate) approval: String,
    /// The requirement to decide, verbatim (`{approvalId, sourceIndex}`): `approval/decide` must
    /// carry exactly it.
    pub(crate) requirement: Value,
    /// The id Reno's `NeedsApproval` carries.
    pub(crate) request_id: String,
    /// What is being approved.
    pub(crate) subject: ApprovalSubject,
    /// What the host takes as an answer.
    pub(crate) choices: Vec<Choice>,
}

impl Requirement {
    /// The single-use allow, when the host offers one.
    pub(crate) fn allow(&self) -> Option<&Choice> {
        self.choices
            .iter()
            .find(|c| c.decision == "approved" && c.scope == "once")
    }

    /// The refusal that grants and saves nothing.
    pub(crate) fn deny(&self) -> Option<&Choice> {
        let once = |decision: &str| {
            self.choices
                .iter()
                .find(|c| c.decision == decision && c.scope == "once")
        };
        once("denied").or_else(|| once("abort"))
    }
}

/// The requirement an `approval/requested` or `approval/updated` params asks to be decided, or
/// `None` when there is nothing to decide: no current requirement, or its stage is already
/// resolved (the update that reports a decision).
pub(crate) fn requirement(params: &Value) -> Option<Requirement> {
    let approval = string(params, "approvalId");
    let current = params
        .get("currentRequirementId")
        .filter(|r| !r.is_null())?;
    if approval.is_empty() {
        return None;
    }
    let stage = params["subject"]["stages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|stage| stage["requirementId"] == *current);
    if stage.is_some_and(|stage| string(&stage["resolution"], "kind") != "unresolved") {
        return None;
    }
    let index = current["sourceIndex"].as_u64().unwrap_or(0);
    let choices = params["availableChoices"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|choice| Choice {
            id: string(choice, "choiceId").to_owned(),
            decision: string(choice, "decision").to_owned(),
            scope: string(choice, "scope").to_owned(),
            accepts_feedback: choice["acceptsFeedback"] == true,
        })
        .filter(|choice| !choice.id.is_empty())
        .collect();
    Some(Requirement {
        approval: approval.to_owned(),
        requirement: current.clone(),
        request_id: format!("{approval}#{index}"),
        subject: subject(params, stage),
        choices,
    })
}

/// `approval/decide` params answering `requirement` with `choice`.
pub(crate) fn decide(
    session: &str,
    command: &str,
    requirement: &Requirement,
    choice: &Choice,
    feedback: Option<&str>,
) -> Value {
    let mut params = json!({
        "commandId": command,
        "sessionId": session,
        "approvalId": requirement.approval,
        "requirementId": requirement.requirement,
        "choiceId": choice.id,
    });
    if let Some(feedback) = feedback.filter(|_| choice.accepts_feedback) {
        params["feedback"] = json!(feedback);
    }
    params
}

fn subject(params: &Value, stage: Option<&Value>) -> ApprovalSubject {
    let kind = &params["subject"];
    let tool = string(params, "toolName");
    let raw = params.clone();
    match string(kind, "kind") {
        "shell" => {
            let whole = string(kind, "command");
            let staged = stage
                .filter(|s| s["totalStages"].as_u64().unwrap_or(1) > 1)
                .filter(|s| s["argvComplete"] == true)
                .and_then(|s| s["argv"].as_array())
                .map(|argv| {
                    argv.iter()
                        .filter_map(Value::as_str)
                        .map(shell_word)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|line| !line.is_empty());
            ApprovalSubject {
                action: ActionClass::Command,
                tool: if tool.is_empty() { "bash" } else { tool }.to_owned(),
                command: Some(staged.unwrap_or_else(|| whole.to_owned())),
                paths: Vec::new(),
                raw,
            }
        }
        "fileAccess" => ApprovalSubject {
            action: if string(kind, "access") == "write" {
                ActionClass::Edit
            } else {
                ActionClass::Read
            },
            tool: tool.to_owned(),
            command: None,
            paths: kind["path"]
                .as_str()
                .map(PathBuf::from)
                .into_iter()
                .collect(),
            raw,
        },
        "network" => ApprovalSubject {
            action: ActionClass::Network,
            tool: tool.to_owned(),
            command: None,
            paths: Vec::new(),
            raw,
        },
        "tool" => ApprovalSubject {
            action: super::normalize::action_class(tool),
            tool: tool.to_owned(),
            command: None,
            paths: Vec::new(),
            raw,
        },
        _ => ApprovalSubject {
            action: ActionClass::Other,
            tool: tool.to_owned(),
            command: None,
            paths: Vec::new(),
            raw,
        },
    }
}

/// One word of a command line: quoted unless it is plain.
fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=@%+,".contains(&b));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// One question of a `userInput/requested`, as Reno's.
pub(crate) fn questions(params: &Value) -> Vec<Question> {
    params["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| {
            let header = string(q, "header");
            let ask = string(q, "question");
            Question {
                id: string(q, "id").to_owned(),
                prompt: if header.is_empty() {
                    ask.to_owned()
                } else {
                    format!("{header}: {ask}")
                },
                options: q["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|o| o["label"].as_str().map(str::to_owned))
                    .collect(),
                multi_select: q["selection"]["mode"] == "multiple",
            }
        })
        .collect()
}

/// `userInput/answer` answers: offered labels select, anything else is free text (at most 500
/// characters, the host's limit). A question nobody answered declines with a placeholder: Muse
/// wants every question answered.
pub(crate) fn answers(params: &Value, given: &[InputAnswer]) -> Vec<Value> {
    questions(params)
        .iter()
        .map(|question| {
            let labels: &[String] = given
                .iter()
                .find(|a| a.question_id == question.id)
                .map_or(&[], |a| &a.labels);
            answer(question, labels)
        })
        .collect()
}

fn answer(question: &Question, labels: &[String]) -> Value {
    let (selected, free): (Vec<&String>, Vec<&String>) = labels
        .iter()
        .partition(|label| question.options.contains(label));
    let free_text: String = free
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(500)
        .collect();
    let mut answer = json!({"questionId": question.id});
    match (question.multi_select, selected.as_slice()) {
        (_, []) => {}
        (false, [first, ..]) => answer["selectedLabel"] = json!(first),
        (true, all) => answer["selectedLabels"] = json!(all),
    }
    if !free_text.is_empty() {
        answer[if selected.is_empty() {
            "freeText"
        } else {
            "note"
        }] = json!(free_text);
    } else if selected.is_empty() {
        answer["freeText"] = json!("(no answer)");
    }
    answer
}

/// `userInput/answer` params.
pub(crate) fn answer_params(
    session: &str,
    command: &str,
    input: &str,
    answers: Vec<Value>,
) -> Value {
    json!({"commandId": command, "sessionId": session, "userInputId": input, "answers": answers})
}

/// `userInput/cancel` params: Muse's own decline, which the model sees as a cancelled call.
pub(crate) fn cancel_params(session: &str, command: &str, input: &str, reason: &str) -> Value {
    json!({"commandId": command, "sessionId": session, "userInputId": input, "reason": reason})
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn req(index: u64) -> Value {
        json!({"approvalId": "ap1", "sourceIndex": index})
    }

    fn shell_params(stages: Value, current: u64, choices: Value) -> Value {
        json!({
            "approvalId": "ap1",
            "toolName": "bash",
            "currentRequirementId": req(current),
            "subject": {"kind": "shell", "command": "mkdir -p out && touch out/x", "stages": stages},
            "availableChoices": choices,
        })
    }

    fn stage(index: u64, total: u64, argv: &[&str], resolution: &str) -> Value {
        json!({"requirementId": req(index), "position": index + 1, "totalStages": total,
               "argv": argv, "argvComplete": true, "resolution": {"kind": resolution}})
    }

    fn offered() -> Value {
        json!([
            {"choiceId": "allow_once", "decision": "approved", "scope": "once", "label": "Allow once"},
            {"choiceId": "allow_session", "decision": "approvedForSession", "scope": "session", "label": "s"},
            {"choiceId": "allow_local_prefix", "decision": "approvedPolicyAmendment", "scope": "localPersistent", "label": "p"},
            {"choiceId": "abort", "decision": "abort", "scope": "once", "label": "Reject", "acceptsFeedback": true}
        ])
    }

    #[test]
    fn a_staged_command_is_decided_one_stage_at_a_time() {
        let stages = json!([
            stage(0, 2, &["mkdir", "-p", "out"], "unresolved"),
            stage(1, 2, &["touch", "out/x y"], "unresolved")
        ]);
        let first = requirement(&shell_params(stages.clone(), 0, offered())).unwrap();
        assert_eq!(first.request_id, "ap1#0");
        assert_eq!(first.requirement, req(0));
        assert_eq!(first.subject.action, ActionClass::Command);
        assert_eq!(first.subject.command.as_deref(), Some("mkdir -p out"));
        let second = requirement(&shell_params(stages, 1, offered())).unwrap();
        assert_eq!(second.request_id, "ap1#1");
        assert_eq!(second.subject.command.as_deref(), Some("touch 'out/x y'"));
        // The whole command stays in the raw request for the audit record.
        assert_eq!(
            second.subject.raw["subject"]["command"],
            "mkdir -p out && touch out/x"
        );
    }

    #[test]
    fn a_single_stage_command_is_asked_whole_and_a_resolved_stage_is_left_alone() {
        let one = json!([stage(0, 1, &["touch", "f"], "unresolved")]);
        let asked = requirement(&shell_params(one, 0, offered())).unwrap();
        assert_eq!(
            asked.subject.command.as_deref(),
            Some("mkdir -p out && touch out/x")
        );
        let resolved = json!([stage(0, 1, &["touch", "f"], "allowOnce")]);
        assert!(requirement(&shell_params(resolved, 0, offered())).is_none());
        assert!(requirement(&json!({"approvalId": "ap1"})).is_none());
        assert!(requirement(&json!({"currentRequirementId": req(0)})).is_none());
    }

    #[test]
    fn an_incomplete_stage_is_asked_as_the_whole_command() {
        let mut incomplete = stage(0, 2, &["echo", "$(x"], "unresolved");
        incomplete["argvComplete"] = json!(false);
        let params = shell_params(json!([incomplete]), 0, offered());
        let asked = requirement(&params).unwrap();
        assert_eq!(
            asked.subject.command.as_deref(),
            Some("mkdir -p out && touch out/x")
        );
    }

    #[test]
    fn only_a_single_use_choice_ever_answers() {
        let asked = requirement(&shell_params(
            json!([stage(0, 1, &["touch", "f"], "unresolved")]),
            0,
            offered(),
        ))
        .unwrap();
        assert_eq!(asked.allow().unwrap().id, "allow_once");
        assert_eq!(asked.deny().unwrap().id, "abort");
        // Only broad grants on offer: there is no allow, and a deny is still possible.
        let broad = json!([
            {"choiceId": "s", "decision": "approvedForSession", "scope": "session", "label": "s"},
            {"choiceId": "p", "decision": "approvedPolicyAmendment", "scope": "localPersistent", "label": "p"},
            {"choiceId": "dp", "decision": "deniedPolicyAmendment", "scope": "localPersistent", "label": "dp"}
        ]);
        let asked = requirement(&shell_params(
            json!([stage(0, 1, &["touch", "f"], "unresolved")]),
            0,
            broad,
        ))
        .unwrap();
        assert!(asked.allow().is_none());
        assert!(
            asked.deny().is_none(),
            "a persistent denial is a rule written to Muse"
        );
    }

    #[test]
    fn a_decision_carries_the_exact_requirement_and_feedback_only_where_taken() {
        let asked = requirement(&shell_params(
            json!([stage(0, 1, &["touch", "f"], "unresolved")]),
            0,
            offered(),
        ))
        .unwrap();
        let deny = decide("s1", "c1", &asked, asked.deny().unwrap(), Some("not here"));
        assert_eq!(deny["choiceId"], "abort");
        assert_eq!(deny["requirementId"], req(0));
        assert_eq!(deny["feedback"], "not here");
        let allow = decide("s1", "c2", &asked, asked.allow().unwrap(), Some("ignored"));
        assert_eq!(allow["choiceId"], "allow_once");
        assert!(
            allow.get("feedback").is_none(),
            "the allow takes no feedback"
        );
    }

    #[test]
    fn other_subjects_are_classed_by_what_they_touch() {
        let file = requirement(&json!({
            "approvalId": "a", "toolName": "write", "currentRequirementId": req(0),
            "subject": {"kind": "fileAccess", "path": ".git/config", "access": "write"},
            "availableChoices": offered()
        }))
        .unwrap();
        assert_eq!(file.subject.action, ActionClass::Edit);
        assert_eq!(file.subject.paths, [PathBuf::from(".git/config")]);
        let read = requirement(&json!({
            "approvalId": "a", "toolName": "read", "currentRequirementId": req(0),
            "subject": {"kind": "fileAccess", "path": "/etc/hosts", "access": "read"},
            "availableChoices": []
        }))
        .unwrap();
        assert_eq!(read.subject.action, ActionClass::Read);
        let net = requirement(&json!({
            "approvalId": "a", "toolName": "web_fetch", "currentRequirementId": req(0),
            "subject": {"kind": "network", "host": "example.com", "port": 443},
            "availableChoices": []
        }))
        .unwrap();
        assert_eq!(net.subject.action, ActionClass::Network);
        let mcp = requirement(&json!({
            "approvalId": "a", "toolName": "linear__search", "currentRequirementId": req(0),
            "subject": {"kind": "tool"}, "availableChoices": []
        }))
        .unwrap();
        assert_eq!(mcp.subject.action, ActionClass::McpTool);
        let strange = requirement(&json!({
            "approvalId": "a", "toolName": "x", "currentRequirementId": req(0),
            "subject": {"kind": "computerUseTakeover"}, "availableChoices": []
        }))
        .unwrap();
        assert_eq!(strange.subject.action, ActionClass::Other);
    }

    fn asked() -> Value {
        json!({"questions": [
            {"id": "q1", "header": "Scope", "question": "Which files?",
             "options": [{"label": "All"}, {"label": "Changed"}], "selection": {"mode": "single"}},
            {"id": "q2", "header": "", "question": "Which of these?",
             "options": [{"label": "A"}, {"label": "B"}], "selection": {"mode": "multiple"}}
        ]})
    }

    #[test]
    fn questions_carry_their_options_and_selection_mode() {
        let questions = questions(&asked());
        assert_eq!(questions[0].prompt, "Scope: Which files?");
        assert_eq!(questions[0].options, ["All", "Changed"]);
        assert!(!questions[0].multi_select);
        assert_eq!(questions[1].prompt, "Which of these?");
        assert!(questions[1].multi_select);
    }

    #[test]
    fn answers_select_labels_or_say_it_in_words_and_never_stay_silent() {
        let labels_a = vec!["Changed".to_owned()];
        let labels_b = vec!["A".to_owned(), "B".to_owned(), "and a note".to_owned()];
        let given = [
            InputAnswer {
                question_id: "q1".into(),
                labels: labels_a,
            },
            InputAnswer {
                question_id: "q2".into(),
                labels: labels_b,
            },
        ];
        let out = answers(&asked(), &given);
        assert_eq!(
            out[0],
            json!({"questionId": "q1", "selectedLabel": "Changed"})
        );
        assert_eq!(
            out[1],
            json!({"questionId": "q2", "selectedLabels": ["A", "B"], "note": "and a note"})
        );
        let free = vec!["something else".to_owned()];
        let given = [InputAnswer {
            question_id: "q1".into(),
            labels: free,
        }];
        let out = answers(&asked(), &given);
        assert_eq!(
            out[0],
            json!({"questionId": "q1", "freeText": "something else"})
        );
        assert_eq!(
            out[1],
            json!({"questionId": "q2", "freeText": "(no answer)"})
        );
        let long = vec!["x".repeat(900)];
        let given = [InputAnswer {
            question_id: "q1".into(),
            labels: long,
        }];
        assert_eq!(
            answers(&asked(), &given)[0]["freeText"]
                .as_str()
                .unwrap()
                .len(),
            500
        );
    }
}
