//! `policy.toml`: an ordered list of rules that decide what happens to an action
//! (ARCHITECTURE section 8). This version loads, validates and versions the policy. Enforcement
//! is T1.6.

use std::borrow::Cow;
use std::collections::BTreeSet;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::trigger::check_glob;
use super::{Sink, check_ref_name};
use crate::exec::ActionClass;

/// The most rules a policy may hold.
pub(super) const MAX_RULES: usize = 1000;
/// The most bots one rule may be scoped to.
pub(super) const MAX_SCOPE_BOTS: usize = 256;
/// The longest `note`, in characters.
pub(super) const MAX_NOTE_CHARS: usize = 200;

/// What a matching rule does to the action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyEffect {
    /// Let it happen without asking.
    Allow,
    /// Ask a human. With nobody to answer, the answer is no.
    Ask,
    /// Refuse it.
    Deny,
}

/// The policy: rules in order, and what to do when none matches.
///
/// **The first rule that matches decides.** Put the narrow and dangerous rules first.
///
/// ```toml
/// default = "ask"
///
/// [[rules]]
/// note = "never reset hard"
/// effect = "deny"
/// match = { action = "command", command = "git reset --hard*" }
///
/// [[rules]]
/// effect = "allow"
/// match = { action = "command", command = "cargo *", scope = ["keeper", "builder"] }
/// ```
///
/// Paths in a rule are matched relative to the run's worktree when they are inside it, and as
/// absolute paths otherwise. So `src/**` means the worktree's `src`, and `/**` matches only
/// paths outside the worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyDef {
    /// The effect when no rule matches. Defaults to `ask`.
    #[serde(default = "default_effect")]
    pub default: PolicyEffect,
    /// The rules, in order.
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
}

fn default_effect() -> PolicyEffect {
    PolicyEffect::Ask
}

impl Default for PolicyDef {
    fn default() -> Self {
        Self {
            default: default_effect(),
            rules: Vec::new(),
        }
    }
}

/// One rule: if the action matches, the effect applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyRule {
    /// A short reason, shown when the rule decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// What happens to a matching action.
    pub effect: PolicyEffect,
    /// Which actions match. All conditions that are present must hold.
    #[serde(default, rename = "match")]
    pub matches: RuleMatch,
}

/// The conditions of a rule. A condition that is absent holds for every action.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuleMatch {
    /// The class of action: `read`, `edit`, `command`, `network`, `mcp_tool` or `other`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ActionClass>,
    /// A glob over the tool's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// A glob over the command line. Only commands have one, so this needs `action = "command"`
    /// or no `action`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// A glob over a path the action touches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The bots the rule applies to: `"*"` for all, or a list of names. Defaults to `"*"`.
    #[serde(default)]
    pub scope: RuleScope,
}

impl RuleMatch {
    fn is_unconditional(&self) -> bool {
        self.action.is_none()
            && self.tool.is_none()
            && self.command.is_none()
            && self.path.is_none()
            && self.scope == RuleScope::All
    }
}

/// The bots a rule applies to. Written `"*"` or `["keeper", "builder"]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RuleScope {
    /// Every bot.
    #[default]
    All,
    /// Exactly these bots.
    Bots(Vec<String>),
}

impl Serialize for RuleScope {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::All => s.serialize_str("*"),
            Self::Bots(bots) => bots.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for RuleScope {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Star(String),
            Bots(Vec<String>),
        }
        match Repr::deserialize(d)? {
            Repr::Star(star) if star == "*" => Ok(Self::All),
            Repr::Star(other) => Err(serde::de::Error::custom(format!(
                "scope {other:?} must be \"*\" or a list of bot names"
            ))),
            Repr::Bots(bots) => Ok(Self::Bots(bots)),
        }
    }
}

impl JsonSchema for RuleScope {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("RuleScope")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "anyOf": [
                { "type": "string", "const": "*" },
                { "type": "array", "items": { "type": "string" } }
            ]
        })
    }
}

impl PolicyDef {
    pub(super) fn validate(&self, sink: &mut Sink<'_>) {
        if self.rules.len() > MAX_RULES {
            sink.err(
                "rules",
                format!("holds {} rules; the limit is {MAX_RULES}", self.rules.len()),
            );
        }
        for (i, rule) in self.rules.iter().enumerate() {
            rule.validate(&format!("rules[{i}]"), sink);
        }
    }
}

impl PolicyRule {
    fn validate(&self, at: &str, sink: &mut Sink<'_>) {
        if let Some(note) = &self.note
            && note.chars().count() > MAX_NOTE_CHARS
        {
            sink.err(
                format!("{at}.note"),
                format!("is longer than {MAX_NOTE_CHARS} characters"),
            );
        }
        let m = &self.matches;
        for (field, glob) in [
            ("tool", &m.tool),
            ("command", &m.command),
            ("path", &m.path),
        ] {
            if let Some(glob) = glob
                && let Err(message) = check_glob(glob)
            {
                sink.err(format!("{at}.match.{field}"), message);
            }
        }
        if m.command.is_some() && m.action.is_some_and(|a| a != ActionClass::Command) {
            sink.err(
                format!("{at}.match.command"),
                "only commands have a command line; use `action = \"command\"` or leave `action` out",
            );
        }
        match &m.scope {
            RuleScope::All => {}
            RuleScope::Bots(bots) => validate_scope(bots, &format!("{at}.match.scope"), sink),
        }
        if self.effect == PolicyEffect::Allow && m.is_unconditional() {
            sink.err(
                format!("{at}.match"),
                "an `allow` rule that matches everything would switch policy off; narrow it by action, tool, command, path or scope",
            );
        }
    }
}

fn validate_scope(bots: &[String], at: &str, sink: &mut Sink<'_>) {
    if bots.is_empty() {
        sink.err(at, "must list at least one bot, or be \"*\"");
    }
    if bots.len() > MAX_SCOPE_BOTS {
        sink.err(
            at,
            format!("lists {} bots; the limit is {MAX_SCOPE_BOTS}", bots.len()),
        );
    }
    let mut seen = BTreeSet::new();
    for (i, bot) in bots.iter().enumerate() {
        let path = format!("{at}[{i}]");
        if check_ref_name(sink, &path, bot) && !seen.insert(bot.as_str()) {
            sink.err(path, format!("duplicate bot {bot:?}"));
        }
    }
}
