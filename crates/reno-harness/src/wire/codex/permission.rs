//! Approval and question requests, as data: what a server-to-client request is about, which
//! access mode lets it through unasked, and the replies the app-server reads back.
//!
//! Sans-IO (ARCH-6). Nothing here reads prose: an approval is an [`ApprovalSubject`] built from
//! the request's structured fields, and a decision is the app-server's own `accept` or `decline`.
//! The shapes follow the app-server protocol of codex-cli 0.164.0 (`codex-rs/app-server-protocol`
//! v2, `core/src/mcp_tool_call.rs`); 0.169.0 is not re-verified (see
//! [`crate::codex::VERIFIED_CLI_VERSION`]).
//!
//! # Request to subject
//!
//! | Request | Becomes | [`ActionClass`] | Subject carries |
//! |---|---|---|---|
//! | `item/commandExecution/requestApproval` | `NeedsApproval` | `Command` | tool `exec`, `command` (a string, or an argv joined by spaces), `paths` = [`cwd`] when sent |
//! | `item/fileChange/requestApproval` | `NeedsApproval` | `Edit` | tool `apply_patch`, `paths` from the request's `changes`, else from the item's own notification, plus `grantRoot` |
//! | `item/permissions/requestApproval` | `NeedsApproval` | `Other` | tool `permissions`; the requested permissions are in `raw` |
//! | `mcpServer/elicitation/request` whose `_meta.codex_approval_kind` is `mcp_tool_call` | `NeedsApproval` | `McpTool` | tool `mcp__<server>__<tool_name>` |
//! | the same with any other `codex_approval_kind` | `NeedsApproval` | `Other` | tool = the kind |
//! | `mcpServer/elicitation/request`, a flat form | `NeedsInput` | | one question per field |
//! | `item/tool/requestUserInput` | `NeedsInput` | | the agent's own questions |
//!
//! Every subject carries the request as Codex sent it (`raw`). An elicitation that is neither an
//! approval nor a flat form (a URL, a device verification) is declined without asking anyone. Any
//! other request is answered with a JSON-RPC error so the app-server never waits on it.
//!
//! # Replies
//!
//! A decision is `accept` or `decline` and nothing else: `acceptForSession`, an execpolicy
//! amendment and a persisted MCP policy are never sent, so Reno decides every time. `cancel` is
//! not sent either (it would interrupt the turn). A permission request is granted for the turn
//! (`scope: "turn"`) or refused with an empty grant.
//!
//! # What an access mode lets through
//!
//! See [`policy`] and [`Authority`]; the table lives on `CodexExecutor`.

use std::path::{Component, Path, PathBuf};

use reno_proto::{AccessMode, ActionClass, ApprovalSubject, Question, SandboxLevel};
use serde_json::{Value, json};

use super::elicitation;
use crate::exec::InputAnswer;

/// The tool name of a command approval and of a command item.
pub(crate) const COMMAND_TOOL: &str = "exec";
/// The tool name of a file-change approval and of a file-change item.
pub(crate) const FILE_CHANGE_TOOL: &str = "apply_patch";
/// The tool name of a permission request.
pub(crate) const PERMISSIONS_TOOL: &str = "permissions";

/// The approval policy and sandbox a run's thread and turns are launched with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Policy {
    /// `approvalPolicy`: `never` or `untrusted`.
    pub approval_policy: &'static str,
    /// The sandbox, never looser than the spec's.
    pub sandbox: SandboxLevel,
}

fn rank(sandbox: SandboxLevel) -> u8 {
    match sandbox {
        SandboxLevel::ReadOnly => 0,
        SandboxLevel::WorkspaceWrite => 1,
        SandboxLevel::Unrestricted => 2,
    }
}

