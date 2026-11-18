//! 2.x bus frames rewritten into the 1.x payloads (`{type, properties}`), so the reducer has one
//! vocabulary.
//!
//! The 2.x frame is `{id, type, data: {sessionID, assistantMessageID, ..}}`. Its vocabulary
//! (shapes captured live from 2.0.3 by zeron; 2.3.2 was checked for `server.connected` and the
//! heartbeat only, see the driver's module docs):
//!
//! - turn lifecycle: `session.execution.started` (busy), `.succeeded` / `.interrupted` /
//!   `.failed` (idle; failed carries the error);
//! - per step: `session.step.started` fixes the assistant message its items stream under, and
//!   `session.step.ended` carries the step's own tokens (the cumulative `session.usage.updated`
//!   totals are ignored);
//! - streaming items: `session.text.delta` (with started / ended snapshots) and
//!   `session.reasoning.*`; `ordinal` numbers the item within the step, which is the synthetic
//!   part id;
//! - tools: `session.tool.input.started` (carries the NAME), `.called` (arguments), `.success` /
//!   `.failed` / `.error`, `.progress`;
//! - `permission.asked` (same name as 1.x, with `action` and `resources`) and, from 2.0.24, the
//!   agent's `question` tool as a session FORM (`form.created`, `metadata.kind == "question"`).

use std::collections::HashMap;

use serde_json::{Value, json};

/// Pending tool calls kept before the stream is declared leaking: the run errors instead of
/// growing without bound (ARCH-11).
pub(crate) const MAX_PENDING_TOOLS: usize = 4096;
const MAX_SESSION_MODELS: usize = 4096;

type ToolKey = (String, String, String);

#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelIdentity {
    provider_id: String,
    model_id: String,
}

fn model_identity(model: &Value) -> Option<ModelIdentity> {
    let provider_id = model.get("providerID")?.as_str()?;
    let model_id = model.get("id")?.as_str()?;
    (!provider_id.is_empty() && !model_id.is_empty()).then(|| ModelIdentity {
        provider_id: provider_id.to_owned(),
        model_id: model_id.to_owned(),
    })
}

/// What the rewriting remembers between frames.
#[derive(Debug, Default)]
pub(crate) struct Normalizer {
    /// Open tool calls by (session, assistant message, call id): the name only the first frame
    /// carries.
    tool_names: HashMap<ToolKey, String>,
    /// The latest model of each session, for `session.step.ended`, which omits it.
    session_models: HashMap<String, ModelIdentity>,
}

impl Normalizer {
    /// Whether more tool calls are open than any real run has: the stream is not closing them.
    pub(crate) fn leaking(&self) -> bool {
        self.tool_names.len() > MAX_PENDING_TOOLS
    }

