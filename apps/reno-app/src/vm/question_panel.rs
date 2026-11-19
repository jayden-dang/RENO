//! What the question panel shows and takes: pages, keycaps, a draft, and the checks an answer
//! must pass before it is sent (ui-brief, "Question panel"; the rules mirror ROAD-1's).

use reno_client::answers::{QuestionAnswer, QuestionRules};
use reno_proto::Question;

/// One option, with the key that picks it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OptionView {
    pub(crate) keycap: String,
    pub(crate) label: String,
}

/// One question of the ask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PageView {
    pub(crate) id: String,
    pub(crate) prompt: String,
    pub(crate) multi_select: bool,
    pub(crate) options: Vec<OptionView>,
}

/// What the panel draws for one `NeedsInput`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PanelModel {
    pub(crate) pages: Vec<PageView>,
    pub(crate) rules: QuestionRules,
}

impl PanelModel {
    pub(crate) fn new(questions: &[Question], rules: QuestionRules) -> Self {
        let pages = questions
            .iter()
            .map(|q| PageView {
                id: q.id.clone(),
                prompt: q.prompt.clone(),
                multi_select: q.multi_select,
                options: q
                    .options
                    .iter()
                    .enumerate()
                    .map(|(i, label)| OptionView {
                        keycap: (i + 1).to_string(),
                        label: label.clone(),
                    })
                    .collect(),
            })
            .collect();
        Self { pages, rules }
    }

    /// A click on an option sends at once: one question, one choice, options to click.
    pub(crate) fn sends_on_click(&self) -> bool {
        matches!(self.pages.as_slice(), [page] if !page.multi_select && !page.options.is_empty())
    }

    /// "1/2" when the ask has several questions.
    pub(crate) fn counter(&self, page: usize) -> Option<String> {
        (self.pages.len() > 1).then(|| format!("{}/{}", page + 1, self.pages.len()))
    }
}

/// Why an answer is not sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The harness takes only its options; typed text is not one.
    OptionsOnly { allowed: Vec<String> },
    /// The typed answer is longer than the harness takes.
    TooLong { max: usize },
    /// A single-choice question got several answers.
    OnlyOne { allowed: Vec<String> },
    /// A question has no answer (or an empty one).
    Missing { prompt: String },
    /// The answer names a question that was not asked.
    NoSuchQuestion { id: String },
}

impl Refusal {
    /// The line shown inline, in plain words, naming the agent.
    pub(crate) fn message(&self, agent: &str) -> String {
        match self {
            Self::OptionsOnly { allowed } | Self::OnlyOne { allowed } => {
                format!("{agent} takes only one of: {}", allowed.join(", "))
            }
            Self::TooLong { max } => format!("{agent} takes at most {max} characters"),
            Self::Missing { prompt } => format!("Answer this first: {prompt}"),
            Self::NoSuchQuestion { .. } => "That question is no longer being asked".to_owned(),
        }
    }
}

/// Checks `answers` against the asked questions and the harness's rules: what ROAD-1's daemon
/// will refuse, caught before anything is sent.
pub(crate) fn check(model: &PanelModel, answers: &[QuestionAnswer]) -> Result<(), Refusal> {
    if let Some(stray) = answers
        .iter()
        .find(|a| !model.pages.iter().any(|p| p.id == a.question_id))
    {
        return Err(Refusal::NoSuchQuestion {
            id: stray.question_id.clone(),
        });
    }
    for page in &model.pages {
        let allowed = || page.options.iter().map(|o| o.label.clone()).collect::<Vec<_>>();
        let labels = answers
            .iter()
            .find(|a| a.question_id == page.id)
            .map(|a| a.labels.as_slice())
            .filter(|labels| !labels.is_empty())
            .ok_or_else(|| Refusal::Missing {
                prompt: page.prompt.clone(),
            })?;
        if !page.multi_select && labels.len() > 1 {
            return Err(Refusal::OnlyOne { allowed: allowed() });
        }
        for label in labels {
            if page.options.iter().any(|o| &o.label == label) {
                continue;
            }
            if !model.rules.text_allowed {
                return Err(Refusal::OptionsOnly { allowed: allowed() });
            }
            if let Some(max) = model.rules.max_text_len
                && label.chars().count() > max
            {
                return Err(Refusal::TooLong { max });
            }
        }
    }
    Ok(())
}

