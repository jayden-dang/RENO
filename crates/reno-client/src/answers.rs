//! The MOCKED answer contract for harness questions, shaped like the ROAD-1 decisions
//! (`question.answer`, `AnswerInvalid {reason, allowed}`, `QuestionNotPending`). The real daemon
//! has no answer method yet, so nothing but the fake daemon implements [`QuestionDesk`]; the app
//! reads and answers questions only through this one seam. ROAD-1 replaces the trait's
//! implementation (and, when the wire types exist, these structs) with the real RPC; no app
//! screen changes. No protocol or daemon change lives here.

use reno_proto::RunId;

/// What a question takes, filled by each driver in ROAD-1 (`text_allowed`, `max_text_len`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuestionRules {
    /// Whether a typed answer outside the options is taken.
    pub text_allowed: bool,
    /// The longest typed answer, in characters, where the harness caps it.
    pub max_text_len: Option<usize>,
}

impl Default for QuestionRules {
    fn default() -> Self {
        Self {
            text_allowed: true,
            max_text_len: None,
        }
    }
}

/// The labels chosen (or the text typed) for one question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionAnswer {
    pub question_id: String,
    pub labels: Vec<String>,
}

/// `answer {questionId, answers: [{questionId, labels}]}`; `[]` declines the whole set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnswerRequest {
    /// The id of the ask (today the journal's `request_id`; ROAD-1 mints a `QuestionId`).
    pub question_id: String,
    pub answers: Vec<QuestionAnswer>,
}

/// Why an answer was not taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnswerError {
    /// The answer breaks the question's rules; `allowed` lists what would be taken.
    AnswerInvalid {
        reason: String,
        allowed: Vec<String>,
    },
    /// The question was already answered, or is closed.
    QuestionNotPending,
}

/// The seam: reading a question's rules and answering it.
pub trait QuestionDesk: Send + Sync {
    /// The rules of the question `request_id` asked by `run`.
    fn rules(&self, run: &RunId, request_id: &str) -> QuestionRules;
    /// Answers it.
    fn answer(&self, run: &RunId, request: AnswerRequest) -> Result<(), AnswerError>;
}
