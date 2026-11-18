//! Extension dialogs as questions.
//!
//! An extension (Pi's own `/mcp` built-in, or one the owner installed) can ask the person
//! something through `ctx.ui`: in RPC mode a `select`, `confirm`, `input` or `editor` request on
//! stdout that waits for an `extension_ui_response` with the same id. Reno turns each into a
//! `NeedsInput` with one [`Question`] and the answer back into the response Pi expects. A
//! declined question, a run that ends first and a timeout all cancel the dialog.
//!
//! The fire-and-forget methods (`notify`, `setStatus`, `setWidget`, `setTitle`,
//! `set_editor_text`) need no answer and have no place in an `ExecEvent`; they are dropped.

use std::time::Duration;

use reno_proto::Question;
use serde_json::Value;

use super::frames::{string, ui_cancelled, ui_confirmed, ui_value};

/// The id of the one question every dialog carries.
pub(crate) const QUESTION_ID: &str = "answer";

/// What kind of answer Pi wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// One of the offered options: the response carries `value`.
    Select,
    /// Yes or no: the response carries `confirmed`.
    Confirm,
    /// Free text: the response carries `value`.
    Input,
}

/// One open dialog.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Dialog {
    /// Pi's id for it, echoed in the response.
    pub(crate) id: Value,
    /// The id Reno's `NeedsInput` carries.
    pub(crate) request_id: String,
    /// What it asks for.
    pub(crate) kind: Kind,
    /// The question to put to a person.
    pub(crate) question: Question,
    /// When Pi gives up waiting for an answer itself (`timeout`, in milliseconds); none when it
    /// waits for ever.
    pub(crate) timeout: Option<Duration>,
}

/// The dialog `frame` is, when it is a dialog that waits for an answer. `Err` carries the
/// response that dismisses it, for a dialog that has already expired (a negative `timeout`).
pub(crate) fn parse(frame: &Value) -> Option<Result<Dialog, Value>> {
    let kind = match string(frame, "method") {
        "select" => Kind::Select,
        "confirm" => Kind::Confirm,
        "input" | "editor" => Kind::Input,
        _ => return None,
    };
    let id = frame.get("id")?.clone();
    // Pi's `timeout` is in milliseconds; 0 means none and a negative one has already expired, so
    // the dialog is over before it could be shown. One too large for a `Duration` is as good as
    // none.
    let timeout = match frame.get("timeout").and_then(Value::as_f64) {
        Some(ms) if ms < 0.0 => return Some(Err(ui_cancelled(&id))),
        Some(ms) if ms > 0.0 => Duration::try_from_secs_f64(ms / 1000.0).ok(),
        _ => None,
    };
    let title = string(frame, "title");
    let message = string(frame, "message");
    let options: Vec<String> = if kind == Kind::Confirm {
        vec!["Yes".into(), "No".into()]
    } else {
        frame
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|option| option.as_str().map(str::to_owned))
            .collect()
    };
    let request_id = match &id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    Some(Ok(Dialog {
        id,
        request_id,
        kind,
        question: Question {
            id: QUESTION_ID.into(),
            prompt: if message.is_empty() {
                title.to_owned()
            } else {
                format!("{title}\n{message}")
            },
            options,
            multi_select: false,
        },
        timeout,
    }))
}

/// The `extension_ui_response` for a person's answer: `label` is the option chosen or the text
/// typed, `None` a declined question.
pub(crate) fn response(dialog: &Dialog, label: Option<&str>) -> Value {
    match (dialog.kind, label) {
        (_, None) => ui_cancelled(&dialog.id),
        (Kind::Confirm, Some(label)) => ui_confirmed(&dialog.id, label == "Yes"),
        (_, Some(label)) => ui_value(&dialog.id, label),
    }
}

/// The response that dismisses `dialog`.
pub(crate) fn dismissed(dialog: &Dialog) -> Value {
    ui_cancelled(&dialog.id)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    fn open(frame: Value) -> Dialog {
        parse(&frame).unwrap().unwrap()
    }

    #[test]
    fn a_select_is_a_question_with_its_options() {
        let dialog = open(json!({
            "type": "extension_ui_request", "id": "u1", "method": "select",
            "title": "Allow dangerous command?", "options": ["Allow", "Block"], "timeout": 10000
        }));
        assert_eq!(dialog.request_id, "u1");
        assert_eq!(dialog.question.options, ["Allow", "Block"]);
        assert_eq!(dialog.question.prompt, "Allow dangerous command?");
        assert_eq!(dialog.timeout, Some(Duration::from_secs(10)));
        assert_eq!(
            response(&dialog, Some("Block")),
            json!({ "type": "extension_ui_response", "id": "u1", "value": "Block" })
        );
    }

    #[test]
    fn a_confirm_answers_with_a_boolean_and_a_message_joins_the_title() {
        let dialog = open(json!({
            "type": "extension_ui_request", "id": "u2", "method": "confirm",
            "title": "Clear session?", "message": "All messages will be lost."
        }));
        assert_eq!(dialog.question.options, ["Yes", "No"]);
        assert_eq!(
            dialog.question.prompt,
            "Clear session?\nAll messages will be lost."
        );
        assert_eq!(dialog.timeout, None);
        assert_eq!(response(&dialog, Some("Yes"))["confirmed"], true);
        assert_eq!(response(&dialog, Some("No"))["confirmed"], false);
    }

    #[test]
    fn input_and_editor_are_free_text_and_a_declined_question_cancels() {
        for method in ["input", "editor"] {
            let dialog = open(json!({
                "type": "extension_ui_request", "id": "u3", "method": method, "title": "Name?"
            }));
            assert!(dialog.question.options.is_empty());
            assert_eq!(response(&dialog, Some("Ada"))["value"], "Ada");
            assert_eq!(response(&dialog, None)["cancelled"], true);
            assert_eq!(dismissed(&dialog)["cancelled"], true);
        }
    }

    #[test]
    fn a_dialog_that_already_expired_is_dismissed_unasked() {
        let expired = parse(&json!({
            "type": "extension_ui_request", "id": "u4", "method": "select",
            "title": "x", "options": [], "timeout": -5
        }))
        .unwrap();
        assert_eq!(expired.unwrap_err()["cancelled"], true);
    }

    #[test]
    fn fire_and_forget_methods_are_not_dialogs() {
        for method in [
            "notify",
            "setStatus",
            "setWidget",
            "setTitle",
            "set_editor_text",
            "future",
        ] {
            assert!(
                parse(&json!({ "type": "extension_ui_request", "id": "n", "method": method }))
                    .is_none(),
                "{method}"
            );
        }
    }
}