/// Where the owner is in the ask and what they picked or typed so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Draft {
    pub(crate) page: usize,
    picks: Vec<Vec<usize>>,
    typed: Vec<String>,
}

impl Draft {
    pub(crate) fn new(model: &PanelModel) -> Self {
        Self {
            page: 0,
            picks: vec![Vec::new(); model.pages.len()],
            typed: vec![String::new(); model.pages.len()],
        }
    }

    /// Picks option `option` (0-based) on the current page: a single choice replaces, a
    /// multi-select toggles.
    pub(crate) fn pick(&mut self, model: &PanelModel, option: usize) {
        let Some(page) = model.pages.get(self.page) else {
            return;
        };
        if option >= page.options.len() {
            return;
        }
        let picks = &mut self.picks[self.page];
        if page.multi_select {
            if let Some(at) = picks.iter().position(|p| *p == option) {
                picks.remove(at);
            } else {
                picks.push(option);
            }
        } else {
            *picks = vec![option];
        }
    }

    /// What is typed on the current page.
    pub(crate) fn type_text(&mut self, text: &str) {
        if let Some(slot) = self.typed.get_mut(self.page) {
            *slot = text.to_owned();
        }
    }

    pub(crate) fn is_picked(&self, option: usize) -> bool {
        self.picks
            .get(self.page)
            .is_some_and(|picks| picks.contains(&option))
    }

    pub(crate) fn next(&mut self, model: &PanelModel) {
        if self.page + 1 < model.pages.len() {
            self.page += 1;
        }
    }

    pub(crate) fn on_last_page(&self, model: &PanelModel) -> bool {
        self.page + 1 >= model.pages.len()
    }

