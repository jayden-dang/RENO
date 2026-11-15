//! Permission requests, as data: what a `can_use_tool` control request is about, which access
//! mode lets it through unasked, and the answers the CLI reads back.
//!
//! Sans-IO (ARCH-6). Nothing here reads prose: an approval is a [`ApprovalSubject`] built from
//! the tool's name and its JSON input, and a decision is an allow or deny control response.
//!
//! # Tool to action class
//!
//! | Claude tool | [`ActionClass`] | Subject carries |
//! |---|---|---|
//! | `Bash` | `Command` | `command` |
//! | `Edit`, `Write`, `MultiEdit`, `NotebookEdit` | `Edit` | `paths` (`file_path`, `notebook_path`) |
//! | `WebFetch`, `WebSearch` | `Network` | |
//! | `mcp__<server>__<tool>` | `McpTool` | |
//! | `Read`, `Glob`, `Grep` | `Read` | `paths` (`file_path`, `path`) |
//! | anything else (`ExitPlanMode`, `TodoWrite`, `Task`, ...) | `Other` | |
//!
//! Every subject also carries the tool's own name and the request as the CLI sent it (`raw`).
//!
//! # What an access mode lets through
//!
//! See [`answered_here`] and [`permission_mode`]; the table lives on
//! `ClaudeExecutor`, which applies them.

use std::path::PathBuf;

use reno_proto::{AccessMode, ActionClass, ApprovalSubject, Question};
use serde_json::Value;

use crate::exec::InputAnswer;

/// Claude's file-changing tools.
const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// The tool whose permission request is really a question to the person.
pub(crate) const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// The tool that asks to leave plan mode.
pub(crate) const EXIT_PLAN_MODE: &str = "ExitPlanMode";

/// What a refused approval tells the agent.
pub(crate) const DENIED: &str = "The user did not allow this.";

/// What a declined `AskUserQuestion` tells the agent.
pub(crate) const QUESTIONS_DISMISSED: &str = "The user dismissed the questions without answering.";

/// The action class of a Claude tool.
pub(crate) fn action_class(tool: &str) -> ActionClass {
    match tool {
        "Bash" => ActionClass::Command,
        t if EDIT_TOOLS.contains(&t) => ActionClass::Edit,
        "WebFetch" | "WebSearch" => ActionClass::Network,
        t if t.starts_with("mcp__") => ActionClass::McpTool,
        "Read" | "Glob" | "Grep" => ActionClass::Read,
        _ => ActionClass::Other,
    }
}

fn string_field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

/// The subject of a permission request: `tool` and `input` from the request, `raw` the request
/// as the CLI sent it, kept for the audit record.
pub(crate) fn subject(tool: &str, input: &Value, raw: &Value) -> ApprovalSubject {
    let action = action_class(tool);
    let command = (action == ActionClass::Command)
        .then(|| string_field(input, "command").map(str::to_owned))
        .flatten();
    let paths = match action {
        ActionClass::Edit | ActionClass::Read => ["file_path", "notebook_path", "path"]
            .iter()
            .filter_map(|key| string_field(input, key))
            .map(PathBuf::from)
            .collect(),
        _ => Vec::new(),
    };
    ApprovalSubject {
        action,
        tool: tool.to_owned(),
        command,
        paths,
        raw: raw.clone(),
    }
}

/// Whether the driver itself answers every permission request that reaches it: full access
/// asks nothing. Under the other two modes whatever the CLI's permission mode does not cover
/// reaches Reno, edits included: under `acceptEdits` an `Edit` request is one the CLI did not
/// accept itself.
pub(crate) fn answered_here(access: AccessMode) -> bool {
    access == AccessMode::FullAccess
}

/// The CLI `--permission-mode` a run launches in. Full access is `default` too: the run is
/// answered here instead (see [`answered_here`]), so no run is launched with a
/// skip-permissions flag, which the CLI refuses as root outside a sandbox.
pub(crate) fn permission_mode(access: AccessMode) -> &'static str {
    match access {
        AccessMode::AutoAcceptEdits => "acceptEdits",
        AccessMode::Supervised | AccessMode::FullAccess => "default",
    }
}

/// One question of an `AskUserQuestion` call, with what is needed to answer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asked {
    /// The question as the contract carries it.
    pub question: Question,
    /// The CLI's own text for it: the key its answer goes under.
    pub key: String,
}