/// The policy for `access` under the spec's sandbox `ceiling`: the mode picks the approval policy
/// and the sandbox it needs, and the spec's sandbox can only narrow that (never run with more
/// than asked, ARCH-15).
///
/// | `AccessMode` | `approvalPolicy` | sandbox before the ceiling |
/// |---|---|---|
/// | `FullAccess` | `never` | `danger-full-access` |
/// | `AutoAcceptEdits` | `untrusted` | `workspace-write` (network on) |
/// | `Supervised` | `untrusted` | `read-only` |
pub(crate) fn policy(access: AccessMode, ceiling: SandboxLevel) -> Policy {
    let (approval_policy, wanted) = match access {
        AccessMode::FullAccess => ("never", SandboxLevel::Unrestricted),
        AccessMode::AutoAcceptEdits => ("untrusted", SandboxLevel::WorkspaceWrite),
        AccessMode::Supervised => ("untrusted", SandboxLevel::ReadOnly),
    };
    Policy {
        approval_policy,
        sandbox: if rank(ceiling) < rank(wanted) {
            ceiling
        } else {
            wanted
        },
    }
}

/// Whether a mode that can ask routes its approvals to the person explicitly: a configured
/// `approvals_reviewer` could otherwise hand them to an AI reviewer instead.
pub(crate) fn asks_person(access: AccessMode) -> bool {
    access != AccessMode::FullAccess
}

/// What the driver answers itself, for one run: the access mode, the sandbox the run really has
/// and its working directory.
///
/// | Request | `FullAccess` | `AutoAcceptEdits` | `Supervised` |
/// |---|---|---|---|
/// | command | accepted | asked | asked |
/// | file change | accepted | accepted when every path is inside the working directory, else asked | asked |
/// | permission grant | accepted | asked | asked |
/// | MCP tool call | accepted | asked | asked |
///
/// "Accepted" for a command, a file change or a permission grant means the run is unconfined:
/// full access *and* an unrestricted sandbox. Under full access with a narrower sandbox Codex
/// never asks (`never`), so a request that arrives anyway asks to leave the sandbox the spec
/// asked for: it is asked, not accepted. A file change inside the working directory needs a
/// sandbox that can write there, and its paths are compared lexically (`..` is resolved, symlinks
/// are not).
#[derive(Debug, Clone)]
pub(crate) struct Authority {
    access: AccessMode,
    sandbox: SandboxLevel,
    cwd: PathBuf,
}

impl Authority {
    pub(crate) fn new(access: AccessMode, ceiling: SandboxLevel, cwd: &Path) -> Self {
        Self {
            access,
            sandbox: policy(access, ceiling).sandbox,
            cwd: cwd.to_path_buf(),
        }
    }

    fn unconfined(&self) -> bool {
        self.access == AccessMode::FullAccess && self.sandbox == SandboxLevel::Unrestricted
    }

    pub(crate) fn accepts_command(&self) -> bool {
        self.unconfined()
    }

    pub(crate) fn accepts_permissions(&self) -> bool {
        self.unconfined()
    }

    pub(crate) fn accepts_mcp_tool(&self) -> bool {
        self.access == AccessMode::FullAccess
    }

    /// `grant_root`: the request asks for write access to a whole root, never accepted unseen
    /// unless the run is unconfined.
    pub(crate) fn accepts_file_change(&self, paths: &[PathBuf], grant_root: bool) -> bool {
        if self.unconfined() {
            return true;
        }
        self.access != AccessMode::Supervised
            && rank(self.sandbox) >= rank(SandboxLevel::WorkspaceWrite)
            && !grant_root
            && !paths.is_empty()
            && paths.iter().all(|path| inside(path, &self.cwd))
    }
}

