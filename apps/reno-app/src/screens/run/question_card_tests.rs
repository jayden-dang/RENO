//! The question card draws how its question ended, not only that it was asked.

use gpui::{Context, TestAppContext, Window};
use reno_proto::Question;

use super::*;
use crate::transcript::Resolution;
use crate::ui::theme::Appearance;

struct Card(Theme, Resolution);

impl Render for Card {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let questions = [Question {
            id: "c".into(),
            prompt: "Which colour?".into(),
            options: vec!["Red".into(), "Blue".into()],
            multi_select: false,
        }];
        div()
            .w(px(600.0))
            .child(question_card(&self.0, &questions, &self.1))
    }
}

/// The selectors painted for a card in `resolution`.
fn painted_for(cx: &mut TestAppContext, resolution: Resolution) -> Vec<&'static str> {
    crate::tests::one_window_test_at_a_time();
    let theme = Theme::for_appearance(Appearance::Dark);
    let (_view, vcx) = cx.add_window_view(|_, _| Card(theme, resolution));
    crate::tests::draw(vcx);
    [
        "question-pending-note",
        "question-tag-declined",
        "question-tag-answered",
        "question-tag-unanswered",
        "question-declined-line",
        "question-chosen-chip",
        "question-dimmed-chip",
    ]
    .into_iter()
    .filter(|s| crate::tests::painted(vcx, s))
    .collect()
}

#[gpui::test]
fn a_pending_card_keeps_its_note_and_has_no_status_tag(cx: &mut TestAppContext) {
    assert_eq!(
        painted_for(cx, Resolution::Pending),
        ["question-pending-note"]
    );
}

#[gpui::test]
fn a_declined_card_says_so_and_dims_the_options(cx: &mut TestAppContext) {
    assert_eq!(
        painted_for(cx, Resolution::Declined),
        [
            "question-tag-declined",
            "question-declined-line",
            "question-dimmed-chip"
        ]
    );
}

#[gpui::test]
fn an_answered_card_highlights_the_chosen_option(cx: &mut TestAppContext) {
    assert_eq!(
        painted_for(cx, Resolution::Answered(vec![Some("Blue".into())])),
        [
            "question-tag-answered",
            "question-chosen-chip",
            "question-dimmed-chip"
        ]
    );
}

#[gpui::test]
fn an_unanswered_card_is_tagged_not_answered(cx: &mut TestAppContext) {
    assert_eq!(
        painted_for(cx, Resolution::Unanswered),
        ["question-tag-unanswered", "question-dimmed-chip"]
    );
}

#[test]
fn the_question_mark_is_warning_only_while_the_question_waits() {
    let theme = Theme::for_appearance(Appearance::Dark);
    assert_eq!(question_icon_tone(&theme, &Resolution::Pending), theme.warning);
    assert_eq!(question_icon_tone(&theme, &Resolution::Declined), theme.faint);
    assert_eq!(question_icon_tone(&theme, &Resolution::Unanswered), theme.faint);
    assert_eq!(
        question_icon_tone(&theme, &Resolution::Answered(vec![])),
        theme.success
    );
}
