//! Settings: the definitions in force (bots, triggers, policy) with their problems and
//! validation, the policy's rules, and the daemon's status. From `defs.watch`, `def.get`,
//! `def.validate` and `daemon.status`.

use reno_client::{ConnectionStatus, Feed, State};
use reno_proto::defs::{DefError, DefKind, DefSummary};
use serde_json::Value;

use super::triggers::schedule_text;
use super::{Banner, Section, banner};
use crate::summary;

/// The result of `def.validate`, as shown under a definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Validation {
    pub(crate) valid: bool,
    pub(crate) errors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DefRow {
    pub(crate) kind: DefKind,
    pub(crate) name: String,
    pub(crate) version: u32,
    /// One line about it, once its body is read; empty before.
    pub(crate) detail: String,
    /// Problems the last load found in this definition's file. The version in force is the last
    /// one that validated, so a definition can be in force and have problems.
    pub(crate) problems: Vec<String>,
    pub(crate) validation: Option<Validation>,
}

impl DefRow {
    pub(crate) fn has_problems(&self) -> bool {
        !self.problems.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DefsView {
    pub(crate) banner: Option<Banner>,
    pub(crate) bots: Section<DefRow>,
    pub(crate) triggers: Section<DefRow>,
    pub(crate) policy: Vec<DefRow>,
    /// Problems in files that have no version in force (a new file that does not validate).
    pub(crate) orphan_problems: Vec<String>,
    /// Every problem, for the lede.
    pub(crate) problem_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuleRow {
    pub(crate) note: String,
    pub(crate) effect: String,
    /// "command `cargo *`".
    pub(crate) matches: String,
    pub(crate) scope: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PolicyView {
    pub(crate) default: String,
    pub(crate) ttl: Option<String>,
    pub(crate) rules: Vec<RuleRow>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DaemonView {
    pub(crate) connection: ConnectionStatus,
    pub(crate) version: Option<String>,
    pub(crate) protocol: Option<u32>,
    pub(crate) pid: Option<u32>,
    /// "6h 26m", carried forward from the last `daemon.status`.
    pub(crate) uptime: Option<String>,
    pub(crate) socket: Option<String>,
    pub(crate) active: Option<u32>,
    pub(crate) queued: Option<u32>,
    pub(crate) executors: Vec<String>,
    pub(crate) features: Vec<String>,
}

fn problems_of(errors: &[DefError], file: &str) -> Vec<String> {
    errors
        .iter()
        .filter(|e| e.file == file)
        .map(|e| {
            if e.path.is_empty() {
                e.message.clone()
            } else {
                format!("{}: {}", e.path, e.message)
            }
        })
        .collect()
}

fn text<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    body.get(key).and_then(Value::as_str)
}

fn detail(state: &State, def: &DefSummary) -> String {
    let Some(record) = state.def_record(def.kind, &def.name, def.version) else {
        return String::new();
    };
    let body = &record.body;
    match def.kind {
        DefKind::Bot => {
            let executors = body
                .get("executors")
                .and_then(Value::as_array)
                .map(|e| {
                    e.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let access = text(body, "access")
                .and_then(super::runs::access_words)
                .unwrap_or("Supervised");
            let mut parts = vec![executors, access.to_owned()];
            if let Some(team) = text(body, "team") {
                parts.push(format!("team {team}"));
            }
            parts.retain(|p| !p.is_empty());
            parts.join(" · ")
        }
        DefKind::Trigger => {
            let when = body.get("when").map(schedule_text).unwrap_or_default();
            match body.get("target").and_then(|t| text(t, "bot")) {
                Some(bot) => format!("{when} → {bot}"),
                None => when,
            }
        }
        DefKind::Policy => {
            let rules = body
                .get("rules")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            format!(
                "default {} · {rules} rule{}",
                text(body, "default").unwrap_or("ask"),
                if rules == 1 { "" } else { "s" }
            )
        }
    }
}

fn def_row(state: &State, def: &DefSummary) -> DefRow {
    DefRow {
        kind: def.kind,
        name: def.name.clone(),
        version: def.version,
        detail: detail(state, def),
        problems: problems_of(&state.defs().errors, &def.kind.path_for(&def.name)),
        validation: state.validation(def.kind, &def.name).map(|v| Validation {
            valid: v.valid,
            errors: v.errors.iter().map(ToString::to_string).collect(),
        }),
    }
}

/// What the Definitions tab shows.
pub(crate) fn defs(state: &State) -> DefsView {
    let feed = state.feed(Feed::Defs);
    let snapshot = state.defs();
    let of_kind = |kind: DefKind| -> Vec<DefRow> {
        snapshot
            .defs
            .iter()
            .filter(|d| d.kind == kind)
            .map(|d| def_row(state, d))
            .collect()
    };
    let in_force: Vec<String> = snapshot
        .defs
        .iter()
        .map(|d| d.kind.path_for(&d.name))
        .collect();
    let orphan_problems: Vec<String> = snapshot
        .errors
        .iter()
        .filter(|e| !in_force.contains(&e.file))
        .map(ToString::to_string)
        .collect();
    DefsView {
        banner: banner(state),
        bots: Section::of(feed.clone(), of_kind(DefKind::Bot)),
        triggers: Section::of(feed, of_kind(DefKind::Trigger)),
        policy: of_kind(DefKind::Policy),
        orphan_problems,
        problem_count: snapshot.errors.len(),
    }
}

fn rule_row(rule: &Value) -> RuleRow {
    let matcher = rule.get("match").cloned().unwrap_or(Value::Null);
    let mut parts = Vec::new();
    if let Some(action) = text(&matcher, "action") {
        parts.push(action.to_owned());
    }
    for (key, label) in [("tool", "tool"), ("command", "command"), ("path", "path")] {
        if let Some(glob) = text(&matcher, key) {
            parts.push(format!("{label} `{glob}`"));
        }
    }
    let scope = match matcher.get("scope") {
        Some(Value::Array(bots)) => bots
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        _ => "all bots".to_owned(),
    };
    RuleRow {
        note: text(rule, "note").unwrap_or("").to_owned(),
        effect: text(rule, "effect").unwrap_or("ask").to_owned(),
        matches: if parts.is_empty() {
            "anything".to_owned()
        } else {
            parts.join(" ")
        },
        scope,
    }
}

/// The policy's rules, once its body is read.
pub(crate) fn policy(state: &State) -> Option<PolicyView> {
    let def = state
        .defs()
        .defs
        .iter()
        .find(|d| d.kind == DefKind::Policy)?;
    let body = &state.def_record(def.kind, &def.name, def.version)?.body;
    Some(PolicyView {
        default: text(body, "default").unwrap_or("ask").to_owned(),
        ttl: text(body, "approval_ttl").map(str::to_owned),
        rules: body
            .get("rules")
            .and_then(Value::as_array)
            .map(|rules| rules.iter().map(rule_row).collect())
            .unwrap_or_default(),
    })
}

/// What the Daemon tab shows. `now_ms` and `status_seen_ms` carry the last uptime forward.
pub(crate) fn daemon(
    state: &State,
    socket: Option<&std::path::Path>,
    now_ms: u64,
    status_seen_ms: u64,
) -> DaemonView {
    let info = state.daemon();
    let status = info.and_then(|i| i.status.as_ref());
    let uptime = status.map(|s| {
        let carried = now_ms.saturating_sub(status_seen_ms);
        summary::duration_label(s.uptime_secs.saturating_mul(1_000).saturating_add(carried))
    });
    DaemonView {
        connection: state.connection(),
        version: info.map(|i| i.hello.server_version.clone()),
        protocol: info.map(|i| i.hello.protocol_version),
        pid: info.map(|i| i.hello.pid),
        uptime,
        socket: socket.map(|p| p.display().to_string()),
        active: status.map(|s| s.active_runs),
        queued: status.map(|s| s.queued_runs),
        executors: status
            .map(|s| s.executors.iter().map(|e| e.as_str().to_owned()).collect())
            .unwrap_or_default(),
        features: info.map(|i| i.hello.features.clone()).unwrap_or_default(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_client::{DaemonInfo, FeedState, Update};
    use reno_proto::defs::{DefRecord, DefValidateResult, DefsSnapshot};
    use reno_proto::{DaemonStatusInfo, HelloResult};
    use serde_json::json;

    use super::*;

    fn summary(kind: DefKind, name: &str) -> DefSummary {
        DefSummary {
            kind,
            name: name.into(),
            version: 1,
            hash: "0".repeat(64),
            created_at_ms: 1,
        }
    }

    fn record(kind: DefKind, name: &str, body: Value) -> DefRecord {
        DefRecord {
            kind,
            name: name.into(),
            version: 1,
            hash: "0".repeat(64),
            body,
            created_at_ms: 1,
            latest: true,
        }
    }

    fn state_with_defs(errors: Vec<DefError>) -> State {
        let mut state = State::new();
        state.apply(Update::Defs(DefsSnapshot {
            defs: vec![
                summary(DefKind::Bot, "keeper"),
                summary(DefKind::Trigger, "nightly"),
                summary(DefKind::Policy, "policy"),
            ],
            errors,
        }));
        state
    }

    #[test]
    fn loading_empty_and_unavailable() {
        let mut state = State::new();
        assert_eq!(defs(&state).bots, Section::Loading);
        state.apply(Update::Defs(DefsSnapshot::default()));
        let view = defs(&state);
        assert_eq!(view.bots, Section::Empty);
        assert_eq!(view.problem_count, 0);
        state.apply(Update::Feed(Feed::Defs, FeedState::Unavailable));
        assert_eq!(defs(&state).bots, Section::Unavailable);
    }

    #[test]
    fn rows_group_by_kind_and_read_their_bodies() {
        let mut state = state_with_defs(vec![]);
        assert_eq!(defs(&state).bots.rows()[0].detail, "", "body not read yet");
        state.apply(Update::DefRecord(record(
            DefKind::Bot,
            "keeper",
            json!({"executors": ["claude-code"], "access": "auto_accept_edits", "team": "core"}),
        )));
        state.apply(Update::DefRecord(record(
            DefKind::Trigger,
            "nightly",
            json!({"when": {"kind": "every", "every": "6h"}, "target": {"bot": "keeper"}}),
        )));
        state.apply(Update::DefRecord(record(
            DefKind::Policy,
            "policy",
            json!({"default": "ask", "rules": [{"effect": "deny"}]}),
        )));
        let view = defs(&state);
        assert_eq!(
            view.bots.rows()[0].detail,
            "claude-code · Auto-accept edits · team core"
        );
        assert_eq!(view.triggers.rows()[0].detail, "every 6h → keeper");
        assert_eq!(view.policy[0].detail, "default ask · 1 rule");
    }

    #[test]
    fn problems_attach_to_their_file_and_orphans_are_listed() {
        let state = state_with_defs(vec![
            DefError::new("bots/keeper.toml", "executors[0]", "unknown executor"),
            DefError::new("bots/new.toml", "", "not valid TOML"),
        ]);
        let view = defs(&state);
        let keeper = &view.bots.rows()[0];
        assert!(keeper.has_problems());
        assert_eq!(keeper.problems, ["executors[0]: unknown executor"]);
        assert!(!view.triggers.rows()[0].has_problems());
        assert_eq!(view.orphan_problems, ["bots/new.toml: not valid TOML"]);
        assert_eq!(view.problem_count, 2);
    }

    #[test]
    fn a_live_defs_update_clears_the_problems() {
        let mut state = state_with_defs(vec![DefError::new("bots/keeper.toml", "", "bad")]);
        assert!(defs(&state).bots.rows()[0].has_problems());
        state.apply(Update::Defs(DefsSnapshot {
            defs: state.defs().defs.clone(),
            errors: vec![],
        }));
        assert!(!defs(&state).bots.rows()[0].has_problems());
    }

    #[test]
    fn validation_results_show_under_the_definition() {
        let mut state = state_with_defs(vec![]);
        assert!(defs(&state).bots.rows()[0].validation.is_none());
        state.apply(Update::Validation {
            kind: DefKind::Bot,
            name: "keeper".into(),
            result: DefValidateResult {
                valid: false,
                errors: vec![DefError::new("bots/keeper.toml", "name", "must match")],
                hash: None,
            },
        });
        let v = defs(&state).bots.rows()[0].validation.clone().unwrap();
        assert!(!v.valid);
        assert_eq!(v.errors, ["bots/keeper.toml: name: must match"]);
    }

    #[test]
    fn the_policy_lists_its_rules() {
        let mut state = state_with_defs(vec![]);
        assert!(policy(&state).is_none(), "body not read yet");
        state.apply(Update::DefRecord(record(
            DefKind::Policy,
            "policy",
            json!({"default": "ask", "approval_ttl": "30m", "rules": [
                {"note": "no reset", "effect": "deny",
                 "match": {"action": "command", "command": "git reset --hard*"}},
                {"effect": "allow", "match": {"action": "command", "command": "cargo *",
                 "scope": ["builder", "keeper"]}},
                {"effect": "allow"},
            ]}),
        )));
        let view = policy(&state).unwrap();
        assert_eq!(view.ttl.as_deref(), Some("30m"));
        assert_eq!(view.rules.len(), 3);
        assert_eq!(view.rules[0].matches, "command command `git reset --hard*`");
        assert_eq!(view.rules[0].scope, "all bots");
        assert_eq!(view.rules[1].scope, "builder, keeper");
        assert_eq!(view.rules[2].matches, "anything");
    }

    #[test]
    fn the_daemon_view_carries_uptime_forward() {
        let mut state = State::new();
        let view = daemon(&state, None, 10_000, 0);
        assert!(view.version.is_none() && view.uptime.is_none());
        state.apply(Update::Daemon(DaemonInfo {
            hello: HelloResult {
                server_version: "0.1.0".into(),
                protocol_version: 1,
                features: vec!["defs".into()],
                pid: 77,
            },
            status: Some(DaemonStatusInfo {
                version: "0.1.0".into(),
                pid: 77,
                uptime_secs: 3_600,
                active_runs: 2,
                queued_runs: 1,
                executors: vec!["claude-code".parse().unwrap()],
            }),
        }));
        let socket = std::path::Path::new("/tmp/reno.sock");
        let view = daemon(&state, Some(socket), 5_000_000, 5_000_000);
        assert_eq!(view.uptime.as_deref(), Some("1h 00m"));
        assert_eq!(view.socket.as_deref(), Some("/tmp/reno.sock"));
        assert_eq!(
            (view.active, view.queued, view.pid),
            (Some(2), Some(1), Some(77))
        );
        assert_eq!(view.executors, ["claude-code"]);
        // Ten minutes later, without a new status.
        let later = daemon(&state, None, 5_000_000 + 600_000, 5_000_000);
        assert_eq!(later.uptime.as_deref(), Some("1h 10m"));
    }
}
