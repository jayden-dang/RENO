//! The inline config of a run, and the read-back that proves it is in force.
//!
//! opencode reads `OPENCODE_CONFIG_CONTENT` as the last user config layer (after the owner's
//! global config and the project's `opencode.json`), so what is built here outranks both and
//! nothing is written to either (Reno never writes opencode's configuration). It carries:
//!
//! - the permission rules of the access mode. opencode allows every tool by default and raises a
//!   request only for an `ask` rule, so a restricted mode adds them: a catch-all `ask`, then
//!   `allow` for what only looks (read, glob, grep, list, lsp, the todo list, questions,
//!   subagents, skills), then `ask` for commands, the network and, supervised, edits. Rules match
//!   last-first, so the named ones win over the catch-all and over the owner's;
//! - the MCP servers of the run (`mcp.<name>` on 1.x, `mcp.servers.<name>` on 2.x);
//! - on 1.x, `permission.question: allow`: the agents deny the `question` tool there unless a
//!   client app asked for it (1.19.5, read off `/agent`).
//!
//! Which generation reads which shape was checked against a real server by reading the config
//! back: 1.19.5 through `/agent`, 2.3.2 through `/api/agent/{id}` and `/api/mcp`. A server whose
//! read-back lacks the rules is never run in a restricted mode ([`rules_in_force`]).

use reno_proto::{AccessMode, McpServerSpec};
use serde_json::{Map, Value, json};

use super::version::Protocol;

/// What only looks, or keeps the agent's own notes: allowed unasked in a restricted mode.
const UNASKED: [&str; 10] = [
    "read",
    "glob",
    "grep",
    "list",
    "lsp",
    "todoread",
    "todowrite",
    "question",
    "task",
    "skill",
];

/// Commands and the network: asked in both restricted modes. Reaching outside the project
/// (`external_directory`) and a repeated identical call (`doom_loop`) are opencode's own asks
/// already.
const ASKED: [&str; 3] = ["bash", "webfetch", "websearch"];

/// Where the `plan` agent writes its plan (a path relative to the worktree); `*` spans
/// directories.
const PLAN_FILES: &str = "*opencode/plans/*.md";

/// What to put in the inline config.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RunConfig<'a> {
    /// The generation the server speaks.
    pub(crate) protocol: Protocol,
    /// What the run may do unasked.
    pub(crate) access: AccessMode,
    /// The MCP servers to register, Reno's own first.
    pub(crate) mcp: &'a [McpServerSpec],
}

impl RunConfig<'_> {
    /// The config, or none when there is nothing to say.
    pub(crate) fn build(&self) -> Option<Value> {
        let restricted = self.access != AccessMode::FullAccess;
        if self.protocol == Protocol::V2 && !restricted && self.mcp.is_empty() {
            return None;
        }
        let mut config = Map::new();
        match self.protocol {
            Protocol::V1 => {
                config.insert("permission".into(), self.v1_permission());
                if self.access == AccessMode::Supervised {
                    // The general `edit: ask` would also relax the plan agent's own edit
                    // denial (1.19.4): it gets the denial back, asking before its plan file.
                    config.insert(
                        "agent".into(),
                        json!({"plan": {"permission": {"edit": {"*": "deny", PLAN_FILES: "ask"}}}}),
                    );
                }
                if !self.mcp.is_empty() {
                    config.insert("mcp".into(), self.mcp_servers());
                }
            }
            Protocol::V2 => {
                if restricted {
                    config.insert("permissions".into(), Value::Array(self.v2_permissions()));
                }
                if self.access == AccessMode::Supervised {
                    config.insert(
                        "agents".into(),
                        json!({"plan": {"permissions": [rule("edit", "*", "deny"), rule("edit", PLAN_FILES, "ask")]}}),
                    );
                }
                if !self.mcp.is_empty() {
                    config.insert("mcp".into(), json!({"servers": self.mcp_servers()}));
                }
            }
        }
        Some(Value::Object(config))
    }

    fn edit_effect(&self) -> &'static str {
        if self.access == AccessMode::Supervised {
            "ask"
        } else {
            "allow"
        }
    }

    fn v1_permission(&self) -> Value {
        let mut rules = Map::new();
        if self.access == AccessMode::FullAccess {
            // Allow-all stays: the agents' own default. Only the question tool is opened.
            rules.insert("question".into(), json!("allow"));
            return Value::Object(rules);
        }
        // The catch-all leads so every named rule after it wins.
        rules.insert("*".into(), json!("ask"));
        for permission in UNASKED {
            rules.insert(permission.into(), json!("allow"));
        }
        rules.insert("edit".into(), json!(self.edit_effect()));
        for permission in ASKED {
            rules.insert(permission.into(), json!("ask"));
        }
        Value::Object(rules)
    }

    fn v2_permissions(&self) -> Vec<Value> {
        let mut rules = vec![rule("*", "*", "ask")];
        rules.extend(UNASKED.iter().map(|action| rule(action, "*", "allow")));
        rules.push(rule("edit", "*", self.edit_effect()));
        rules.extend(ASKED.iter().map(|action| rule(action, "*", "ask")));
        rules
    }

    fn mcp_servers(&self) -> Value {
        let mut servers = Map::new();
        for server in self.mcp {
            let command: Vec<&str> = std::iter::once(server.command.as_str())
                .chain(server.args.iter().map(String::as_str))
                .collect();
            let mut entry = json!({"type": "local", "command": command, "environment": server.env});
            match self.protocol {
                Protocol::V1 => entry["enabled"] = json!(true),
                Protocol::V2 => entry["disabled"] = json!(false),
            }
            servers.insert(server.name.clone(), entry);
        }
        Value::Object(servers)
    }
}

