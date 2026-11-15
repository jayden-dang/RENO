//! `session/request_permission` as data: what the request is about, which access mode lets it
//! through unasked, and the outcomes the agent reads back.
//!
//! Sans-IO (ARCH-6). Nothing here reads prose. An approval is an [`ApprovalSubject`] built from
//! the request's `toolCall` (its `kind`, `rawInput`, `locations`), and a decision selects one of
//! the request's own `options` by its `kind`.
//!
//! # Tool kind to action class
//!
//! The same table classifies a tool call that is merely reported (`ToolCall::class`) and one
//! that asks for approval.
//!
//! | ACP `kind` | [`ActionClass`] | Subject carries |
//! |---|---|---|
//! | `execute` | `Command` | `command` from `rawInput.command`, when present |
//! | `edit`, `delete`, `move` | `Edit` | `paths` from `locations` |
//! | `fetch` | `Network` | |
//! | `read`, `search` | `Read` | `paths` from `locations` |
//! | `think` | `Other`: nothing in [`ActionClass`] fits, and it is never asked (see below) | |
//! | `other`, `switch_mode`, an unknown kind, no kind | `Other` | |
//!
//! Every subject also names the tool and keeps the whole request as the agent sent it (`raw`).
//!
//! # What an access mode lets through
//!
//! [`allowed_without_asking`] is the one table, and it is part of the executor's documented
//! behavior (see `AcpExecutor`):
//!
//! | `kind` | `FullAccess` | `AutoAcceptEdits` | `Supervised` |
//! |---|---|---|---|
//! | `read`, `search`, `think` | allowed | allowed | allowed |
//! | `edit`, `delete`, `move` | allowed | allowed | asked |
//! | anything else, or no kind | allowed | asked | asked |
//!
//! Looking around is allowed in every mode because an agent that
//! cannot read or search cannot work. "Allowed" is answered with the `allow_once` option and
//! never with `allow_always`: Reno decides every time.
//!
//! # Questions
//!
//! A request whose options are not all allow/reject kinds is an agent asking a question, not a
//! tool asking for permission. It becomes `NeedsInput` in every mode: nobody can approve it on
//! the person's behalf.

use std::path::PathBuf;

use reno_proto::{AccessMode, ActionClass, ApprovalSubject, Question};
use serde_json::{Value, json};

use crate::exec::InputAnswer;

/// The action class of an ACP tool `kind`.
pub(crate) fn action_class(kind: Option<&str>) -> ActionClass {
    match kind {
        Some("execute") => ActionClass::Command,
        Some("edit" | "delete" | "move") => ActionClass::Edit,
        Some("fetch") => ActionClass::Network,
        Some("read" | "search") => ActionClass::Read,
        _ => ActionClass::Other,
    }
}

/// Whether `access` lets a tool of `kind` through without asking. The table is in the module
/// docs.
pub(crate) fn allowed_without_asking(access: AccessMode, kind: Option<&str>) -> bool {
    match access {
        AccessMode::FullAccess => true,
        AccessMode::AutoAcceptEdits | AccessMode::Supervised => match kind {
            Some("read" | "search" | "think") => true,
            Some("edit" | "delete" | "move") => access == AccessMode::AutoAcceptEdits,
            _ => false,
        },
    }
}

/// The `kind` of a request's or update's tool call.
pub(crate) fn kind_of(tool_call: &Value) -> Option<&str> {
    tool_call.get("kind").and_then(Value::as_str)
}

/// The name a tool call is reported under: its `kind`, or, for a call whose kind says nothing
/// (`other`, missing), its `title`.
pub(crate) fn tool_name(tool_call: &Value) -> String {
    let kind = kind_of(tool_call);
    let title = tool_call
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty());
    match (kind, title) {
        (Some(kind), _) if kind != "other" => kind.to_owned(),
        (_, Some(title)) => title.to_owned(),
        (Some(kind), None) => kind.to_owned(),
        (None, None) => "tool".to_owned(),
    }
}

