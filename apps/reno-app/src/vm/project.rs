//! The Project page (ui-brief 2026-11-17): what may run without asking in one project, and what
//! was decided there. The Allowlist lists this project's rules, then the ones every project
//! shares (`~/.reno/policy.toml` and answers kept for every run); History is the decided list
//! the Inbox used to show, filtered to the project's runs.

use std::path::{Path, PathBuf};

use reno_client::{Feed, State};
use reno_proto::{ApprovalRecord, StandingRule, StandingScope};

use super::Section;
use super::history::{self, HistoryRow};
use super::projects::FolderKey;
use super::rules;
use super::settings;

/// One rule on the Allowlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AllowRow {
    /// "allow", "ask", "deny".
    pub(crate) effect: String,
    /// What it matches: "command `cargo test --workspace`".
    pub(crate) matches: String,
    /// Where it came from: "policy.toml", "kept from a run".
    pub(crate) from: String,
}

/// One rule kept for this project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HereRow {
    pub(crate) id: reno_proto::RuleId,
    /// "allow" or "deny".
    pub(crate) effect: String,
    pub(crate) matches: String,
    /// "kept from a run" or "added by you".
    pub(crate) from: String,
}

/// What the Project page shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectView {
    pub(crate) name: String,
    /// The repository root, in full; `None` for "No project".
    pub(crate) path: Option<String>,
    /// Whether the daemon keeps rules for one project (`rules.project`).
    pub(crate) project_rules: bool,
    /// The rules kept for this project, newest first.
    pub(crate) here: Vec<HereRow>,
    /// Rules every project shares, policy file first.
    pub(crate) everywhere: Vec<AllowRow>,
    /// The policy's answer when no rule matches: "ask".
    pub(crate) default: String,
    pub(crate) history: Section<HistoryRow>,
}

/// Whether `record`'s run is in the project `key`.
fn in_project(
    state: &State,
    record: &ApprovalRecord,
    key: &FolderKey,
    root_of: &impl Fn(&Path) -> Option<PathBuf>,
) -> bool {
    let Some(run) = state.run(&record.run_id) else {
        return false;
    };
    let here = run
        .cwd
        .as_deref()
        .and_then(root_of)
        .map_or(FolderKey::NoProject, FolderKey::Repo);
    &here == key
}

/// The page for the project `key`.
pub(crate) fn build(
    state: &State,
    key: &FolderKey,
    root_of: impl Fn(&Path) -> Option<PathBuf>,
    standing: Option<&Result<Vec<StandingRule>, String>>,
    now_ms: u64,
) -> ProjectView {
    let policy = settings::policy(state);
    let mut everywhere: Vec<AllowRow> = policy
        .as_ref()
        .map(|p| {
            p.rules
                .iter()
                .map(|r| AllowRow {
                    effect: r.effect.clone(),
                    matches: r.matches.clone(),
                    from: "policy.toml".to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    if let Some(Ok(rules)) = standing {
        everywhere.extend(
            rules
                .iter()
                .filter(|r| r.is_active(now_ms) && r.scope == StandingScope::Always)
                .map(|r| AllowRow {
                    effect: match r.effect {
                        reno_proto::StandingEffect::Allow => "allow",
                        reno_proto::StandingEffect::Deny => "deny",
                    }
                    .to_owned(),
                    matches: rules::matcher_text(&r.matcher),
                    from: "kept from a run".to_owned(),
                }),
        );
    }
    let here: Vec<HereRow> = match (standing, key) {
        (Some(Ok(rules)), FolderKey::Repo(root)) => rules
            .iter()
            .filter(|r| r.is_active(now_ms))
            .filter(|r| matches!(&r.scope, StandingScope::Project { root: at } if at == root))
            .map(|r| HereRow {
                id: r.id.clone(),
                effect: match r.effect {
                    reno_proto::StandingEffect::Allow => "allow",
                    reno_proto::StandingEffect::Deny => "deny",
                }
                .to_owned(),
                matches: rules::matcher_text(&r.matcher),
                from: if r.source_approval.is_some() {
                    "kept from a run"
                } else {
                    "added by you"
                }
                .to_owned(),
            })
            .collect(),
        _ => Vec::new(),
    };
    let history: Vec<HistoryRow> = state
        .approval_history()
        .iter()
        .filter(|r| in_project(state, r, key, &root_of))
        .filter_map(|r| history::row(state, r, now_ms))
        .collect();
    ProjectView {
        name: key.name(),
        path: match key {
            FolderKey::Repo(root) => Some(root.display().to_string()),
            FolderKey::NoProject => None,
        },
        project_rules: state.supports(reno_proto::FEATURE_PROJECT_RULES),
        here,
        everywhere,
        default: policy.map_or_else(|| "ask".to_owned(), |p| p.default),
        history: Section::of_state(state, state.feed(Feed::ApprovalHistory), history),
    }
}