fn rule(action: &str, resource: &str, effect: &str) -> Value {
    json!({"action": action, "resource": resource, "effect": effect})
}

/// One permission rule, read from either generation's shape.
#[derive(Debug, PartialEq, Eq)]
struct Rule<'a> {
    action: &'a str,
    resource: &'a str,
    effect: &'a str,
}

impl<'a> Rule<'a> {
    /// 1.x `{permission, pattern, action}` or 2.x `{action, resource, effect}`.
    fn parse(value: &'a Value) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(Value::as_str);
        match (text("permission"), text("pattern"), text("action")) {
            (Some(action), Some(resource), Some(effect)) => Some(Self {
                action,
                resource,
                effect,
            }),
            _ => Some(Self {
                action: text("action")?,
                resource: text("resource")?,
                effect: text("effect")?,
            }),
        }
    }
}

/// `*` matches any run of characters, anything else itself.
fn glob(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, tail)) => text.strip_prefix(head).is_some_and(|rest| {
            (0..=rest.len())
                .filter(|at| rest.is_char_boundary(*at))
                .any(|at| glob(tail, &rest[at..]))
        }),
    }
}

/// What opencode does for `action` on `resource` under `rules`: the last matching rule, `ask` when
/// none matches (its own default).
fn effective<'a>(rules: &'a [Value], action: &str, resource: &str) -> &'a str {
    rules
        .iter()
        .filter_map(Rule::parse)
        .rfind(|rule| glob(rule.action, action) && glob(rule.resource, resource))
        .map_or("ask", |rule| rule.effect)
}

/// Whether the agent as the server reports it (`/agent` item on 1.x, `/api/agent/{id}` data on
/// 2.x) is held to the rules of `access`: a command, a fetch and a search ask, so does an edit
/// when supervised, and a tool nobody named asks. `Err` says what is missing. Full access has no
/// rules to prove.
///
/// The read-back is what makes a restricted mode fail closed: a server that read a different
/// config shape (a later opencode), or an owner's config that outranks ours, ends the run before
/// a prompt is sent instead of running it unasked.
pub(crate) fn rules_in_force(agent: &Value, access: AccessMode) -> Result<(), String> {
    if access == AccessMode::FullAccess {
        return Ok(());
    }
    let rules = agent
        .get("permission")
        .or_else(|| agent.get("permissions"))
        .and_then(Value::as_array)
        .ok_or("the agent lists no permission rules")?;
    let mut must_ask = vec!["bash", "webfetch", "websearch", "mcp__a_tool_nobody_named"];
    if access == AccessMode::Supervised {
        must_ask.push("edit");
    }
    let loose: Vec<&str> = must_ask
        .into_iter()
        .filter(|action| effective(rules, action, "*") == "allow")
        .collect();
    if loose.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "opencode does not ask before {}: the permission rules Reno set are not in force",
            loose.join(", ")
        ))
    }
}