    /// Rewrite one frame into zero or more 1.x payloads.
    pub(crate) fn normalize(&mut self, event: &Value) -> Vec<Value> {
        if self.session_models.len() > MAX_SESSION_MODELS {
            self.session_models.clear();
        }
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let data = event.get("data").cloned().unwrap_or(Value::Null);
        // Form frames keep their owner inside `data.form`; everything else must name a session.
        if kind == "form.created" {
            return question_form_payload(data.get("form").unwrap_or(&data));
        }
        if data
            .get("sessionID")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Vec::new();
        }
        let session = || data.get("sessionID").cloned().unwrap_or(Value::Null);
        let message = || {
            data.get("assistantMessageID")
                .cloned()
                .unwrap_or(Value::Null)
        };
        let tool_key = || {
            let text = |key: &str| {
                data.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            (text("sessionID"), text("assistantMessageID"), text("id"))
        };
        let id = data.get("id").and_then(Value::as_str).unwrap_or_default();
        if matches!(
            kind,
            "session.execution.succeeded"
                | "session.execution.interrupted"
                | "session.execution.failed"
        ) {
            self.tool_names.retain(|(owner, _, _), _| {
                Some(owner.as_str()) != data.get("sessionID").and_then(Value::as_str)
            });
        }
        match kind {
            "session.status" => vec![json!({"type": "session.status", "properties": data})],
            "session.retry.scheduled" => vec![json!({
                "type": "session.status",
                "properties": {"sessionID": session(), "status": {
                    "type": "retry", "attempt": data.get("attempt"), "next": data.get("at"),
                    "message": data.pointer("/error/message").and_then(Value::as_str).filter(|s| !s.is_empty())
                        .or_else(|| data.pointer("/error/type").and_then(Value::as_str)).unwrap_or("provider retry"),
                }}
            })],
            "session.tool.progress" => {
                let name = self
                    .tool_names
                    .get(&tool_key())
                    .cloned()
                    .unwrap_or_default();
                vec![tool_part(
                    &data,
                    id,
                    &name,
                    &json!({"status": "running", "metadata": data.get("metadata")}),
                )]
            }
            "session.execution.started" => vec![json!({
                "type": "session.status",
                "properties": {"sessionID": session(), "status": {"type": "busy"}}
            })],
            "session.execution.interrupted" => vec![json!({
                "type": "session.interrupted",
                "properties": {"sessionID": session()}
            })],
            "session.execution.succeeded" => vec![json!({
                "type": "session.idle",
                "properties": {"sessionID": session()}
            })],
            "session.execution.failed" => vec![
                error_payload(&data),
                json!({"type": "session.idle", "properties": {"sessionID": session()}}),
            ],
            "session.step.failed" => {
                // An interrupt surfaces as step.failed{aborted}; the terminal execution.interrupted
                // frame settles the turn. Any other failure is a step-level provider error: a
                // warning, the turn itself keeps going (execution.* decides its end).
                if data.pointer("/error/type").and_then(Value::as_str) == Some("aborted") {
                    return Vec::new();
                }
                let mut warning = error_payload(&data);
                warning["type"] = json!("session.warning");
                vec![warning]
            }
            "session.step.started" => {
                if let (Some(session_id), Some(model)) = (
                    data.get("sessionID").and_then(Value::as_str),
                    data.get("model").and_then(model_identity),
                ) {
                    self.session_models.insert(session_id.to_owned(), model);
                }
                vec![json!({
                    "type": "message.updated",
                    "properties": {"info": {"sessionID": session(), "id": message(), "role": "assistant"}}
                })]
            }
            "session.text.started" | "session.text.ended" => vec![stream_part(
                &data,
                "text",
                data.get("text").cloned().unwrap_or(json!("")),
            )],
            "session.text.delta" | "session.reasoning.delta" => vec![json!({
                "type": "message.part.delta",
                "properties": {
                    "sessionID": session(),
                    "messageID": message(),
                    "partID": part_id(&data, if kind == "session.reasoning.delta" { 'r' } else { 't' }),
                    "field": "text",
                    "delta": data.get("delta").cloned().unwrap_or(json!("")),
                }
            })],
            "session.reasoning.started" | "session.reasoning.ended" => vec![stream_part(
                &data,
                "reasoning",
                data.get("text").cloned().unwrap_or(json!("")),
            )],
            "session.tool.input.started" => {
                let name = data.get("name").and_then(Value::as_str).unwrap_or_default();
                self.tool_names.insert(tool_key(), name.to_owned());
                vec![tool_part(&data, id, name, &json!({"status": "pending"}))]
            }
            "session.tool.called" => {
                let name = self
                    .tool_names
                    .get(&tool_key())
                    .cloned()
                    .unwrap_or_default();
                let state = json!({
                    "status": "running",
                    "input": data.get("input").cloned().unwrap_or(json!({})),
                });
                vec![tool_part(&data, id, &name, &state)]
            }
            "session.tool.success" => {
                let name = self.tool_names.remove(&tool_key()).unwrap_or_default();
                let output = data
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                vec![tool_part(
                    &data,
                    id,
                    &name,
                    &json!({"status": "completed", "output": output}),
                )]
            }
            "session.tool.failed" | "session.tool.error" => {
                let name = self.tool_names.remove(&tool_key()).unwrap_or_default();
                let error = data.get("error").cloned().unwrap_or(Value::Null);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| error.as_str().map(str::to_owned))
                    .unwrap_or_default();
                vec![tool_part(
                    &data,
                    id,
                    &name,
                    &json!({"status": "error", "error": message}),
                )]
            }
            "session.step.ended" => {
                let tokens = data.get("tokens").cloned().unwrap_or(Value::Null);
                if !tokens.is_object() {
                    return Vec::new();
                }
                // One step's tokens are the prompt it sent plus its reply, which is what the 1.x
                // assistant message carried. The frame omits the model: the session's latest step
                // supplies it, so the advertised context limit resolves. Marked completed, and
                // keyed by the step's message, so it is counted once.
                let mut info = json!({
                    "sessionID": session(),
                    "id": format!("usage:{}", data.get("assistantMessageID").and_then(Value::as_str).unwrap_or_default()),
                    "role": "assistant",
                    "tokens": tokens,
                    "time": {"completed": true},
                });
                if let Some(model) = data
                    .get("sessionID")
                    .and_then(Value::as_str)
                    .and_then(|id| self.session_models.get(id))
                {
                    info["providerID"] = json!(model.provider_id);
                    info["modelID"] = json!(model.model_id);
                }
                vec![json!({"type": "message.updated", "properties": {"info": info}})]
            }
            // Cumulative session totals (every step, title and compaction summed): spend, not
            // context occupancy.
            "session.usage.updated" => Vec::new(),
            "session.created" => {
                let Some(id) = data.get("sessionID").and_then(Value::as_str) else {
                    return Vec::new();
                };
                vec![json!({
                    "type": "session.created",
                    "properties": {"info": {
                        "id": id,
                        "parentID": data.get("parentID").cloned().unwrap_or(Value::Null),
                        "title": data.get("title").or_else(|| data.get("slug")).cloned().unwrap_or(Value::Null),
                    }}
                })]
            }
            // Same name as 1.x; keep the details for the reply and the approval.
            "permission.asked" => {
                if data.get("id").and_then(Value::as_str).is_none() {
                    return Vec::new();
                }
                vec![json!({"type": "permission.asked", "properties": data})]
            }
            // Dropped on purpose: the server's own chatter.
            other if is_ignored(other) => Vec::new(),
            // A frame this table has never seen: handed on under a name no 1.x frame has, so the
            // reducer counts it (`type:v2:<kind>`) and does nothing else with it.
            other => vec![json!({"type": format!("v2:{other}"), "properties": {}})],
        }
    }
}

