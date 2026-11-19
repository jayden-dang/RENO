//! The fake daemon's side of the mocked answer contract (see [`crate::answers`]): which
//! questions take what, the checks ROAD-1's daemon will make, and the journal an answer leaves.

use reno_proto::{ExecEvent, Question, RunId, RunStatus};

use super::{FakeDaemon, data};
use crate::answers::{AnswerError, AnswerRequest, QuestionDesk, QuestionRules};

/// The longest typed answer a Muse-like agent takes; the demo's retry-policy question has it.
const MUSE_MAX_TEXT: usize = 500;

fn rules_of(request_id: &str) -> QuestionRules {
    match request_id {
        // An ACP agent (Grok) takes only the options it offered.
        "req-grok-format" => QuestionRules {
            text_allowed: false,
            max_text_len: None,
        },
        "req-asker-policy" => QuestionRules {
            text_allowed: true,
            max_text_len: Some(MUSE_MAX_TEXT),
        },
        _ => QuestionRules::default(),
    }
}

fn invalid(reason: &str, allowed: Vec<String>) -> AnswerError {
    AnswerError::AnswerInvalid {
        reason: reason.to_owned(),
        allowed,
    }
}

fn check_one(
    question: &Question,
    rules: QuestionRules,
    labels: &[String],
) -> Result<(), AnswerError> {
    let allowed = question.options.clone();
    if labels.is_empty() {
        return Err(invalid("an answer needs a choice", allowed));
    }
    if !question.multi_select && labels.len() > 1 {
        return Err(invalid("this question takes only one answer", allowed));
    }
    for label in labels {
        if question.options.contains(label) {
            continue;
        }
        if !rules.text_allowed {
            return Err(invalid("this question takes only its options", allowed));
        }
        if rules.max_text_len.is_some_and(|max| label.chars().count() > max) {
            return Err(invalid("the answer is too long", Vec::new()));
        }
    }
    Ok(())
}

/// The asking `NeedsInput` of `request_id`, when nothing has resolved it since.
fn pending_questions(journal: &[ExecEvent], request_id: &str) -> Option<Vec<Question>> {
    let at = journal.iter().rposition(
        |e| matches!(e, ExecEvent::NeedsInput { request_id: r, .. } if r == request_id),
    )?;
    let closed = journal[at + 1..].iter().any(|e| {
        matches!(
            e,
            ExecEvent::InputExpired { .. } | ExecEvent::ToolResult { .. } | ExecEvent::Done { .. }
        )
    });
    match (&journal[at], closed) {
        (ExecEvent::NeedsInput { questions, .. }, false) => Some(questions.clone()),
        _ => None,
    }
}

impl QuestionDesk for FakeDaemon {
    fn rules(&self, _: &RunId, request_id: &str) -> QuestionRules {
        rules_of(request_id)
    }

    fn answer(&self, run: &RunId, request: AnswerRequest) -> Result<(), AnswerError> {
        self.shared.with(|world| {
            let questions = world
                .run(run)
                .and_then(|r| pending_questions(&r.journal, &request.question_id))
                .ok_or(AnswerError::QuestionNotPending)?;
            let rules = rules_of(&request.question_id);
            if request.answers.is_empty() {
                // `[]` declines the whole set.
                world.push(
                    run,
                    ExecEvent::InputExpired {
                        request_id: request.question_id.clone(),
                    },
                );
                world.set_status(run, RunStatus::Running);
                return Ok(());
            }
            let mut said = Vec::new();
            for question in &questions {
                let answer = request
                    .answers
                    .iter()
                    .find(|a| a.question_id == question.id)
                    .ok_or_else(|| invalid("a question has no answer", question.options.clone()))?;
                check_one(question, rules, &answer.labels)?;
                said.push(answer.labels.iter().map(|l| format!("\"{l}\"")).collect::<Vec<_>>());
            }
            let summary = format!(
                "User answered: {}",
                said.iter().map(|s| s.join(", ")).collect::<Vec<_>>().join("; ")
            );
            let id = format!("ask-{}", request.question_id);
            world.push(run, data::tool(&id, "AskUserQuestion", serde_json::json!({})));
            world.push(run, data::tool_ok(&id, &summary));
            world.set_status(run, RunStatus::Running);
            Ok(())
        })
    }
}