/// Parse `AskUserQuestion`'s input, tolerantly: `header` or `title`, `question` or `prompt`,
/// options as strings or objects (their descriptions are dropped; the contract carries labels).
/// Ids are `<request_id>:<index>`, so they are stable and unique within the run.
pub(crate) fn parse_questions(request_id: &str, input: &Value) -> Vec<Asked> {
    let Some(raw) = input.get("questions").and_then(Value::as_array) else {
        return Vec::new();
    };
    raw.iter()
        .enumerate()
        .map(|(at, q)| {
            let field =
                |keys: [&str; 2]| keys.iter().find_map(|k| q.get(*k).and_then(Value::as_str));
            let header = field(["header", "title"]).unwrap_or_default();
            let text = field(["question", "prompt"]).unwrap_or_default();
            Asked {
                question: Question {
                    id: format!("{request_id}:{at}"),
                    prompt: if header.is_empty() {
                        text.to_owned()
                    } else {
                        format!("{header}: {text}")
                    },
                    options: q
                        .get("options")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        .iter()
                        .map(|option| match option {
                            Value::String(s) => s.clone(),
                            other => other
                                .get("label")
                                .or_else(|| other.get("value"))
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        })
                        .collect(),
                    multi_select: ["multiSelect", "multi_select"]
                        .iter()
                        .find_map(|k| q.get(*k).and_then(Value::as_bool))
                        .unwrap_or(false),
                },
                key: text.to_owned(),
            }
        })
        .collect()
}