/// 2.x frame kinds the driver knows by name and has no use for.
fn is_ignored(kind: &str) -> bool {
    const EXACT: &[&str] = &[
        "session.updated",
        "session.deleted",
        "session.compacted",
        "session.usage.updated",
    ];
    const PREFIXES: &[&str] = &[
        "server.",
        "file.",
        "vcs.",
        "lsp.",
        "mcp.",
        "pty.",
        "tui.",
        "installation.",
        "project.",
        "workspace.",
        "worktree.",
        "integration.",
        "plugin.",
        "todo.",
        "command.",
        "form.",
        "permission.",
        "question.",
        "message.",
    ];
    EXACT.contains(&kind) || PREFIXES.iter().any(|p| kind.starts_with(p))
}

/// 2.x (2.0.24 and later) implements the agent's `question` tool as a session FORM:
/// `form.created` carries `form.metadata.kind == "question"` and one field per question (`q0`..:
/// `title` the header, `description` the question, `options`, `multiselect` for several). Folded
/// into the 1.x `question.asked` payload, carrying each field's key so the reply, which rides the
/// session's form route, can address it. A form of another feature is not a question.
pub(crate) fn question_form_payload(form: &Value) -> Vec<Value> {
    if form.pointer("/metadata/kind").and_then(Value::as_str) != Some("question") {
        return Vec::new();
    }
    let (Some(id), Some(session_id)) = (
        form.get("id").and_then(Value::as_str),
        form.get("sessionID").and_then(Value::as_str),
    ) else {
        return Vec::new();
    };
    let questions: Vec<Value> = form
        .get("fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(|field| {
                    let question = field
                        .get("description")
                        .and_then(Value::as_str)
                        .filter(|q| !q.trim().is_empty())
                        .or_else(|| field.get("title").and_then(Value::as_str))
                        .filter(|q| !q.trim().is_empty())?;
                    Some(json!({
                        "key": field.get("key").and_then(Value::as_str).unwrap_or_default(),
                        "header": field.get("title").and_then(Value::as_str).filter(|h| !h.is_empty()).unwrap_or("Question"),
                        "question": question,
                        "multiple": field.get("type").and_then(Value::as_str) == Some("multiselect"),
                        // The answer is the option's VALUE (the tool sets value == label).
                        "options": field.get("options").and_then(Value::as_array).map(|opts| {
                            opts.iter()
                                .filter_map(|o| {
                                    o.get("value").and_then(Value::as_str)
                                        .or_else(|| o.get("label").and_then(Value::as_str))
                                        .map(|label| json!({"label": label}))
                                })
                                .collect::<Vec<_>>()
                        }).unwrap_or_default(),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    if questions.is_empty() {
        return Vec::new();
    }
    vec![json!({
        "type": "question.asked",
        "properties": {"id": id, "sessionID": session_id, "form": true, "questions": questions}
    })]
}

/// The 2.x error (`{type, message}`) as the 1.x `session.error` properties (`{name, data:
/// {message}}`). A 2.x provider failure can carry an EMPTY message (`provider.auth`): the type
/// stands in, so the failure is never blank.
fn error_payload(data: &Value) -> Value {
    let error = data.get("error").cloned().unwrap_or(Value::Null);
    let name = error.get("type").and_then(Value::as_str).unwrap_or("");
    let message = match error.get("message").and_then(Value::as_str) {
        Some(message) if !message.is_empty() => message,
        _ => name,
    };
    json!({
        "type": "session.error",
        "properties": {
            "sessionID": data.get("sessionID").cloned().unwrap_or(Value::Null),
            "error": {"name": name, "data": {"message": message}},
        }
    })
}

/// A synthetic part id for a streamed 2.x item: message id, kind and ordinal.
fn part_id(data: &Value, kind: char) -> String {
    let message = data
        .get("assistantMessageID")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let ordinal = data.get("ordinal").and_then(Value::as_u64).unwrap_or(0);
    format!("{message}:{kind}{ordinal}")
}

/// A text or reasoning item as a 1.x part.
fn stream_part(data: &Value, part_type: &str, text: Value) -> Value {
    let kind = if part_type == "text" { 't' } else { 'r' };
    json!({
        "type": "message.part.updated",
        "properties": {"part": {
            "sessionID": data.get("sessionID").cloned().unwrap_or(Value::Null),
            "id": part_id(data, kind),
            "messageID": data.get("assistantMessageID").cloned().unwrap_or(Value::Null),
            "type": part_type,
            "text": text,
        }}
    })
}

/// A tool frame as a 1.x tool part. A provider call id can repeat across assistant messages, so
/// the chip's id carries the session and message too.
fn tool_part(data: &Value, id: &str, name: &str, state: &Value) -> Value {
    let id = format!(
        "{}:{}:{id}",
        data.get("sessionID")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        data.get("assistantMessageID")
            .and_then(Value::as_str)
            .unwrap_or_default()
    );
    json!({
        "type": "message.part.updated",
        "properties": {"part": {
            "sessionID": data.get("sessionID").cloned().unwrap_or(Value::Null),
            "messageID": data.get("assistantMessageID").cloned().unwrap_or(Value::Null),
            "id": id,
            "callID": id,
            "type": "tool",
            "tool": name,
            "state": state,
        }}
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn frame(kind: &str, data: Value) -> Value {
        json!({"id": "evt_1", "type": kind, "data": data})
    }

    fn run(frames: Vec<Value>) -> Vec<Value> {
        let mut normalizer = Normalizer::default();
        frames
            .iter()
            .flat_map(|f| normalizer.normalize(f))
            .collect()
    }

    #[test]
    fn a_turn_is_busy_then_idle_and_a_failure_is_an_error_then_idle() {
        let out = run(vec![
            frame("session.execution.started", json!({"sessionID": "ses_a"})),
            frame("session.execution.succeeded", json!({"sessionID": "ses_a"})),
            frame(
                "session.execution.failed",
                json!({"sessionID": "ses_a", "error": {"type": "provider.auth", "message": ""}}),
            ),
        ]);
        assert_eq!(out[0]["properties"]["status"]["type"], "busy");
        assert_eq!(out[1]["type"], "session.idle");
        assert_eq!(out[2]["type"], "session.error");
        assert_eq!(
            out[2]["properties"]["error"]["data"]["message"],
            "provider.auth"
        );
        assert_eq!(out[3]["type"], "session.idle");
    }

    #[test]
    fn a_frame_without_a_session_is_dropped() {
        assert!(run(vec![frame("session.text.delta", json!({"delta": "x"}))]).is_empty());
        assert!(run(vec![json!({"type": "server.connected", "data": {}})]).is_empty());
    }

    #[test]
    fn a_tool_keeps_the_name_only_its_first_frame_carries() {
        let ids = json!({"sessionID": "ses_a", "assistantMessageID": "msg_1", "id": "call_1"});
        let with = |extra: Value| {
            let mut data = ids.clone();
            data.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            data
        };
        let out = run(vec![
            frame("session.tool.input.started", with(json!({"name": "bash"}))),
            frame(
                "session.tool.called",
                with(json!({"input": {"command": "ls"}})),
            ),
            frame(
                "session.tool.success",
                with(json!({"content": [{"text": "a"}, {"text": "b"}]})),
            ),
        ]);
        let part = |i: usize| out[i]["properties"]["part"].clone();
        assert_eq!(part(1)["tool"], "bash");
        assert_eq!(part(1)["state"]["input"]["command"], "ls");
        assert_eq!(part(2)["state"]["status"], "completed");
        assert_eq!(part(2)["state"]["output"], "a\nb");
        assert_eq!(part(2)["callID"], "ses_a:msg_1:call_1");
    }

    #[test]
    fn a_step_end_is_one_completed_usage_keyed_by_its_message_with_the_steps_model() {
        let out = run(vec![
            frame(
                "session.step.started",
                json!({"sessionID": "ses_a", "assistantMessageID": "msg_1",
                       "model": {"providerID": "opencode", "id": "big-pickle"}}),
            ),
            frame(
                "session.step.ended",
                json!({"sessionID": "ses_a", "assistantMessageID": "msg_1",
                       "tokens": {"input": 5, "output": 7, "cache": {"read": 3, "write": 1}}}),
            ),
        ]);
        let info = &out[1]["properties"]["info"];
        assert_eq!(info["id"], "usage:msg_1");
        assert_eq!(info["modelID"], "big-pickle");
        assert_eq!(info["time"]["completed"], true);
    }

    #[test]
    fn an_aborted_step_is_not_an_error_and_another_step_failure_is_a_warning() {
        let aborted = run(vec![frame(
            "session.step.failed",
            json!({"sessionID": "ses_a", "error": {"type": "aborted"}}),
        )]);
        assert!(aborted.is_empty());
        let failed = run(vec![frame(
            "session.step.failed",
            json!({"sessionID": "ses_a", "error": {"type": "rate", "message": "slow down"}}),
        )]);
        assert_eq!(failed[0]["type"], "session.warning");
        assert_eq!(
            failed[0]["properties"]["error"]["data"]["message"],
            "slow down"
        );
    }

    #[test]
    fn a_retry_is_a_status_with_the_attempt_and_a_message() {
        let out = run(vec![frame(
            "session.retry.scheduled",
            json!({"sessionID": "ses_a", "attempt": 3, "at": 9, "error": {"message": "overloaded"}}),
        )]);
        let status = &out[0]["properties"]["status"];
        assert_eq!(status["type"], "retry");
        assert_eq!(status["attempt"], 3);
        assert_eq!(status["message"], "overloaded");
    }

    #[test]
    fn a_question_form_is_a_question_and_another_form_is_not() {
        let form = |kind: &str| {
            frame(
                "form.created",
                json!({"form": {
                    "id": "frm_1", "sessionID": "ses_a", "metadata": {"kind": kind},
                    "fields": [
                        {"key": "q0", "title": "Pick", "description": "Which one?", "type": "select",
                         "options": [{"label": "A", "value": "A"}, {"label": "B", "value": "B"}]},
                        {"key": "q1", "title": "Many", "type": "multiselect", "options": []},
                    ]
                }}),
            )
        };
        let out = run(vec![form("question"), form("other")]);
        assert_eq!(out.len(), 1);
        let props = &out[0]["properties"];
        assert_eq!(props["id"], "frm_1");
        assert_eq!(props["form"], true);
        assert_eq!(props["questions"][0]["question"], "Which one?");
        assert_eq!(props["questions"][0]["options"][1]["label"], "B");
        assert_eq!(props["questions"][1]["multiple"], true);
        assert_eq!(props["questions"][1]["question"], "Many");
    }

    #[test]
    fn closed_tools_are_forgotten_when_the_turn_ends() {
        let mut normalizer = Normalizer::default();
        normalizer.normalize(&frame(
            "session.tool.input.started",
            json!({"sessionID": "ses_a", "assistantMessageID": "m", "id": "c", "name": "bash"}),
        ));
        assert_eq!(normalizer.tool_names.len(), 1);
        normalizer.normalize(&frame(
            "session.execution.interrupted",
            json!({"sessionID": "ses_a"}),
        ));
        assert!(normalizer.tool_names.is_empty());
        assert!(!normalizer.leaking());
    }
}
