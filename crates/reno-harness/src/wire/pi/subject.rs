//! A Pi gate call as the ladder's [`ApprovalSubject`].
//!
//! Sans-IO (ARCH-6): the gate's `call` line in, the subject out. The class is
//! [`normalize::action_class`], so a call is classed as the `Tool` event for it is.

use std::path::PathBuf;

use reno_proto::{ActionClass, ApprovalSubject};
use serde_json::{Value, json};

use super::normalize::action_class;

/// The subject of the gate call `call_id` to `tool`, made by `parent` when another tool made it.
///
/// A command is `input.command`. Edits and reads name `input.path`; `grep`, `find` and `ls` may
/// omit it (meaning the working directory), which leaves no path. Paths are kept as written. A
/// `find` pattern and a `grep` glob are not paths and stay only in `raw`, which is the call as
/// the gate sent it.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the driver reads gate calls from Task 6 on")
)]
pub(crate) fn subject(
    tool: &str,
    call_id: &str,
    parent: Option<&str>,
    input: &Value,
) -> ApprovalSubject {
    let action = action_class(tool);
    let text = |key: &str| input.get(key).and_then(Value::as_str);
    let command = (action == ActionClass::Command)
        .then(|| text("command").map(str::to_owned))
        .flatten();
    let paths = match action {
        ActionClass::Edit | ActionClass::Read => {
            text("path").map(PathBuf::from).into_iter().collect()
        }
        _ => Vec::new(),
    };
    ApprovalSubject {
        action,
        tool: tool.to_owned(),
        command,
        paths,
        raw: json!({
            "toolName": tool,
            "toolCallId": call_id,
            "parentToolCallId": parent,
            "input": input,
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn paths(subject: &ApprovalSubject) -> Vec<&str> {
        subject.paths.iter().map(|p| p.to_str().unwrap()).collect()
    }

    #[test]
    fn a_shell_tool_is_a_command_with_its_command_line() {
        for tool in ["bash", "powershell"] {
            let subject = subject(tool, "c1", None, &json!({ "command": "git status" }));
            assert_eq!(subject.action, ActionClass::Command, "{tool}");
            assert_eq!(subject.tool, tool);
            assert_eq!(subject.command.as_deref(), Some("git status"));
            assert!(subject.paths.is_empty());
        }
    }

    #[test]
    fn a_reading_tool_is_a_read_of_its_path_and_of_nothing_when_it_names_none() {
        for tool in ["read", "ls", "grep", "find"] {
            let named = subject(tool, "c1", None, &json!({ "path": "src/lib.rs" }));
            assert_eq!(named.action, ActionClass::Read, "{tool}");
            assert_eq!(paths(&named), ["src/lib.rs"], "{tool}");
            assert_eq!(named.command, None);

            let unnamed = subject(tool, "c2", None, &json!({ "pattern": "todo" }));
            assert_eq!(unnamed.action, ActionClass::Read, "{tool}");
            assert!(
                unnamed.paths.is_empty(),
                "{tool}: the working directory is no path"
            );
        }
    }

    #[test]
    fn a_search_pattern_is_not_a_path() {
        let subject = subject(
            "find",
            "c1",
            None,
            &json!({ "pattern": "*.rs", "glob": "src/**" }),
        );
        assert!(subject.paths.is_empty());
        assert_eq!(subject.raw["input"]["pattern"], "*.rs");
    }

    #[test]
    fn a_writing_tool_is_an_edit_of_its_path() {
        for tool in ["edit", "write"] {
            let subject = subject(
                tool,
                "c1",
                None,
                &json!({ "path": ".reno/gate.mjs", "content": "x" }),
            );
            assert_eq!(subject.action, ActionClass::Edit, "{tool}");
            assert_eq!(paths(&subject), [".reno/gate.mjs"], "{tool}");
            assert_eq!(subject.command, None);
        }
    }

    #[test]
    fn an_mcp_tool_is_an_mcp_tool_call_by_name() {
        let subject = subject("mcp__x__y", "c1", None, &json!({ "q": 1 }));
        assert_eq!(subject.action, ActionClass::McpTool);
        assert_eq!(subject.tool, "mcp__x__y");
        assert!(subject.paths.is_empty());
        assert_eq!(subject.command, None);
    }

    #[test]
    fn an_unknown_tool_is_other_and_keeps_its_name() {
        let subject = subject(
            "extension_tool",
            "c1",
            None,
            &json!({ "path": "a", "command": "b" }),
        );
        assert_eq!(subject.action, ActionClass::Other);
        assert_eq!(subject.tool, "extension_tool");
        assert_eq!(subject.command, None);
        assert!(subject.paths.is_empty());
    }

    #[test]
    fn raw_is_the_call_verbatim() {
        let input = json!({ "command": "ls", "extra": { "n": [1, null] } });
        let top = subject("bash", "c1", None, &input);
        assert_eq!(
            top.raw,
            json!({
                "toolName": "bash",
                "toolCallId": "c1",
                "parentToolCallId": null,
                "input": input,
            })
        );
        let nested = subject("read", "c2", Some("c1"), &json!({ "path": "a" }));
        assert_eq!(nested.raw["parentToolCallId"], "c1");
    }

    #[test]
    fn a_command_that_is_not_text_is_no_command() {
        let subject = subject("bash", "c1", None, &json!({ "command": 7 }));
        assert_eq!(subject.action, ActionClass::Command);
        assert_eq!(subject.command, None);
    }
}