/// Merge the answers back into the tool input, keyed by question text (single select: a string,
/// multi select: an array), as the tool expects. A question nobody answered is an empty pick.
pub(crate) fn updated_input_with_answers(
    input: &Value,
    asked: &[Asked],
    answers: &[InputAnswer],
) -> Value {
    let mut updated = match input {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    let mut by_question = serde_json::Map::new();
    for a in asked {
        let labels: Vec<String> = answers
            .iter()
            .find(|given| given.question_id == a.question.id)
            .map(|given| given.labels.clone())
            .unwrap_or_default();
        let value = if a.question.multi_select {
            Value::Array(labels.into_iter().map(Value::String).collect())
        } else {
            Value::String(labels.into_iter().next().unwrap_or_default())
        };
        by_question.insert(a.key.clone(), value);
    }
    updated.insert("answers".into(), Value::Object(by_question));
    Value::Object(updated)
}

/// The control response that allows a tool call. `leave_plan_for` is set when the call is
/// `ExitPlanMode`: approving the plan then continues in that permission mode.
pub(crate) fn allow(updated_input: Value, leave_plan_for: Option<&str>) -> Value {
    match leave_plan_for {
        Some(mode) => super::wire::exit_plan_response(updated_input, mode),
        None => super::wire::allow_response(updated_input),
    }
}

/// The control response that refuses a tool call; the model reads `message`.
pub(crate) fn deny(message: &str) -> Value {
    super::wire::deny_response(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::InputAnswer;
    use reno_proto::{AccessMode, ActionClass};
    use serde_json::json;

    #[test]
    fn every_tool_has_one_action_class() {
        for (tool, class) in [
            ("Bash", ActionClass::Command),
            ("Edit", ActionClass::Edit),
            ("Write", ActionClass::Edit),
            ("MultiEdit", ActionClass::Edit),
            ("NotebookEdit", ActionClass::Edit),
            ("WebFetch", ActionClass::Network),
            ("WebSearch", ActionClass::Network),
            ("mcp__x__y", ActionClass::McpTool),
            ("mcp__linear__search", ActionClass::McpTool),
            ("Read", ActionClass::Read),
            ("Glob", ActionClass::Read),
            ("Grep", ActionClass::Read),
            ("ExitPlanMode", ActionClass::Other),
            ("TodoWrite", ActionClass::Other),
            ("Task", ActionClass::Other),
            ("Agent", ActionClass::Other),
            ("", ActionClass::Other),
            // Case matters: Claude's tool names are exact.
            ("bash", ActionClass::Other),
            ("mcp_x", ActionClass::Other),
        ] {
            assert_eq!(action_class(tool), class, "{tool:?}");
        }
    }

    #[test]
    fn a_subject_carries_command_paths_and_the_raw_request() {
        let raw = json!({"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "rm -rf build"}});
        let bash = subject("Bash", &json!({"command": "rm -rf build"}), &raw);
        assert_eq!(bash.action, ActionClass::Command);
        assert_eq!(bash.tool, "Bash");
        assert_eq!(bash.command.as_deref(), Some("rm -rf build"));
        assert!(bash.paths.is_empty());
        assert_eq!(bash.raw, raw);

        let edit = subject(
            "Edit",
            &json!({"file_path": "/w/a.rs", "old_string": "a"}),
            &json!({}),
        );
        assert_eq!(edit.action, ActionClass::Edit);
        assert_eq!(edit.command, None);
        assert_eq!(edit.paths, vec![std::path::PathBuf::from("/w/a.rs")]);

        let notebook = subject(
            "NotebookEdit",
            &json!({"notebook_path": "/w/n.ipynb"}),
            &json!({}),
        );
        assert_eq!(notebook.paths, vec![std::path::PathBuf::from("/w/n.ipynb")]);

        let grep = subject(
            "Grep",
            &json!({"pattern": "x", "path": "/w/src"}),
            &json!({}),
        );
        assert_eq!(grep.action, ActionClass::Read);
        assert_eq!(grep.paths, vec![std::path::PathBuf::from("/w/src")]);

        // Only a command action has a command, whatever the input says.
        let mcp = subject(
            "mcp__x__y",
            &json!({"command": "ls", "file_path": "/etc"}),
            &json!({}),
        );
        assert_eq!(mcp.action, ActionClass::McpTool);
        assert_eq!(mcp.command, None);
        assert!(mcp.paths.is_empty());

        // An input that is not an object, or fields that are not strings, read as absent.
        let odd = subject("Bash", &json!("not an object"), &json!(null));
        assert_eq!(odd.command, None);
        assert_eq!(odd.raw, json!(null));
        let odd = subject("Edit", &json!({"file_path": 7}), &json!({}));
        assert!(odd.paths.is_empty());
    }

    #[test]
    fn only_full_access_is_answered_without_asking_reno() {
        assert!(answered_here(AccessMode::FullAccess));
        // Under acceptEdits the CLI accepts the edits it covers itself, so an edit request that
        // arrives is one it did not cover: it is asked like the rest.
        assert!(!answered_here(AccessMode::AutoAcceptEdits));
        assert!(!answered_here(AccessMode::Supervised));
    }

    #[test]
    fn access_modes_map_onto_the_clis_permission_modes() {
        assert_eq!(permission_mode(AccessMode::Supervised), "default");
        assert_eq!(permission_mode(AccessMode::AutoAcceptEdits), "acceptEdits");
        // Never `bypassPermissions`: full access is answered here, so no run launches with a
        // skip-permissions flag the CLI refuses as root.
        assert_eq!(permission_mode(AccessMode::FullAccess), "default");
    }

    #[test]
    fn questions_parse_tolerantly_into_contract_questions() {
        let input = json!({"questions": [
            {"header": "Choice", "question": "Pick one", "options": ["A", "B"], "multiSelect": false},
            {"title": "Many", "prompt": "Pick some", "options": [{"label": "X", "description": "dropped"}, {"value": "Y"}, {}], "multi_select": true},
            {"question": "Free text?"},
        ]});
        let asked = parse_questions("cr-1", &input);
        let questions: Vec<_> = asked.iter().map(|a| a.question.clone()).collect();
        assert_eq!(
            questions,
            vec![
                Question {
                    id: "cr-1:0".into(),
                    prompt: "Choice: Pick one".into(),
                    options: vec!["A".into(), "B".into()],
                    multi_select: false,
                },
                Question {
                    id: "cr-1:1".into(),
                    prompt: "Many: Pick some".into(),
                    options: vec!["X".into(), "Y".into(), String::new()],
                    multi_select: true,
                },
                Question {
                    id: "cr-1:2".into(),
                    prompt: "Free text?".into(),
                    options: Vec::new(),
                    multi_select: false,
                },
            ]
        );
        assert!(parse_questions("cr-2", &json!({})).is_empty());
        assert!(parse_questions("cr-2", &json!({"questions": "nope"})).is_empty());
    }

    #[test]
    fn answers_key_by_question_text() {
        let input = json!({"questions": [
            {"header": "Choice", "question": "Pick one", "options": ["A", "B"]},
            {"header": "Many", "question": "Pick some", "options": ["X", "Y"], "multiSelect": true},
        ], "keep": 1});
        let asked = parse_questions("cr-1", &input);
        let answers = vec![
            InputAnswer {
                question_id: "cr-1:0".into(),
                labels: vec!["B".into()],
            },
            InputAnswer {
                question_id: "cr-1:1".into(),
                labels: vec!["X".into(), "Y".into()],
            },
        ];
        let updated = updated_input_with_answers(&input, &asked, &answers);
        assert_eq!(updated["keep"], 1);
        assert_eq!(updated["answers"]["Pick one"], "B");
        assert_eq!(updated["answers"]["Pick some"], json!(["X", "Y"]));
        // A question nobody answered reads as an empty pick, not a missing key.
        let updated = updated_input_with_answers(&input, &asked, &answers[..1]);
        assert_eq!(updated["answers"]["Pick some"], json!([]));
    }

    #[test]
    fn responses_are_the_shapes_the_cli_reads() {
        assert_eq!(
            allow(json!({"command": "ls"}), None),
            json!({"behavior": "allow", "updatedInput": {"command": "ls"}})
        );
        // Approving a plan leaves plan mode for the run's own mode.
        let plan = allow(json!({"plan": "p"}), Some("acceptEdits"));
        assert_eq!(plan["updatedPermissions"][0]["mode"], "acceptEdits");
        assert_eq!(deny("nope"), json!({"behavior": "deny", "message": "nope"}));
        assert_eq!(DENIED, "The user did not allow this.");
        assert!(QUESTIONS_DISMISSED.contains("dismissed"));
    }
}
