//! The requests a native executor has raised and not yet had answered.
//!
//! A harness asks two different things, and a different party answers each: `NeedsApproval`
//! goes to the policy engine or a human and is answered with a [`Decision`]; `NeedsInput` goes to
//! whoever the agent is asking and is answered with [`InputAnswer`]s. [`OpenRequests`] keeps one
//! table for both and enforces the rules every driver shares:
//!
//! - an answer of the wrong kind never resolves a request, and never approves one: the request
//!   stays open;
//! - an answer for an unknown or already resolved request is reported, not guessed at;
//! - when the run ends or is cancelled every open request is handed back by
//!   [`OpenRequests::drain`], so the driver can refuse each one (ARCH-15: nothing is allowed by
//!   default).
//!
//! The payload types are the driver's own: what it needs to answer the harness later (Claude
//! keeps the tool input to echo back in its allow response, for example).

use std::collections::HashMap;

use super::{Answer, Decision, ExecAnswer, InputAnswer};

/// One open request.
#[derive(Debug, PartialEq, Eq)]
pub enum Open<A, I> {
    /// A `NeedsApproval`, with the driver's payload.
    Approval(A),
    /// A `NeedsInput`, with the driver's payload.
    Input(I),
}

/// What [`OpenRequests::resolve`] did with an answer.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolved<A, I> {
    /// A decision closed an approval.
    Decision(A, Decision),
    /// Answers closed an input request. An empty list declines it.
    Input(I, Vec<InputAnswer>),
    /// The request exists, but this answer is of the other kind. It stays open.
    WrongKind,
    /// No such request: it was answered already, or never raised.
    Unknown,
}

/// The table of open requests of one run, keyed by the `request_id` the consumer sees.
#[derive(Debug)]
pub struct OpenRequests<A, I> {
    open: HashMap<String, Open<A, I>>,
}

impl<A, I> Default for OpenRequests<A, I> {
    fn default() -> Self {
        Self {
            open: HashMap::new(),
        }
    }
}

impl<A, I> OpenRequests<A, I> {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an approval the run is waiting on. A repeated id replaces the earlier request,
    /// which is handed back so the driver can refuse it.
    pub fn open_approval(
        &mut self,
        request_id: impl Into<String>,
        payload: A,
    ) -> Option<Open<A, I>> {
        self.open.insert(request_id.into(), Open::Approval(payload))
    }

    /// Record an input request the run is waiting on. See [`Self::open_approval`].
    pub fn open_input(&mut self, request_id: impl Into<String>, payload: I) -> Option<Open<A, I>> {
        self.open.insert(request_id.into(), Open::Input(payload))
    }

    /// Apply one answer.
    pub fn resolve(&mut self, answer: ExecAnswer) -> Resolved<A, I> {
        let ExecAnswer { request_id, answer } = answer;
        match (self.open.remove(&request_id), answer) {
            (Some(Open::Approval(payload)), Answer::Decision(decision)) => {
                Resolved::Decision(payload, decision)
            }
            (Some(Open::Input(payload)), Answer::Input(answers)) => {
                Resolved::Input(payload, answers)
            }
            (Some(kept), _) => {
                self.open.insert(request_id, kept);
                Resolved::WrongKind
            }
            (None, _) => Resolved::Unknown,
        }
    }

    /// Whether anything is waiting.
    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    /// How many requests are waiting, for a driver that caps them (ARCH-11).
    pub fn len(&self) -> usize {
        self.open.len()
    }

    /// Take every open request, in no particular order, leaving the table empty.
    pub fn drain(&mut self) -> Vec<(String, Open<A, I>)> {
        self.open.drain().collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn answer(id: &str, answer: Answer) -> ExecAnswer {
        ExecAnswer {
            request_id: id.into(),
            answer,
        }
    }

    fn input(label: &str) -> Answer {
        Answer::Input(vec![InputAnswer {
            question_id: "q".into(),
            labels: vec![label.into()],
        }])
    }

    #[test]
    fn a_decision_closes_an_approval_and_input_closes_a_question() {
        let mut table: OpenRequests<&str, &str> = OpenRequests::new();
        table.open_approval("a", "tool input");
        table.open_input("q", "questions");
        assert_eq!(
            table.resolve(answer("a", Answer::Decision(Decision::Allow))),
            Resolved::Decision("tool input", Decision::Allow)
        );
        assert!(matches!(
            table.resolve(answer("q", input("B"))),
            Resolved::Input("questions", given) if given[0].labels == ["B"]
        ));
        assert!(table.is_empty());
    }

    #[test]
    fn a_wrong_kind_of_answer_never_resolves_and_never_approves() {
        let mut table: OpenRequests<(), ()> = OpenRequests::new();
        table.open_approval("a", ());
        table.open_input("q", ());
        // "Yes" typed as an input answer is not a decision.
        assert_eq!(
            table.resolve(answer("a", input("Yes"))),
            Resolved::WrongKind
        );
        assert_eq!(
            table.resolve(answer("q", Answer::Decision(Decision::Allow))),
            Resolved::WrongKind
        );
        // Both are still open, and are handed back to be refused at the end.
        let mut left: Vec<_> = table.drain().into_iter().map(|(id, _)| id).collect();
        left.sort();
        assert_eq!(left, ["a", "q"]);
        assert!(table.is_empty());
    }

    #[test]
    fn an_unknown_or_repeated_answer_is_reported() {
        let mut table: OpenRequests<(), ()> = OpenRequests::new();
        assert_eq!(
            table.resolve(answer("nope", Answer::Decision(Decision::Deny))),
            Resolved::Unknown
        );
        table.open_approval("a", ());
        assert!(matches!(
            table.resolve(answer("a", Answer::Decision(Decision::Deny))),
            Resolved::Decision((), Decision::Deny)
        ));
        assert_eq!(
            table.resolve(answer("a", Answer::Decision(Decision::Allow))),
            Resolved::Unknown
        );
    }

    #[test]
    fn a_repeated_request_id_hands_back_the_one_it_replaces() {
        let mut table: OpenRequests<u8, ()> = OpenRequests::new();
        assert_eq!(table.open_approval("a", 1), None);
        assert_eq!(table.open_approval("a", 2), Some(Open::Approval(1)));
    }
}