/// Whether a server that was given `names` as MCP servers lists them all (`GET /mcp` on 1.x:
/// `{name: {status}}`; `GET /api/mcp` on 2.x: `{data: [{name, status}]}`).
pub(crate) fn mcp_registered(listing: &Value, names: &[&str]) -> Result<(), String> {
    let listed: Vec<&str> = match super::requests::unwrap_data(listing) {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("name").and_then(Value::as_str))
            .collect(),
        Value::Object(map) => map.keys().map(String::as_str).collect(),
        _ => Vec::new(),
    };
    let missing: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| !listed.contains(n))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "opencode did not register the MCP server{} {}",
            if missing.len() == 1 { "" } else { "s" },
            missing.join(", ")
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn mcp(name: &str) -> McpServerSpec {
        McpServerSpec {
            name: name.into(),
            command: "/bin/reno-mcp".into(),
            args: vec!["--stdio".into()],
            env: BTreeMap::from([("RENO_RUN".into(), "r1".into())]),
        }
    }

    fn build(protocol: Protocol, access: AccessMode, servers: &[McpServerSpec]) -> Option<Value> {
        RunConfig {
            protocol,
            access,
            mcp: servers,
        }
        .build()
    }

    #[test]
    fn v1_full_access_only_opens_the_question_tool_and_registers_mcp() {
        let config = build(Protocol::V1, AccessMode::FullAccess, &[mcp("reno")]).unwrap();
        assert_eq!(
            config,
            json!({
                "permission": {"question": "allow"},
                "mcp": {"reno": {
                    "type": "local", "command": ["/bin/reno-mcp", "--stdio"],
                    "environment": {"RENO_RUN": "r1"}, "enabled": true
                }}
            })
        );
    }

    #[test]
    fn v2_full_access_with_nothing_to_register_has_no_config_at_all() {
        assert!(build(Protocol::V2, AccessMode::FullAccess, &[]).is_none());
        let config = build(Protocol::V2, AccessMode::FullAccess, &[mcp("reno")]).unwrap();
        assert_eq!(config["mcp"]["servers"]["reno"]["disabled"], false);
        assert!(config.get("permissions").is_none());
    }

    #[test]
    fn v1_supervised_asks_for_edits_commands_and_the_network_and_lets_looking_through() {
        let config = build(Protocol::V1, AccessMode::Supervised, &[]).unwrap();
        let rules = &config["permission"];
        // The catch-all comes first: rule order is insertion order of the JSON object.
        assert_eq!(rules.as_object().unwrap().keys().next().unwrap(), "*");
        assert_eq!(rules["*"], "ask");
        for allowed in UNASKED {
            assert_eq!(rules[allowed], "allow", "{allowed}");
        }
        for asked in ["edit", "bash", "webfetch", "websearch"] {
            assert_eq!(rules[asked], "ask", "{asked}");
        }
        assert_eq!(config["agent"]["plan"]["permission"]["edit"]["*"], "deny");
    }

    #[test]
    fn auto_accept_edits_lets_edits_through_and_keeps_asking_for_the_rest() {
        let v1 = build(Protocol::V1, AccessMode::AutoAcceptEdits, &[]).unwrap();
        assert_eq!(v1["permission"]["edit"], "allow");
        assert_eq!(v1["permission"]["bash"], "ask");
        assert!(v1.get("agent").is_none());
        let v2 = build(Protocol::V2, AccessMode::AutoAcceptEdits, &[]).unwrap();
        let rules = v2["permissions"].as_array().unwrap();
        assert_eq!(effective(rules, "edit", "/w/a.rs"), "allow");
        assert_eq!(effective(rules, "bash", "ls"), "ask");
        assert_eq!(effective(rules, "mcp__x__y", "*"), "ask");
        assert_eq!(effective(rules, "read", "/w/a.rs"), "allow");
        assert!(v2.get("agents").is_none());
    }

    #[test]
    fn v2_supervised_gives_the_plan_agent_its_edit_denial_back() {
        let v2 = build(Protocol::V2, AccessMode::Supervised, &[]).unwrap();
        let plan = v2["agents"]["plan"]["permissions"].as_array().unwrap();
        assert_eq!(effective(plan, "edit", "src/a.rs"), "deny");
        assert_eq!(effective(plan, "edit", ".opencode/plans/p.md"), "ask");
    }

    #[test]
    fn globs_match_the_way_opencodes_rules_do() {
        assert!(glob("*", "anything"));
        assert!(glob("read", "read"));
        assert!(!glob("read", "readme"));
        assert!(glob("*.env", "a/b.env"));
        assert!(!glob("*.env", "a/b.env.x"));
        assert!(glob("*opencode/plans/*.md", ".opencode/plans/p.md"));
        assert!(!glob("*opencode/plans/*.md", ".opencode/plans/p.txt"));
        assert!(glob("a*b*c", "aXbYc"));
        assert!(!glob("a*b*c", "aXcYb"));
    }

    /// The read-back of the build agent from a real 1.19.5, started with exactly the supervised
    /// config above (`/agent`, trimmed to the fields read).
    fn real_1_19_5_build_agent() -> Value {
        let rules: Vec<Value> = [
            ("*", "*", "allow"),
            ("doom_loop", "*", "ask"),
            ("external_directory", "*", "ask"),
            ("question", "*", "deny"),
            ("read", "*", "allow"),
            ("read", "*.env", "ask"),
            ("question", "*", "allow"),
            ("*", "*", "ask"),
            ("read", "*", "allow"),
            ("glob", "*", "allow"),
            ("grep", "*", "allow"),
            ("list", "*", "allow"),
            ("question", "*", "allow"),
            ("task", "*", "allow"),
            ("skill", "*", "allow"),
            ("edit", "*", "ask"),
            ("bash", "*", "ask"),
            ("webfetch", "*", "ask"),
            ("websearch", "*", "ask"),
        ]
        .iter()
        .map(|(permission, pattern, action)| json!({"permission": permission, "pattern": pattern, "action": action}))
        .collect();
        json!({"name": "build", "permission": rules})
    }

    #[test]
    fn the_read_back_of_a_real_server_proves_the_rules_and_a_default_one_does_not() {
        let agent = real_1_19_5_build_agent();
        assert_eq!(rules_in_force(&agent, AccessMode::Supervised), Ok(()));
        assert_eq!(rules_in_force(&agent, AccessMode::AutoAcceptEdits), Ok(()));
        // The same agent before our config was layered on: everything allowed.
        let mut loose = agent.clone();
        loose["permission"] = json!([{"permission": "*", "pattern": "*", "action": "allow"}]);
        let error = rules_in_force(&loose, AccessMode::Supervised).unwrap_err();
        assert!(error.contains("bash") && error.contains("edit"), "{error}");
        // Full access has nothing to prove.
        assert_eq!(rules_in_force(&loose, AccessMode::FullAccess), Ok(()));
    }

    #[test]
    fn an_edit_that_is_allowed_fails_supervised_and_passes_auto_accept() {
        let mut agent = real_1_19_5_build_agent();
        agent["permission"]
            .as_array_mut()
            .unwrap()
            .push(json!({"permission": "edit", "pattern": "*", "action": "allow"}));
        assert!(rules_in_force(&agent, AccessMode::Supervised).is_err());
        assert!(rules_in_force(&agent, AccessMode::AutoAcceptEdits).is_ok());
    }

    #[test]
    fn a_reply_with_no_rules_is_not_proof() {
        assert!(rules_in_force(&json!({"name": "build"}), AccessMode::Supervised).is_err());
    }

    #[test]
    fn the_v2_read_back_reads_its_own_shape() {
        let rules =
            build(Protocol::V2, AccessMode::Supervised, &[]).unwrap()["permissions"].clone();
        let mut all = vec![rule("*", "*", "allow"), rule("read", "*.env", "ask")];
        all.extend(rules.as_array().unwrap().iter().cloned());
        assert!(
            rules_in_force(
                &json!({"id": "build", "permissions": all}),
                AccessMode::Supervised
            )
            .is_ok()
        );
        assert!(
            rules_in_force(
                &json!({"id": "build", "permissions": [rule("*", "*", "allow")]}),
                AccessMode::AutoAcceptEdits
            )
            .is_err()
        );
    }

    #[test]
    fn an_mcp_server_is_registered_when_the_listing_names_it_in_either_shape() {
        let v1 = json!({"reno": {"status": "connected"}, "other": {"status": "failed"}});
        let v2 = json!({"data": [{"name": "reno", "status": {"status": "disabled"}}]});
        assert!(mcp_registered(&v1, &["reno"]).is_ok());
        assert!(mcp_registered(&v2, &["reno"]).is_ok());
        let error = mcp_registered(&v2, &["reno", "extra"]).unwrap_err();
        assert!(
            error.contains("extra") && !error.contains("reno,"),
            "{error}"
        );
    }
}
