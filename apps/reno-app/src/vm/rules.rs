//! Standing rules: what the Inbox card says about keeping an answer, and the rows of Settings →
//! Policy that list and revoke them. From `rule.list` and the held approval's own subject.

use reno_proto::{
    ActionClass, ApprovalSubject, CommandPattern, RememberScope, RuleId, RuleMatcher,
    StandingEffect, StandingRule, StandingScope,
};

use super::Section;
use crate::summary;

/// One standing rule, as a table row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuleRow {
    pub(crate) id: RuleId,
    pub(crate) effect: StandingEffect,
    /// "this run", "keeper", "every run".
    pub(crate) scope: String,
    /// What it holds, on one line: "command `git push`".
    pub(crate) matches: String,
    /// "never", "in 59m", "expired".
    pub(crate) ends: String,
    pub(crate) expired: bool,
}

/// What a rule matches, in words and the exact text it holds.
pub(crate) fn matcher_text(matcher: &RuleMatcher) -> String {
    let what = match matcher.action {
        ActionClass::Command => "command",
        ActionClass::Network => "network",
        ActionClass::Edit => "edit",
        ActionClass::Read => "read",
        ActionClass::McpTool => "tool call",
        ActionClass::Other => "action",
    };
    let mut text = format!("{what} {}", matcher.tool);
    match &matcher.command {
        Some(CommandPattern::Exact { command }) => text.push_str(&format!(" `{command}`")),
        Some(CommandPattern::Glob { pattern }) => text.push_str(&format!(" matching `{pattern}`")),
        None => {}
    }
    if !matcher.paths.is_empty() {
        text.push_str(&format!(" on {}", matcher.paths.join(", ")));
    }
    if matcher.command.is_none() && matcher.paths.is_empty() {
        text.push_str(" (any use)");
    }
    text
}

fn scope_text(scope: &StandingScope) -> String {
    match scope {
        StandingScope::Run { .. } => "one run".to_owned(),
        StandingScope::Bot { bot } => format!("bot {bot}"),
        StandingScope::Project { root } => format!("project {}", root.display()),
        StandingScope::Always => "every run".to_owned(),
    }
}

/// The rules in force, newest first, as rows.
pub(crate) fn rows(
    rules: Option<&Result<Vec<StandingRule>, String>>,
    supported: bool,
    now_ms: u64,
) -> Section<RuleRow> {
    if !supported {
        return Section::Unavailable;
    }
    match rules {
        None => Section::Loading,
        Some(Err(reason)) => Section::Failed(reason.clone()),
        Some(Ok(rules)) => {
            let rows: Vec<RuleRow> = rules
                .iter()
                .filter(|rule| rule.revoked_at_ms.is_none())
                .map(|rule| {
                    let expired = rule.expires_at_ms.is_some_and(|at| at <= now_ms);
                    RuleRow {
                        id: rule.id.clone(),
                        effect: rule.effect,
                        scope: scope_text(&rule.scope),
                        matches: matcher_text(&rule.matcher),
                        ends: match rule.expires_at_ms {
                            None => "never".to_owned(),
                            Some(_) if expired => "expired".to_owned(),
                            Some(at) => format!("in {}", summary::age_label(at - now_ms)),
                        },
                        expired,
                    }
                })
                .collect();
            if rows.is_empty() {
                Section::Empty
            } else {
                Section::Rows(rows)
            }
        }
    }
}

/// How far an answer on a held card is kept, as the card's choice names it.
pub(crate) fn choices(has_bot: bool) -> Vec<(Option<RememberScope>, &'static str)> {
    let mut all = vec![(None, "Once"), (Some(RememberScope::Run), "This run")];
    if has_bot {
        all.push((Some(RememberScope::Bot), "This bot"));
    }
    all.push((Some(RememberScope::Always), "Always"));
    all
}