/// `path` with `.` and `..` resolved lexically.
fn normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Above the root is the root; above a relative path's start stays a `..`.
                if !out.pop() && !out.has_root() {
                    out.push(Component::ParentDir);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Whether `path` is `root` or below it, compared by components.
fn inside(path: &Path, root: &Path) -> bool {
    normalized(path).starts_with(normalized(root))
}

fn resolve(path: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The subject of `item/commandExecution/requestApproval`. The command is a string in the
/// schema; an argv array (older builds) is joined by spaces.
pub(crate) fn command_subject(params: &Value) -> ApprovalSubject {
    let command = match params.get("command") {
        Some(Value::String(command)) => Some(command.clone()),
        Some(Value::Array(parts)) => Some(
            parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    };
    ApprovalSubject {
        action: ActionClass::Command,
        tool: COMMAND_TOOL.to_owned(),
        command: command.filter(|c| !c.is_empty()),
        paths: string(params, "cwd")
            .map(PathBuf::from)
            .into_iter()
            .collect(),
        raw: params.clone(),
    }
}

/// A file-change approval and whether it asks for a write grant on a whole root.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FileChange {
    pub subject: ApprovalSubject,
    pub grant_root: bool,
}

/// The subject of `item/fileChange/requestApproval`. The 0.164.0 request names only its item
/// (`itemId`, `reason`, `grantRoot`): the paths are the ones the item's own notification carried,
/// `known`. A request that carries `changes` itself (older builds) wins. Relative paths are taken
/// from `cwd`; `grantRoot` is listed after the changed paths.
pub(crate) fn file_change(params: &Value, known: Option<&[String]>, cwd: &Path) -> FileChange {
    let inline: Vec<&str> = params
        .get("changes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|change| string(change, "path"))
        .collect();
    let mut paths: Vec<PathBuf> = if inline.is_empty() {
        known
            .unwrap_or_default()
            .iter()
            .map(|path| resolve(path, cwd))
            .collect()
    } else {
        inline.iter().map(|path| resolve(path, cwd)).collect()
    };
    let grant = string(params, "grantRoot");
    if let Some(root) = grant {
        paths.push(resolve(root, cwd));
    }
    FileChange {
        subject: ApprovalSubject {
            action: ActionClass::Edit,
            tool: FILE_CHANGE_TOOL.to_owned(),
            command: None,
            paths,
            raw: params.clone(),
        },
        grant_root: grant.is_some(),
    }
}

/// The subject of `item/permissions/requestApproval`: the requested permissions are in `raw`.
pub(crate) fn permissions_subject(params: &Value) -> ApprovalSubject {
    ApprovalSubject {
        action: ActionClass::Other,
        tool: PERMISSIONS_TOOL.to_owned(),
        command: None,
        paths: Vec::new(),
        raw: params.clone(),
    }
}

/// The reply to a command or file-change approval.
pub(crate) fn decision(accept: bool) -> Value {
    json!({ "decision": if accept { "accept" } else { "decline" } })
}

/// The reply to a permission request: what was asked, for this turn only, or nothing.
pub(crate) fn permissions_reply(params: &Value, accept: bool) -> Value {
    json!({
        "permissions": if accept { params["permissions"].clone() } else { json!({}) },
        "scope": "turn",
    })
}

/// What an `mcpServer/elicitation/request` is.
#[derive(Debug)]
pub(crate) enum Elicitation {
    /// An approval Codex files as an elicitation: an MCP tool call, or another privileged kind.
    Approval(ApprovalSubject),
    /// A flat form to ask.
    Form(elicitation::Form),
    /// Neither: answered with a decline, nobody asked.
    Decline,
}

/// Classify an elicitation by its structured fields: `_meta.codex_approval_kind` marks an
/// approval; anything else is a form or is declined.
pub(crate) fn classify_elicitation(request_id: &str, params: &Value) -> Elicitation {
    let kind = params
        .pointer("/_meta/codex_approval_kind")
        .and_then(Value::as_str);
    let Some(kind) = kind else {
        return elicitation::form(request_id, params)
            .map_or(Elicitation::Decline, Elicitation::Form);
    };
    let server = string(params, "serverName").unwrap_or("mcp");
    let (action, tool) = if kind == "mcp_tool_call" {
        let tool = match params
            .pointer("/_meta/tool_name")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            Some(tool) => format!("mcp__{server}__{tool}"),
            None => format!("mcp__{server}"),
        };
        (ActionClass::McpTool, tool)
    } else {
        (ActionClass::Other, kind.to_owned())
    };
    Elicitation::Approval(ApprovalSubject {
        action,
        tool,
        command: None,
        paths: Vec::new(),
        raw: params.clone(),
    })
}

/// One question of `item/tool/requestUserInput`: the contract's question, and the id the reply
/// is keyed by.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Asked {
    pub wire_id: String,
    pub question: Question,
}

/// Parse `item/tool/requestUserInput`'s questions, tolerantly (field spellings, options as
/// strings or objects). Question ids are `<request_id>:<index>`.
pub(crate) fn user_input_questions(request_id: &str, params: &Value) -> Vec<Asked> {
    params
        .get("questions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(at, q)| {
            let field = |keys: [&str; 3]| {
                keys.iter()
                    .find_map(|k| q.get(*k).and_then(Value::as_str))
                    .unwrap_or("")
                    .to_owned()
            };
            let wire_id = match field(["id", "questionId", "question_id"]) {
                id if id.is_empty() => format!("q{at}"),
                id => id,
            };
            let header = field(["header", "title", "label"]);
            let text = field(["question", "prompt", "text"]);
            Asked {
                wire_id,
                question: Question {
                    id: format!("{request_id}:{at}"),
                    prompt: if header.is_empty() {
                        text
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
                                .unwrap_or("")
                                .to_owned(),
                        })
                        .collect(),
                    multi_select: ["multiSelect", "multi_select"]
                        .iter()
                        .find_map(|k| q.get(*k).and_then(Value::as_bool))
                        .unwrap_or(false),
                },
            }
        })
        .collect()
}

