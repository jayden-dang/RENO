//! What the approval panel says about a held action: a kind, a plain sentence, the exact detail.

use reno_proto::{ActionClass, ApprovalSubject};

use crate::summary;

/// The kind of action, which picks the label and the icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Terminal,
    Network,
    Edit,
    Read,
    Tool,
    Other,
}

impl Kind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::Network => "Network",
            Self::Edit => "Edit",
            Self::Read => "Read",
            Self::Tool => "Tool",
            Self::Other => "Action",
        }
    }
}

/// What the panel draws for one approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PanelView {
    pub(crate) kind: Kind,
    /// "May I run this command?"
    pub(crate) sentence: String,
    /// The exact command, URL or path, for the muted mono block.
    pub(crate) detail: String,
}

pub(crate) fn view(subject: &ApprovalSubject) -> PanelView {
    let paths = || {
        subject
            .paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
    };
    let many = subject.paths.len() > 1;
    let (kind, sentence) = match subject.action {
        ActionClass::Command => (Kind::Terminal, "May I run this command?".to_owned()),
        ActionClass::Network => (
            Kind::Network,
            "May I fetch this URL outside the sandbox?".to_owned(),
        ),
        ActionClass::Edit => (
            Kind::Edit,
            if many {
                "May I edit these files?"
            } else {
                "May I edit this file?"
            }
            .to_owned(),
        ),
        ActionClass::Read => (
            Kind::Read,
            if many {
                "May I read these files?"
            } else {
                "May I read this file?"
            }
            .to_owned(),
        ),
        ActionClass::McpTool => (Kind::Tool, format!("May I use the {} tool?", subject.tool)),
        ActionClass::Other => (Kind::Other, "May I do this?".to_owned()),
    };
    let detail = match kind {
        Kind::Network => subject
            .raw
            .get("url")
            .and_then(|v| v.as_str())
            .map_or_else(|| summary::subject_line(subject), str::to_owned),
        Kind::Edit | Kind::Read if !subject.paths.is_empty() => paths().join("\n"),
        _ => summary::subject_line(subject),
    };
    PanelView {
        kind,
        sentence,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject(action: ActionClass) -> ApprovalSubject {
        ApprovalSubject {
            action,
            tool: "Bash".to_owned(),
            command: None,
            paths: Vec::new(),
            raw: serde_json::Value::Null,
        }
    }

    #[test]
    fn a_command_is_a_terminal_question_with_the_exact_command() {
        let v = view(&ApprovalSubject {
            command: Some("git push origin main".to_owned()),
            ..subject(ActionClass::Command)
        });
        assert_eq!(v.kind, Kind::Terminal);
        assert_eq!(v.kind.label(), "Terminal");
        assert_eq!(v.sentence, "May I run this command?");
        assert_eq!(v.detail, "git push origin main");
    }

    #[test]
    fn a_fetch_is_a_network_question_showing_only_the_url() {
        let v = view(&ApprovalSubject {
            raw: serde_json::json!({"url": "https://a.example/feed.json"}),
            ..subject(ActionClass::Network)
        });
        assert_eq!(v.kind.label(), "Network");
        assert_eq!(v.sentence, "May I fetch this URL outside the sandbox?");
        assert_eq!(v.detail, "https://a.example/feed.json");
    }

    #[test]
    fn edits_and_reads_name_their_paths() {
        let v = view(&ApprovalSubject {
            paths: vec!["src/a.rs".into(), "src/b.rs".into()],
            ..subject(ActionClass::Edit)
        });
        assert_eq!(v.kind.label(), "Edit");
        assert_eq!(v.sentence, "May I edit these files?");
        assert_eq!(v.detail, "src/a.rs\nsrc/b.rs");
        let one = view(&ApprovalSubject {
            paths: vec!["src/a.rs".into()],
            ..subject(ActionClass::Read)
        });
        assert_eq!(one.sentence, "May I read this file?");
    }

    #[test]
    fn a_tool_call_names_the_tool_and_anything_else_stays_plain() {
        let tool = view(&ApprovalSubject {
            tool: "mcp__db__query".to_owned(),
            ..subject(ActionClass::McpTool)
        });
        assert_eq!(tool.sentence, "May I use the mcp__db__query tool?");
        assert_eq!(
            view(&subject(ActionClass::Other)).sentence,
            "May I do this?"
        );
    }
}