/// One sentence under the card's choice: what exactly is kept, and for whom. It names the
/// subject's own parts, because that is all the rule will ever match.
pub(crate) fn hint(subject: &ApprovalSubject, scope: RememberScope, bot: Option<&str>) -> String {
    let who = match scope {
        RememberScope::Run => "this run".to_owned(),
        RememberScope::Bot => format!("every run of {}", bot.unwrap_or("this bot")),
        RememberScope::Project => "every run in this project".to_owned(),
        RememberScope::Always => "every run".to_owned(),
    };
    let what = match (&subject.command, subject.paths.is_empty()) {
        (Some(command), _) => format!("exactly `{command}`"),
        (None, false) => format!(
            "{} on exactly {}",
            subject.tool,
            subject
                .paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        (None, true) => format!("every use of {}", subject.tool),
    };
    format!(
        "Your answer is kept as a rule for {what}, for {who}. Nothing broader; revoke it in Settings."
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn subject(command: Option<&str>, paths: &[&str]) -> ApprovalSubject {
        ApprovalSubject {
            action: if command.is_some() {
                ActionClass::Command
            } else {
                ActionClass::Edit
            },
            tool: if command.is_some() { "bash" } else { "write" }.into(),
            command: command.map(str::to_owned),
            paths: paths.iter().map(Into::into).collect(),
            raw: serde_json::Value::Null,
        }
    }

    fn rule(id: &str, effect: StandingEffect, expires: Option<u64>, revoked: bool) -> StandingRule {
        StandingRule {
            id: RuleId::from_suffix(id).unwrap(),
            scope: StandingScope::Bot {
                bot: "keeper".into(),
            },
            effect,
            matcher: RuleMatcher::derive(&subject(Some("git push"), &[]), "/w".as_ref()),
            created_by: "human".into(),
            created_at_ms: 0,
            expires_at_ms: expires,
            revoked_at_ms: revoked.then_some(1),
            source_approval: None,
        }
    }

    #[test]
    fn the_hint_names_exactly_what_is_kept_and_for_whom() {
        let push = subject(Some("git push"), &[]);
        assert_eq!(
            hint(&push, RememberScope::Bot, Some("keeper")),
            "Your answer is kept as a rule for exactly `git push`, for every run of keeper. Nothing broader; revoke it in Settings."
        );
        assert!(hint(&push, RememberScope::Run, None).contains("exactly `git push`, for this run"));
        let edit = subject(None, &["src/lib.rs"]);
        assert!(
            hint(&edit, RememberScope::Always, None)
                .contains("write on exactly src/lib.rs, for every run")
        );
        // A subject with only a tool is a grant for the whole tool, and the card says so.
        let fetch = ApprovalSubject {
            action: ActionClass::Network,
            tool: "fetch".into(),
            command: None,
            paths: vec![],
            raw: serde_json::Value::Null,
        };
        assert!(hint(&fetch, RememberScope::Always, None).contains("every use of fetch"));
    }

    #[test]
    fn a_run_with_no_bot_is_not_offered_the_bot_scope() {
        let with: Vec<_> = choices(true).into_iter().map(|c| c.1).collect();
        assert_eq!(with, ["Once", "This run", "This bot", "Always"]);
        let without: Vec<_> = choices(false).into_iter().map(|c| c.1).collect();
        assert_eq!(without, ["Once", "This run", "Always"]);
        assert_eq!(choices(true)[0].0, None, "the default is once");
    }

    #[test]
    fn rows_say_loading_unavailable_failed_empty_and_rows() {
        assert_eq!(rows(None, false, 0), Section::Unavailable);
        assert_eq!(rows(None, true, 0), Section::Loading);
        assert_eq!(
            rows(Some(&Err("down".into())), true, 0),
            Section::Failed("down".into())
        );
        assert_eq!(rows(Some(&Ok(vec![])), true, 0), Section::Empty);
        let listed = vec![
            rule("a", StandingEffect::Allow, None, false),
            rule("b", StandingEffect::Deny, Some(3_600_000), false),
            rule("c", StandingEffect::Allow, Some(5), false),
            rule("d", StandingEffect::Allow, None, true),
        ];
        let section = rows(Some(&Ok(listed)), true, 10);
        let rows = section.rows();
        assert_eq!(rows.len(), 3, "a revoked rule is not listed");
        assert_eq!(rows[0].matches, "command bash `git push`");
        assert_eq!(rows[0].scope, "bot keeper");
        assert_eq!(rows[0].ends, "never");
        assert!(rows[1].ends.starts_with("in "), "{}", rows[1].ends);
        assert_eq!((rows[2].ends.as_str(), rows[2].expired), ("expired", true));
    }

    #[test]
    fn a_rule_with_only_a_tool_says_any_use() {
        let fetch = ApprovalSubject {
            action: ActionClass::Network,
            tool: "fetch".into(),
            command: None,
            paths: vec![],
            raw: serde_json::Value::Null,
        };
        let matcher = RuleMatcher::derive(&fetch, "/w".as_ref());
        assert_eq!(matcher_text(&matcher), "network fetch (any use)");
    }
}