/// The reply to `item/tool/requestUserInput`: `{ answers: { <wire id>: { answers: [..] } } }`.
/// A question nobody answered has no entry; no answers at all is an empty map, the refusal.
pub(crate) fn user_input_reply(asked: &[Asked], answers: &[InputAnswer]) -> Value {
    let mut by_id = serde_json::Map::new();
    for a in asked {
        if let Some(given) = answers.iter().find(|g| g.question_id == a.question.id) {
            by_id.insert(a.wire_id.clone(), json!({ "answers": given.labels }));
        }
    }
    json!({ "answers": by_id })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn authority(access: AccessMode, ceiling: SandboxLevel) -> Authority {
        Authority::new(access, ceiling, Path::new("/work"))
    }

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn the_access_table_never_exceeds_the_sandbox_the_spec_asks_for() {
        use AccessMode::*;
        use SandboxLevel::*;
        let policy = |access, ceiling| {
            let p = super::policy(access, ceiling);
            (p.approval_policy, p.sandbox)
        };
        assert_eq!(policy(FullAccess, Unrestricted), ("never", Unrestricted));
        assert_eq!(
            policy(FullAccess, WorkspaceWrite),
            ("never", WorkspaceWrite)
        );
        assert_eq!(policy(FullAccess, ReadOnly), ("never", ReadOnly));
        assert_eq!(
            policy(AutoAcceptEdits, Unrestricted),
            ("untrusted", WorkspaceWrite)
        );
        assert_eq!(
            policy(AutoAcceptEdits, WorkspaceWrite),
            ("untrusted", WorkspaceWrite)
        );
        assert_eq!(policy(AutoAcceptEdits, ReadOnly), ("untrusted", ReadOnly));
        for ceiling in [Unrestricted, WorkspaceWrite, ReadOnly] {
            assert_eq!(policy(Supervised, ceiling), ("untrusted", ReadOnly));
        }
        assert!(!asks_person(FullAccess));
        assert!(asks_person(AutoAcceptEdits));
        assert!(asks_person(Supervised));
    }

    #[test]
    fn only_an_unconfined_run_answers_commands_and_permissions_itself() {
        use AccessMode::*;
        use SandboxLevel::*;
        let unconfined = authority(FullAccess, Unrestricted);
        assert!(unconfined.accepts_command() && unconfined.accepts_permissions());
        for confined in [
            authority(FullAccess, WorkspaceWrite),
            authority(AutoAcceptEdits, Unrestricted),
            authority(Supervised, Unrestricted),
        ] {
            assert!(
                !confined.accepts_command() && !confined.accepts_permissions(),
                "{confined:?}"
            );
        }
        // An MCP tool call leaves no sandbox: full access answers it, whatever the sandbox.
        assert!(authority(FullAccess, ReadOnly).accepts_mcp_tool());
        assert!(!authority(AutoAcceptEdits, Unrestricted).accepts_mcp_tool());
        assert!(!authority(Supervised, Unrestricted).accepts_mcp_tool());
    }

    #[test]
    fn a_file_change_is_accepted_only_inside_the_working_directory() {
        use AccessMode::*;
        use SandboxLevel::*;
        let auto = authority(AutoAcceptEdits, Unrestricted);
        assert!(auto.accepts_file_change(&paths(&["/work/a.rs", "/work/src/b.rs"]), false));
        // Outside, a sibling that shares a prefix, and a climb out through `..`.
        assert!(!auto.accepts_file_change(&paths(&["/work/a.rs", "/other/b.rs"]), false));
        assert!(!auto.accepts_file_change(&paths(&["/work-evil/a.rs"]), false));
        assert!(!auto.accepts_file_change(&paths(&["/work/../etc/passwd"]), false));
        assert!(auto.accepts_file_change(&paths(&["/work/src/../a.rs"]), false));
        // Nothing known, or a session-wide grant: asked.
        assert!(!auto.accepts_file_change(&[], false));
        assert!(!auto.accepts_file_change(&paths(&["/work/a.rs"]), true));
        // A sandbox that cannot write, or a mode that asks about everything: asked.
        assert!(
            !authority(AutoAcceptEdits, ReadOnly)
                .accepts_file_change(&paths(&["/work/a.rs"]), false)
        );
        assert!(
            !authority(Supervised, Unrestricted)
                .accepts_file_change(&paths(&["/work/a.rs"]), false)
        );
        // Full access: anything when unconfined, the working directory when confined.
        let full = authority(FullAccess, Unrestricted);
        assert!(full.accepts_file_change(&paths(&["/anywhere"]), true));
        let confined = authority(FullAccess, WorkspaceWrite);
        assert!(confined.accepts_file_change(&paths(&["/work/a.rs"]), false));
        assert!(!confined.accepts_file_change(&paths(&["/anywhere"]), false));
    }

    #[test]
    fn a_command_approval_is_a_command_with_its_working_directory() {
        let params = json!({
            "threadId": "t", "turnId": "u", "itemId": "c1", "startedAtMs": 1,
            "command": "rm -rf /tmp/x", "cwd": "/repo", "reason": "cleanup",
            "commandActions": [{"type": "unknown", "command": "rm -rf /tmp/x"}],
        });
        let subject = command_subject(&params);
        assert_eq!(subject.action, ActionClass::Command);
        assert_eq!(subject.tool, "exec");
        assert_eq!(subject.command.as_deref(), Some("rm -rf /tmp/x"));
        assert_eq!(subject.paths, paths(&["/repo"]));
        assert_eq!(subject.raw, params);

        // An older build's argv, and a request with neither command nor cwd (a stdin write).
        let argv = command_subject(&json!({"command": ["git", "push", "--force"]}));
        assert_eq!(argv.command.as_deref(), Some("git push --force"));
        assert!(argv.paths.is_empty());
        let stdin = command_subject(&json!({"kind": "writeStdin", "itemId": "c2"}));
        assert_eq!(stdin.action, ActionClass::Command);
        assert_eq!(stdin.command, None);
    }

    #[test]
    fn a_file_change_approval_takes_its_paths_from_the_request_or_its_item() {
        let cwd = Path::new("/work");
        // The 0.164.0 shape: the item's paths were seen earlier, one relative.
        let bare = json!({"itemId": "f1", "reason": "outside", "grantRoot": "/work/pkg"});
        let known = ["/work/a.rs".to_owned(), "b.rs".to_owned()];
        let change = file_change(&bare, Some(&known), cwd);
        assert_eq!(change.subject.action, ActionClass::Edit);
        assert_eq!(change.subject.tool, "apply_patch");
        assert_eq!(
            change.subject.paths,
            paths(&["/work/a.rs", "/work/b.rs", "/work/pkg"])
        );
        assert!(change.grant_root);
        assert_eq!(change.subject.raw, bare);

        // Older builds name the changes in the request, and that wins.
        let inline = json!({"changes": [{"path": "/x/a.rs", "kind": "add"}, {"kind": "add"}]});
        let change = file_change(&inline, Some(&known), cwd);
        assert_eq!(change.subject.paths, paths(&["/x/a.rs"]));
        assert!(!change.grant_root);

        // Never seen: no paths, which no mode accepts unseen.
        assert!(file_change(&bare, None, cwd).subject.paths.len() == 1);
        assert!(
            file_change(&json!({"itemId": "z"}), None, cwd)
                .subject
                .paths
                .is_empty()
        );
    }

    #[test]
    fn replies_are_accept_or_decline_and_never_a_session_grant() {
        assert_eq!(decision(true), json!({"decision": "accept"}));
        assert_eq!(decision(false), json!({"decision": "decline"}));
        let params = json!({"permissions": {"network": {"enabled": true}}});
        assert_eq!(
            permissions_reply(&params, true),
            json!({"permissions": {"network": {"enabled": true}}, "scope": "turn"})
        );
        assert_eq!(
            permissions_reply(&params, false),
            json!({"permissions": {}, "scope": "turn"})
        );
        for reply in [
            decision(true),
            decision(false),
            permissions_reply(&params, true),
        ] {
            assert!(!reply.to_string().contains("ForSession"), "{reply}");
        }
    }

    #[test]
    fn permissions_keep_what_was_asked_in_raw() {
        let params = json!({"reason": "fetch", "permissions": {"network": {"enabled": true}}});
        let subject = permissions_subject(&params);
        assert_eq!(subject.action, ActionClass::Other);
        assert_eq!(subject.tool, "permissions");
        assert_eq!(subject.raw["permissions"]["network"]["enabled"], true);
    }

    #[test]
    fn an_elicitation_is_an_approval_a_form_or_a_decline_by_its_structure() {
        let tool = json!({
            "serverName": "reno", "mode": "form", "message": "Allow it?",
            "_meta": {"codex_approval_kind": "mcp_tool_call", "tool_name": "board_post"},
            "requestedSchema": {"type": "object", "properties": {}},
        });
        match classify_elicitation("r1", &tool) {
            Elicitation::Approval(subject) => {
                assert_eq!(subject.action, ActionClass::McpTool);
                assert_eq!(subject.tool, "mcp__reno__board_post");
                assert_eq!(subject.raw, tool);
            }
            other => panic!("{other:?}"),
        }
        // No tool name: the server is still named.
        let nameless =
            json!({"serverName": "reno", "_meta": {"codex_approval_kind": "mcp_tool_call"}});
        assert!(matches!(
            classify_elicitation("r1", &nameless),
            Elicitation::Approval(s) if s.tool == "mcp__reno"
        ));
        // Another privileged kind is an approval of no known class.
        let other = json!({"serverName": "apps", "_meta": {"codex_approval_kind": "browser_auth"}});
        assert!(matches!(
            classify_elicitation("r1", &other),
            Elicitation::Approval(s) if s.action == ActionClass::Other && s.tool == "browser_auth"
        ));
        // Prose never makes an approval: the same words with no kind are only a form.
        let prose = json!({
            "serverName": "reno", "mode": "form", "message": "Allow the reno MCP server to run tool \"x\"?",
            "requestedSchema": {"type": "object", "properties": {}},
        });
        assert!(matches!(
            classify_elicitation("r1", &prose),
            Elicitation::Form(_)
        ));
        let url = json!({"mode": "url", "url": "https://x", "message": "m", "elicitationId": "e"});
        assert!(matches!(
            classify_elicitation("r1", &url),
            Elicitation::Decline
        ));
    }

    #[test]
    fn user_input_questions_are_keyed_by_their_wire_id() {
        let params = json!({"questions": [
            {"id": "mode", "header": "Mode", "question": "Which mode?",
             "options": [{"label": "fast", "description": "d"}, "safe"], "multiSelect": true},
            {"question": "Why?"},
        ]});
        let asked = user_input_questions("r7", &params);
        assert_eq!(asked.len(), 2);
        assert_eq!(asked[0].wire_id, "mode");
        assert_eq!(asked[0].question.id, "r7:0");
        assert_eq!(asked[0].question.prompt, "Mode: Which mode?");
        assert_eq!(asked[0].question.options, ["fast", "safe"]);
        assert!(asked[0].question.multi_select);
        assert_eq!(asked[1].wire_id, "q1");
        assert_eq!(asked[1].question.prompt, "Why?");

        let answers = vec![InputAnswer {
            question_id: "r7:0".into(),
            labels: vec!["safe".into()],
        }];
        assert_eq!(
            user_input_reply(&asked, &answers),
            json!({"answers": {"mode": {"answers": ["safe"]}}})
        );
        // Declined: an empty map, which the agent reads as no answer.
        assert_eq!(user_input_reply(&asked, &[]), json!({"answers": {}}));
    }
}