/// The command of an `execute` call, from `rawInput.command`: a string, or an argv array.
fn command_of(tool_call: &Value) -> Option<String> {
    let command = tool_call.get("rawInput")?.get("command")?;
    let line = match command {
        Value::String(line) => line.clone(),
        Value::Array(argv) => argv
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()?
            .join(" "),
        _ => return None,
    };
    (!line.trim().is_empty()).then_some(line)
}

/// Every `locations[].path` of a tool call.
fn locations(tool_call: &Value) -> Vec<PathBuf> {
    tool_call
        .get("locations")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|location| location.get("path").and_then(Value::as_str))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// The subject of a permission request: `params` is the request exactly as the agent sent it,
/// kept as `raw` for the audit record.
pub(crate) fn subject(params: &Value) -> ApprovalSubject {
    let tool_call = params.get("toolCall").unwrap_or(&Value::Null);
    let action = action_class(kind_of(tool_call));
    ApprovalSubject {
        action,
        tool: tool_name(tool_call),
        command: (action == ActionClass::Command)
            .then(|| command_of(tool_call))
            .flatten(),
        paths: match action {
            ActionClass::Edit | ActionClass::Read => locations(tool_call),
            _ => Vec::new(),
        },
        raw: params.clone(),
    }
}

/// The request's `options`.
pub(crate) fn options(params: &Value) -> Vec<Value> {
    params
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// A permission request is a question, not a tool permission, when any of its options lacks an
/// allow/reject kind: that is how an agent relays a user-facing choice. Kinds may repeat in a
/// real tool permission (two `allow_always` options).
pub(crate) fn is_question(options: &[Value]) -> bool {
    options.iter().any(|option| {
        !matches!(
            option.get("kind").and_then(Value::as_str),
            Some("allow_once" | "allow_always" | "reject_once" | "reject_always")
        )
    })
}

/// The outcome of a request nobody answers: the agent treats the call as cancelled.
pub(crate) fn cancelled() -> Value {
    json!({ "outcome": { "outcome": "cancelled" } })
}

fn selected(option_id: &str) -> Value {
    json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
}

/// [`outcome`], with the reason for a refusal. ACP's permission response has no message field,
/// so the reason rides in the response's `_meta` (the protocol's extension point on every type)
/// as `denyMessage`; an agent that does not read it ignores it. An allow carries none.
pub(crate) fn outcome_with_reason(options: &[Value], allow: bool, reason: Option<&str>) -> Value {
    let mut response = outcome(options, allow);
    if let Some(reason) = reason.map(str::trim).filter(|r| !allow && !r.is_empty()) {
        response["_meta"] = json!({ "denyMessage": reason });
    }
    response
}

/// The answer to a tool permission: yes selects `allow_once`, never `allow_always`, which would
/// stop the agent asking; no selects `reject_once`, else `reject_always`. Without such an option
/// the request is `cancelled`, never answered with another one.
pub(crate) fn outcome(options: &[Value], allow: bool) -> Value {
    let kinds: &[&str] = if allow {
        &["allow_once"]
    } else {
        &["reject_once", "reject_always"]
    };
    kinds
        .iter()
        .find_map(|kind| {
            options
                .iter()
                .find(|option| option.get("kind").and_then(Value::as_str) == Some(*kind))
        })
        .and_then(|option| option.get("optionId").and_then(Value::as_str))
        .map_or_else(cancelled, selected)
}

/// The question an option set stands for: its prompt is the tool call's `title`, its choices the
/// options' names.
pub(crate) fn question(question_id: &str, params: &Value, options: &[Value]) -> Question {
    let prompt = params
        .get("toolCall")
        .and_then(|call| call.get("title"))
        .and_then(Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("The agent needs your input.");
    Question {
        id: question_id.to_owned(),
        prompt: prompt.to_owned(),
        options: options
            .iter()
            .map(|option| {
                option
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect(),
        multi_select: false,
    }
}

/// The outcome for answers to [`question`]: the option whose name is the first label chosen for
/// the question, else `cancelled`.
pub(crate) fn answer_outcome(
    question_id: &str,
    options: &[Value],
    answers: &[InputAnswer],
) -> Value {
    answers
        .iter()
        .find(|answer| answer.question_id == question_id)
        .and_then(|answer| answer.labels.first())
        .and_then(|label| {
            options
                .iter()
                .find(|option| option.get("name").and_then(Value::as_str) == Some(label.as_str()))
        })
        .and_then(|option| option.get("optionId").and_then(Value::as_str))
        .map_or_else(cancelled, selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(tool_call: Value) -> Value {
        json!({ "sessionId": "s", "toolCall": tool_call, "options": [] })
    }

    #[test]
    fn kinds_map_to_action_classes() {
        for (kind, class) in [
            ("execute", ActionClass::Command),
            ("edit", ActionClass::Edit),
            ("delete", ActionClass::Edit),
            ("move", ActionClass::Edit),
            ("fetch", ActionClass::Network),
            ("read", ActionClass::Read),
            ("search", ActionClass::Read),
            ("think", ActionClass::Other),
            ("other", ActionClass::Other),
            ("switch_mode", ActionClass::Other),
            ("something_new", ActionClass::Other),
        ] {
            assert_eq!(action_class(Some(kind)), class, "{kind}");
        }
        assert_eq!(action_class(None), ActionClass::Other);
    }

    #[test]
    fn an_execute_subject_carries_the_command_and_no_paths() {
        let params = request(json!({
            "toolCallId": "t", "kind": "execute", "title": "Run it",
            "rawInput": { "command": "rm -rf build" },
            "locations": [{ "path": "/w/ignored" }],
        }));
        let subject = subject(&params);
        assert_eq!(subject.action, ActionClass::Command);
        assert_eq!(subject.tool, "execute");
        assert_eq!(subject.command.as_deref(), Some("rm -rf build"));
        assert!(subject.paths.is_empty());
        assert_eq!(subject.raw, params);
    }

    #[test]
    fn a_command_given_as_argv_is_joined_and_a_missing_one_is_none() {
        let argv = request(json!({
            "kind": "execute", "rawInput": { "command": ["cargo", "test", "-p", "x"] },
        }));
        assert_eq!(subject(&argv).command.as_deref(), Some("cargo test -p x"));
        for raw in [
            json!({ "kind": "execute" }),
            json!({ "kind": "execute", "rawInput": { "command": "  " } }),
            json!({ "kind": "execute", "rawInput": { "command": 4 } }),
            // The title is prose; it is never read as a command.
            json!({ "kind": "execute", "title": "cargo test" }),
        ] {
            assert_eq!(subject(&request(raw)).command, None);
        }
    }

    #[test]
    fn an_edit_subject_carries_every_location() {
        let subject = subject(&request(json!({
            "kind": "edit",
            "rawInput": { "command": "not a command" },
            "locations": [{ "path": "/w/a.rs", "line": 3 }, { "path": "/w/b.rs" }, {}],
        })));
        assert_eq!(subject.action, ActionClass::Edit);
        assert_eq!(subject.command, None);
        assert_eq!(
            subject.paths,
            [PathBuf::from("/w/a.rs"), PathBuf::from("/w/b.rs")]
        );
    }

    #[test]
    fn other_kinds_carry_neither_command_nor_paths_except_reads() {
        let locations = json!([{ "path": "/w/a.rs" }]);
        for (kind, class, paths) in [
            ("fetch", ActionClass::Network, 0),
            ("read", ActionClass::Read, 1),
            ("search", ActionClass::Read, 1),
            ("other", ActionClass::Other, 0),
        ] {
            let subject = subject(&request(json!({
                "kind": kind, "rawInput": { "command": "x" }, "locations": locations,
            })));
            assert_eq!(subject.action, class, "{kind}");
            assert_eq!(subject.command, None, "{kind}");
            assert_eq!(subject.paths.len(), paths, "{kind}");
        }
    }

    #[test]
    fn a_tool_is_named_by_its_kind_or_else_its_title() {
        let name = |call| tool_name(&call);
        assert_eq!(
            name(json!({ "kind": "fetch", "title": "Fetch x" })),
            "fetch"
        );
        assert_eq!(
            name(json!({ "kind": "other", "title": " Agent: count " })),
            "Agent: count"
        );
        assert_eq!(name(json!({ "title": "spawn_subagent" })), "spawn_subagent");
        assert_eq!(name(json!({ "kind": "other" })), "other");
        assert_eq!(name(json!({})), "tool");
    }

    #[test]
    fn the_access_table_is_what_the_module_documents() {
        use AccessMode::*;
        for kind in ["read", "search", "think"] {
            for access in [Supervised, AutoAcceptEdits, FullAccess] {
                assert!(allowed_without_asking(access, Some(kind)), "{kind}");
            }
        }
        for kind in ["edit", "delete", "move"] {
            assert!(!allowed_without_asking(Supervised, Some(kind)), "{kind}");
            assert!(
                allowed_without_asking(AutoAcceptEdits, Some(kind)),
                "{kind}"
            );
            assert!(allowed_without_asking(FullAccess, Some(kind)), "{kind}");
        }
        for kind in [
            Some("execute"),
            Some("fetch"),
            Some("other"),
            Some("switch_mode"),
            None,
        ] {
            assert!(!allowed_without_asking(Supervised, kind), "{kind:?}");
            assert!(!allowed_without_asking(AutoAcceptEdits, kind), "{kind:?}");
            assert!(allowed_without_asking(FullAccess, kind), "{kind:?}");
        }
    }

    #[test]
    fn answers_pick_the_once_options_and_never_allow_always() {
        let options = vec![
            json!({ "optionId": "always", "kind": "allow_always" }),
            json!({ "optionId": "once", "kind": "allow_once" }),
            json!({ "optionId": "never", "kind": "reject_always" }),
            json!({ "optionId": "no", "kind": "reject_once" }),
        ];
        let picked =
            |options: &[Value], allow| outcome(options, allow)["outcome"]["optionId"].clone();
        assert_eq!(picked(&options, true), "once");
        assert_eq!(picked(&options, false), "no");
        // Only a lasting allow on offer: a yes must not grant it.
        assert_eq!(outcome(&options[..1], true), cancelled());
        assert_eq!(picked(&options[..3], false), "never");
        // Nothing to refuse with: cancelled, not an allow.
        assert_eq!(outcome(&options[..2], false), cancelled());
        assert_eq!(outcome(&[], true), cancelled());
    }

    #[test]
    fn options_without_allow_or_reject_kinds_are_a_question() {
        let permission = options(&json!({ "options": [
            { "optionId": "a", "kind": "allow_always" },
            { "optionId": "a2", "kind": "allow_always" },
            { "optionId": "n", "kind": "reject_once" },
        ] }));
        assert!(!is_question(&permission));
        let choices = options(&json!({ "options": [
            { "optionId": "x", "name": "Use tokio" },
            { "optionId": "y", "name": "Use smol", "kind": "allow_once" },
        ] }));
        assert!(is_question(&choices));
        assert!(!is_question(&[]));
    }

    #[test]
    fn a_question_is_answered_by_the_option_named_by_the_label() {
        let params = json!({
            "toolCall": { "title": "Which runtime?" },
            "options": [
                { "optionId": "opt-tokio", "name": "Use tokio" },
                { "optionId": "opt-smol", "name": "Use smol" },
            ],
        });
        let options = options(&params);
        let asked = question("7", &params, &options);
        assert_eq!(asked.prompt, "Which runtime?");
        assert_eq!(asked.options, ["Use tokio", "Use smol"]);
        let answer = |id: &str, label: &str| {
            vec![InputAnswer {
                question_id: id.into(),
                labels: vec![label.into()],
            }]
        };
        assert_eq!(
            answer_outcome("7", &options, &answer("7", "Use smol")),
            selected("opt-smol")
        );
        // A label that names no option, an answer to another question, and no answer at all
        // never select anything.
        assert_eq!(
            answer_outcome("7", &options, &answer("7", "Use both")),
            cancelled()
        );
        assert_eq!(
            answer_outcome("7", &options, &answer("8", "Use smol")),
            cancelled()
        );
        assert_eq!(answer_outcome("7", &options, &[]), cancelled());
    }
}
