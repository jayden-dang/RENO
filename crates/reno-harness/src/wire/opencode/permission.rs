//! What opencode asks of a person: a permission request becomes an approval, the `question` tool
//! (a `question.asked` on 1.x, a question form on 2.x) becomes questions, and the answers become
//! the replies the server takes.
//!
//! A permission is answered `once` or `reject` (see [`super::requests::Reply`]): never `always`,
//! which would save a rule in the owner's permission store.

use std::path::PathBuf;

use reno_proto::{ActionClass, ApprovalSubject, Question};
use serde_json::{Value, json};

use super::requests::{self, Request};

/// A `permission.asked`: who asked, and what for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Permission {
    /// The request id the reply addresses.
    pub(crate) id: String,
    /// The session (the run's, or a subagent's) that is blocked on it.
    pub(crate) session_id: String,
    /// What the approval is for.
    pub(crate) subject: ApprovalSubject,
}

/// opencode's permission for changing files: 1.x folds its write and patch tools into `edit`
/// (1.19.4); older names are kept.
pub(crate) fn is_edit(permission: &str) -> bool {
    matches!(
        permission,
        "edit" | "write" | "patch" | "multiedit" | "apply_patch"
    )
}

/// How opencode spells the name of an MCP server in a tool name: anything outside letters,
/// digits, `_` and `-` becomes `_`.
fn sanitized(server: &str) -> String {
    server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A tool of one of the run's MCP servers as Reno names it everywhere else, `mcp__<server>__<tool>`
/// (the kernel recognises Reno's own tools and a policy rule matches another server's by that
/// name). opencode names such a tool `<server>_<tool>`; the longest server name that fits wins.
pub(crate) fn mcp_tool_name(tool: &str, mcp_servers: &[String]) -> Option<String> {
    mcp_servers
        .iter()
        .filter_map(|server| {
            let prefix = format!("{}_", sanitized(server));
            let rest = tool
                .strip_prefix(prefix.as_str())
                .filter(|rest| !rest.is_empty())?;
            Some((prefix.len(), format!("mcp__{server}__{rest}")))
        })
        .max_by_key(|(length, _)| *length)
        .map(|(_, name)| name)
}

/// The name Reno gives a tool or permission of opencode's, and what kind of action it is.
pub(crate) fn canonical(tool: &str, mcp_servers: &[String]) -> (String, ActionClass) {
    if let Some(name) = mcp_tool_name(tool, mcp_servers) {
        return (name, ActionClass::McpTool);
    }
    let class = match tool {
        "bash" => ActionClass::Command,
        t if is_edit(t) => ActionClass::Edit,
        "read" | "glob" | "grep" | "list" | "lsp" => ActionClass::Read,
        "webfetch" | "websearch" => ActionClass::Network,
        _ => ActionClass::Other,
    };
    (tool.to_owned(), class)
}

/// Read a `permission.asked` payload (1.x: `permission` and `patterns`; 2.x: `action` and
/// `resources`). `mcp_servers` are the names of the run's MCP servers.
pub(crate) fn parse_permission(props: &Value, mcp_servers: &[String]) -> Option<Permission> {
    let id = props.get("id").and_then(Value::as_str)?.to_owned();
    let session_id = props.get("sessionID").and_then(Value::as_str)?.to_owned();
    let permission = props
        .get("permission")
        .or_else(|| props.get("action"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let patterns: Vec<&str> = props
        .get("patterns")
        .or_else(|| props.get("resources"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let metadata = |key: &str| {
        props
            .pointer(&format!("/metadata/{key}"))
            .and_then(Value::as_str)
    };
    let (tool, class) = canonical(permission, mcp_servers);
    let command = (class == ActionClass::Command)
        .then(|| metadata("command").map_or_else(|| patterns.join(" && "), str::to_owned));
    let paths: Vec<PathBuf> = match permission {
        p if is_edit(p) => match metadata("filepath") {
            Some(path) => vec![PathBuf::from(path)],
            None => patterns.iter().map(PathBuf::from).collect(),
        },
        "external_directory" => metadata("parentDir")
            .map(|dir| vec![PathBuf::from(dir)])
            .unwrap_or_else(|| patterns.iter().map(PathBuf::from).collect()),
        "read" | "glob" | "grep" | "list" => patterns.iter().map(PathBuf::from).collect(),
        _ => Vec::new(),
    };
    Some(Permission {
        id,
        session_id,
        subject: ApprovalSubject {
            action: class,
            tool: if tool.is_empty() { "tool".into() } else { tool },
            command,
            paths,
            raw: props.clone(),
        },
    })
}

/// A question the `question` tool put to the person, with the field keys its reply addresses.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Asked {
    /// The request (1.x) or form (2.x) id.
    pub(crate) id: String,
    /// The session that is blocked on it.
    pub(crate) session_id: String,
    /// A 2.x form: answered through the form route, keyed by field.
    pub(crate) form: bool,
    /// The questions: the key, then the question as Reno shows it.
    pub(crate) questions: Vec<(String, Question)>,
}

/// Read a `question.asked` payload (a 2.x form is folded into the same shape by
/// [`super::v2::question_form_payload`]). The question id is the field key (`q0`..): unique within
/// the request, which is all an answer needs.
pub(crate) fn parse_question(props: &Value) -> Option<Asked> {
    let id = props.get("id").and_then(Value::as_str)?.to_owned();
    let session_id = props.get("sessionID").and_then(Value::as_str)?.to_owned();
    let questions: Vec<(String, Question)> = props
        .get("questions")
        .and_then(Value::as_array)?
        .iter()
        .enumerate()
        .filter_map(|(ix, q)| {
            let prompt = q.get("question").and_then(Value::as_str)?;
            let key = q
                .get("key")
                .and_then(Value::as_str)
                .filter(|key| !key.is_empty())
                .map_or_else(|| format!("q{ix}"), str::to_owned);
            Some((
                key.clone(),
                Question {
                    id: key,
                    prompt: prompt.to_owned(),
                    options: q
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|options| {
                            options
                                .iter()
                                .filter_map(|o| o.get("label").and_then(Value::as_str))
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                    multi_select: q.get("multiple").and_then(Value::as_bool).unwrap_or(false),
                },
            ))
        })
        .collect();
    (!questions.is_empty()).then_some(Asked {
        id,
        session_id,
        form: props.get("form").and_then(Value::as_bool).unwrap_or(false),
        questions,
    })
}

impl Asked {
    /// The requests that deliver `answers` (`None`, or an empty list, declines). A 2.x form takes
    /// one string for a single-select field (empty when unanswered) and the labels for a
    /// multi-select one, keyed by field; 1.x takes one list of labels per question, in order.
    /// Declining a 2.x form sends nothing: a pending form is instance state and dies with the run.
    pub(crate) fn reply(&self, answers: Option<&[(String, Vec<String>)]>) -> Option<Request> {
        let given = |key: &str| -> Vec<String> {
            answers
                .into_iter()
                .flatten()
                .find(|(id, _)| id == key)
                .map(|(_, labels)| labels.clone())
                .unwrap_or_default()
        };
        let answered = answers.is_some_and(|a| !a.is_empty());
        if self.form {
            return answered.then(|| {
                let mut body = serde_json::Map::new();
                for (key, question) in &self.questions {
                    let labels = given(key);
                    body.insert(
                        key.clone(),
                        if question.multi_select {
                            json!(labels)
                        } else {
                            json!(labels.into_iter().next().unwrap_or_default())
                        },
                    );
                }
                requests::form_reply(&self.session_id, &self.id, json!({"answer": body}))
            });
        }
        Some(if answered {
            let ordered: Vec<Vec<String>> =
                self.questions.iter().map(|(key, _)| given(key)).collect();
            requests::question_reply(&self.id, &ordered)
        } else {
            requests::question_reject(&self.id)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ask(props: Value) -> Permission {
        parse_permission(&props, &["reno".to_owned()]).unwrap()
    }

    #[test]
    fn a_command_is_asked_with_its_command_line() {
        let permission = ask(json!({
            "id": "per_1", "sessionID": "ses_a", "permission": "bash", "patterns": ["rm *"],
            "metadata": {"command": "rm -rf build"}
        }));
        assert_eq!(permission.id, "per_1");
        assert_eq!(permission.subject.action, ActionClass::Command);
        assert_eq!(permission.subject.command.as_deref(), Some("rm -rf build"));
        assert_eq!(permission.subject.raw["patterns"][0], "rm *");
    }

    #[test]
    fn an_edit_names_its_file_and_a_v2_request_is_read_alike() {
        let v1 = ask(
            json!({"id": "p", "sessionID": "s", "permission": "edit", "patterns": ["src/*"], "metadata": {"filepath": "/w/src/a.rs"}}),
        );
        assert_eq!(v1.subject.action, ActionClass::Edit);
        assert_eq!(v1.subject.paths, [PathBuf::from("/w/src/a.rs")]);
        let v2 =
            ask(json!({"id": "p", "sessionID": "s", "action": "write", "resources": ["/w/b.rs"]}));
        assert_eq!(v2.subject.action, ActionClass::Edit);
        assert_eq!(v2.subject.paths, [PathBuf::from("/w/b.rs")]);
        assert_eq!(v2.subject.tool, "write");
    }

    #[test]
    fn network_outside_the_project_and_mcp_tools_have_their_own_classes() {
        let web = ask(
            json!({"id": "p", "sessionID": "s", "permission": "webfetch", "patterns": ["https://x"]}),
        );
        assert_eq!(web.subject.action, ActionClass::Network);
        let outside = ask(
            json!({"id": "p", "sessionID": "s", "permission": "external_directory", "patterns": ["/etc/*"], "metadata": {"parentDir": "/etc"}}),
        );
        assert_eq!(outside.subject.action, ActionClass::Other);
        assert_eq!(outside.subject.paths, [PathBuf::from("/etc")]);
        let mcp = ask(json!({"id": "p", "sessionID": "s", "permission": "reno_report"}));
        assert_eq!(mcp.subject.action, ActionClass::McpTool);
        assert_eq!(mcp.subject.tool, "mcp__reno__report");
        let stranger = ask(json!({"id": "p", "sessionID": "s", "permission": "renox_do"}));
        assert_eq!(stranger.subject.action, ActionClass::Other);
        let unnamed = ask(json!({"id": "p", "sessionID": "s"}));
        assert_eq!(unnamed.subject.tool, "tool");
    }

    #[test]
    fn a_request_without_an_owner_or_an_id_is_not_ours_to_answer() {
        assert!(parse_permission(&json!({"id": "p", "permission": "bash"}), &[]).is_none());
        assert!(parse_permission(&json!({"sessionID": "s", "permission": "bash"}), &[]).is_none());
    }

    fn asked(form: bool) -> Asked {
        parse_question(&json!({
            "id": "que_1", "sessionID": "ses_a", "form": form,
            "questions": [
                {"question": "Which?", "header": "Pick", "options": [{"label": "A"}, {"label": "B"}], "key": "q0"},
                {"question": "Many?", "multiple": true, "options": [{"label": "X"}, {"label": "Y"}]},
            ]
        }))
        .unwrap()
    }

    #[test]
    fn questions_keep_their_field_keys_and_options() {
        let asked = asked(false);
        assert_eq!(asked.questions[0].1.id, "q0");
        assert_eq!(asked.questions[0].1.options, ["A", "B"]);
        assert_eq!(
            asked.questions[1].0, "q1",
            "a question with no key is positional"
        );
        assert!(asked.questions[1].1.multi_select);
        assert!(parse_question(&json!({"id": "q", "sessionID": "s", "questions": []})).is_none());
    }

    #[test]
    fn a_v1_answer_is_one_list_per_question_in_order_and_declining_rejects() {
        let asked = asked(false);
        let answers = vec![
            ("q1".to_owned(), vec!["X".to_owned(), "Y".to_owned()]),
            ("q0".to_owned(), vec!["B".to_owned()]),
        ];
        let reply = asked.reply(Some(&answers)).unwrap();
        assert_eq!(reply.path, "/question/que_1/reply");
        assert_eq!(reply.body.unwrap(), json!({"answers": [["B"], ["X", "Y"]]}));
        assert_eq!(
            asked.reply(Some(&[])).unwrap().path,
            "/question/que_1/reject"
        );
        assert_eq!(asked.reply(None).unwrap().path, "/question/que_1/reject");
    }

    #[test]
    fn a_v2_form_answer_is_keyed_by_field_and_declining_sends_nothing() {
        let asked = asked(true);
        let answers = vec![
            ("q0".to_owned(), vec!["A".to_owned()]),
            ("q1".to_owned(), vec!["Y".to_owned()]),
        ];
        let reply = asked.reply(Some(&answers)).unwrap();
        assert_eq!(reply.path, "/api/session/ses_a/form/que_1/reply");
        assert_eq!(
            reply.body.unwrap(),
            json!({"answer": {"q0": "A", "q1": ["Y"]}})
        );
        assert!(asked.reply(None).is_none());
        // An unanswered single-select field is the empty string.
        let partial = vec![("q1".to_owned(), vec!["Y".to_owned()])];
        assert_eq!(
            asked.reply(Some(&partial)).unwrap().body.unwrap()["answer"]["q0"],
            ""
        );
    }
}