    /// The answers so far, one per question that has one; typed text stands in for a pick.
    pub(crate) fn answers(&self, model: &PanelModel) -> Vec<QuestionAnswer> {
        model
            .pages
            .iter()
            .enumerate()
            .filter_map(|(i, page)| {
                let text = self.typed.get(i).map_or("", |t| t.trim());
                let labels: Vec<String> = if text.is_empty() {
                    let mut picks = self.picks.get(i).cloned().unwrap_or_default();
                    picks.sort_unstable();
                    picks
                        .into_iter()
                        .filter_map(|ix| page.options.get(ix).map(|o| o.label.clone()))
                        .collect()
                } else {
                    vec![text.to_owned()]
                };
                (!labels.is_empty()).then(|| QuestionAnswer {
                    question_id: page.id.clone(),
                    labels,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(id: &str, options: &[&str], multi: bool) -> Question {
        Question {
            id: id.to_owned(),
            prompt: format!("{id}?"),
            options: options.iter().map(|o| (*o).to_owned()).collect(),
            multi_select: multi,
        }
    }

    fn model(questions: &[Question], rules: QuestionRules) -> PanelModel {
        PanelModel::new(questions, rules)
    }

    const OPEN: QuestionRules = QuestionRules {
        text_allowed: true,
        max_text_len: None,
    };

    fn ans(id: &str, labels: &[&str]) -> QuestionAnswer {
        QuestionAnswer {
            question_id: id.to_owned(),
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
        }
    }

    #[test]
    fn options_get_numbered_keycaps() {
        let m = model(&[q("c", &["Red", "Green", "Blue"], false)], OPEN);
        let caps: Vec<_> = m.pages[0]
            .options
            .iter()
            .map(|o| (o.keycap.as_str(), o.label.as_str()))
            .collect();
        assert_eq!(caps, [("1", "Red"), ("2", "Green"), ("3", "Blue")]);
        assert_eq!(m.pages[0].prompt, "c?");
    }

    #[test]
    fn a_single_choice_sends_on_click_and_nothing_else_does() {
        assert!(model(&[q("c", &["A", "B"], false)], OPEN).sends_on_click());
        assert!(!model(&[q("c", &["A", "B"], true)], OPEN).sends_on_click());
        assert!(
            !model(&[q("a", &["A"], false), q("b", &["B"], false)], OPEN).sends_on_click(),
            "several questions use Send"
        );
        assert!(!model(&[q("c", &[], false)], OPEN).sends_on_click());
    }

    #[test]
    fn several_questions_page_as_n_of_m() {
        let m = model(&[q("a", &["A"], false), q("b", &["B"], true)], OPEN);
        assert_eq!(m.counter(0).as_deref(), Some("1/2"));
        assert_eq!(m.counter(1).as_deref(), Some("2/2"));
        assert_eq!(model(&[q("a", &["A"], false)], OPEN).counter(0), None);
    }

    #[test]
    fn a_draft_picks_one_or_toggles_many_and_pages_with_next() {
        let m = model(&[q("a", &["A", "B"], false), q("b", &["X", "Y", "Z"], true)], OPEN);
        let mut d = Draft::new(&m);
        d.pick(&m, 0);
        d.pick(&m, 1);
        assert!(!d.is_picked(0) && d.is_picked(1), "a single choice replaces");
        assert!(!d.on_last_page(&m));
        d.next(&m);
        assert_eq!(d.page, 1);
        assert!(d.on_last_page(&m));
        d.pick(&m, 0);
        d.pick(&m, 2);
        d.pick(&m, 0);
        assert!(!d.is_picked(0) && d.is_picked(2), "a multi-select toggles");
        d.pick(&m, 1);
        assert_eq!(
            d.answers(&m),
            [ans("a", &["B"]), ans("b", &["Y", "Z"])],
            "labels come in the order of the options"
        );
        d.next(&m);
        assert_eq!(d.page, 1, "no page past the last");
    }

    #[test]
    fn typed_text_is_the_answer_in_place_of_a_pick() {
        let m = model(&[q("a", &["A", "B"], false)], OPEN);
        let mut d = Draft::new(&m);
        d.pick(&m, 0);
        d.type_text("something else");
        assert_eq!(d.answers(&m), [ans("a", &["something else"])]);
        d.type_text("   ");
        assert_eq!(d.answers(&m), [ans("a", &["A"])], "blank text is no text");
    }

    #[test]
    fn typed_text_is_refused_where_the_harness_takes_only_its_options() {
        let closed = QuestionRules {
            text_allowed: false,
            max_text_len: None,
        };
        let m = model(&[q("f", &["JSON lines", "Plain text"], false)], closed);
        let refused = check(&m, &[ans("f", &["yaml please"])]).unwrap_err();
        assert_eq!(
            refused,
            Refusal::OptionsOnly {
                allowed: vec!["JSON lines".into(), "Plain text".into()]
            }
        );
        assert_eq!(
            refused.message("Grok"),
            "Grok takes only one of: JSON lines, Plain text"
        );
        assert_eq!(check(&m, &[ans("f", &["Plain text"])]), Ok(()));
    }

    #[test]
    fn typed_text_has_a_length_cap_where_the_harness_sets_one() {
        let capped = QuestionRules {
            text_allowed: true,
            max_text_len: Some(500),
        };
        let m = model(&[q("a", &["A"], false)], capped);
        assert_eq!(check(&m, &[ans("a", &[&"x".repeat(500)])]), Ok(()));
        let refused = check(&m, &[ans("a", &[&"x".repeat(501)])]).unwrap_err();
        assert_eq!(refused, Refusal::TooLong { max: 500 });
        assert_eq!(refused.message("Muse"), "Muse takes at most 500 characters");
    }

    #[test]
    fn a_single_choice_takes_one_answer_and_every_question_needs_one() {
        let m = model(&[q("a", &["A", "B"], false), q("b", &["X"], true)], OPEN);
        assert_eq!(
            check(&m, &[ans("a", &["A", "B"]), ans("b", &["X"])]),
            Err(Refusal::OnlyOne {
                allowed: vec!["A".into(), "B".into()]
            })
        );
        assert_eq!(
            check(&m, &[ans("a", &["A"])]),
            Err(Refusal::Missing { prompt: "b?".into() })
        );
        assert_eq!(
            check(&m, &[ans("a", &["A"]), ans("b", &[])]),
            Err(Refusal::Missing { prompt: "b?".into() })
        );
        assert_eq!(
            check(&m, &[ans("a", &["A"]), ans("b", &["X"]), ans("zz", &["X"])]),
            Err(Refusal::NoSuchQuestion { id: "zz".into() })
        );
        assert_eq!(check(&m, &[ans("a", &["B"]), ans("b", &["X"])]), Ok(()));
    }
}
