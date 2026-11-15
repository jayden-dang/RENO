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

        let edit = subject("Edit", &json!({"file_path": "/w/a.rs", "old_string": "a"}), &json!({}));
        assert_eq!(edit.action, ActionClass::Edit);
        assert_eq!(edit.command, None);
        assert_eq!(edit.paths, vec![std::path::PathBuf::from("/w/a.rs")]);

        let notebook = subject("NotebookEdit", &json!({"notebook_path": "/w/n.ipynb"}), &json!({}));
        assert_eq!(notebook.paths, vec![std::path::PathBuf::from("/w/n.ipynb")]);

        let grep = subject("Grep", &json!({"pattern": "x", "path": "/w/src"}), &json!({}));
        assert_eq!(grep.action, ActionClass::Read);
        assert_eq!(grep.paths, vec![std::path::PathBuf::from("/w/src")]);

        // Only a command action has a command, whatever the input says.
        let mcp = subject("mcp__x__y", &json!({"command": "ls", "file_path": "/etc"}), &json!({}));
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
    fn each_access_mode_covers_what_it_says() {
        use AccessMode::*;
        for tool in ["Bash", "Edit", "mcp__x__y", "WebFetch", "Read", "Whatever", "ExitPlanMode"] {
            assert!(allowed_without_asking(FullAccess, tool), "{tool}");
            assert!(!allowed_without_asking(Supervised, tool), "{tool}");
        }
        for tool in ["Edit", "Write", "MultiEdit", "NotebookEdit"] {
            assert!(allowed_without_asking(AutoAcceptEdits, tool), "{tool}");
        }
        for tool in ["Bash", "mcp__x__y", "WebFetch", "Read", "Whatever", "ExitPlanMode"] {
            assert!(!allowed_without_asking(AutoAcceptEdits, tool), "{tool}");
        }
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
        assert_eq!(
            deny("nope"),
            json!({"behavior": "deny", "message": "nope"})
        );
        assert_eq!(DENIED, "The user did not allow this.");
        assert!(QUESTIONS_DISMISSED.contains("dismissed"));
    }
}
