//! The fake daemon's ACP registry (`executor.registry.*`): a small index with entries in every
//! state the Executors page has to show, and add, remove, own agents and per-agent env that act on
//! the fake's executors.
//!
//! The states, by registry id:
//!
//! | id | state |
//! |---|---|
//! | `goose` | a binary with a published checksum, not added |
//! | `vtcode` | a binary with no checksum published, not added |
//! | `cline` | an npm package, added at 1.0.0 while the index lists 1.1.0 (update available) |
//! | `amp-acp` | an npm package, added and up to date |
//! | `fast-agent` | a Python package; the device lacks uv (`needs_uv`, fix `brew install uv`) |
//! | `gemini` | an npm package; the device lacks Node.js (`needs_node`) |
//! | `windows-only` | no build for this device (`no_build`) |
//! | `claude-acp`, `codex-acp`, `devin` | built in: use `claude-code`, `codex`, `acp.devin` |
//! | `old-agent` | added, no longer in the index (`delisted`) |
//! | `own.mine` | the person's own agent |
//!
//! Adding runs as an install job the same way an install does; a failure queued with
//! `fail_next_job` ends it `failed`, and dismissing it removes the agent again.

use std::collections::BTreeMap;

use reno_proto::{
    AccessMode, AuthMethod, AuthMethodKind, AutoUpdate, ErrorCode, ErrorObject, ExecutorCaps,
    ExecutorDescriptor, ExecutorHealth, ExecutorId, ExecutorInfo, ExecutorRegistryListResult,
    ExecutorSource, InstallStatus, Maintenance, ModelCatalog, NotInstalledReason, RegistryAdded,
    RegistryDistribution, RegistryEntry, RegistryReadiness, RegistrySignInOption, RegistryStatus,
    SandboxLevel, SignedIn, Steering, UpdaterInfo, VersionGate, VersionRange,
};
use tokio_util::sync::CancellationToken;

use super::data::NOW_MS;
use super::jobs::Plan;
use super::world::World;

/// One agent of the fake index.
struct Listed {
    id: &'static str,
    name: &'static str,
    version: &'static str,
    description: &'static str,
    kind: Option<RegistryDistribution>,
    readiness: RegistryReadiness,
    fix: Option<&'static str>,
    host: Option<&'static str>,
    checksum: Option<bool>,
    built_in: Option<&'static str>,
}

const INDEX: &[Listed] = &[
    Listed {
        id: "goose",
        name: "goose",
        version: "1.60.1",
        description: "A local, extensible agent that installs, runs and tests code.",
        kind: Some(RegistryDistribution::Binary),
        readiness: RegistryReadiness::Ready,
        fix: None,
        host: Some("github.com"),
        checksum: Some(true),
        built_in: None,
    },
    Listed {
        id: "vtcode",
        name: "VT Code",
        version: "0.9.0",
        description: "A terminal coding agent with semantic code search.",
        kind: Some(RegistryDistribution::Binary),
        readiness: RegistryReadiness::Ready,
        fix: None,
        host: Some("github.com"),
        checksum: Some(false),
        built_in: None,
    },
    Listed {
        id: "cline",
        name: "Cline",
        version: "1.1.0",
        description: "An autonomous coding agent that edits files and runs commands.",
        kind: Some(RegistryDistribution::Npx),
        readiness: RegistryReadiness::Ready,
        fix: None,
        host: None,
        checksum: None,
        built_in: None,
    },
    Listed {
        id: "amp-acp",
        name: "Amp",
        version: "0.7.0",
        description: "Amp over ACP.",
        kind: Some(RegistryDistribution::Npx),
        readiness: RegistryReadiness::Ready,
        fix: None,
        host: None,
        checksum: None,
        built_in: None,
    },
    Listed {
        id: "fast-agent",
        name: "fast-agent",
        version: "0.10.1",
        description: "Build and run agents and workflows with any model provider.",
        kind: Some(RegistryDistribution::Uvx),
        readiness: RegistryReadiness::NeedsUv,
        fix: Some("brew install uv"),
        host: None,
        checksum: None,
        built_in: None,
    },
    Listed {
        id: "gemini",
        name: "Gemini CLI",
        version: "0.9.0",
        description: "Google's command-line agent.",
        kind: Some(RegistryDistribution::Npx),
        readiness: RegistryReadiness::NeedsNode,
        fix: Some("brew install node"),
        host: None,
        checksum: None,
        built_in: None,
    },
    Listed {
        id: "windows-only",
        name: "Windows Only",
        version: "2.0.0",
        description: "An agent with a build for Windows alone.",
        kind: None,
        readiness: RegistryReadiness::NoBuild,
        fix: None,
        host: None,
        checksum: None,
        built_in: None,
    },
    Listed {
        id: "claude-acp",
        name: "Claude Agent",
        version: "0.5.0",
        description: "Claude over ACP.",
        kind: Some(RegistryDistribution::Npx),
        readiness: RegistryReadiness::Ready,
        fix: None,
        host: None,
        checksum: None,
        built_in: Some("claude-code"),
    },
    Listed {
        id: "codex-acp",
        name: "Codex CLI",
        version: "0.4.0",
        description: "Codex over ACP.",
        kind: Some(RegistryDistribution::Binary),
        readiness: RegistryReadiness::Ready,
        fix: None,
        host: Some("github.com"),
        checksum: Some(true),
        built_in: Some("codex"),
    },
    Listed {
        id: "devin",
        name: "Devin",
        version: "1.2.0",
        description: "Devin for Terminal.",
        kind: Some(RegistryDistribution::Binary),
        readiness: RegistryReadiness::Ready,
        fix: None,
        host: Some("static.devin.ai"),
        checksum: Some(false),
        built_in: Some("acp.devin"),
    },
];

/// Where an `own.<slug>` agent is named from: lower-case letters, digits and dashes.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

fn icon(id: &str) -> String {
    let n = id.bytes().map(u32::from).sum::<u32>() % 8 + 4;
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 16 16\"><circle cx=\"8\" cy=\"8\" r=\"{n}\" fill=\"currentColor\"/></svg>"
    )
}

fn err(code: ErrorCode, message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(code, message)
}

/// What the fake keeps of the registry.
#[derive(Default)]
pub(super) struct RegistryState {
    /// Each agent's own environment, as `KEY=VALUE` lines.
    env: BTreeMap<String, String>,
    /// Serve the cached copy as if the index could not be fetched.
    pub(super) offline: bool,
}

/// An added agent as an executor.
fn added_executor(
    id: &str,
    name: &str,
    version: Option<&str>,
    latest: Option<&str>,
    own: bool,
) -> Result<ExecutorInfo, reno_proto::IdError> {
    let installed = version.is_some();
    Ok(ExecutorInfo {
        descriptor: ExecutorDescriptor {
            id: id.parse()?,
            contract_version: 1,
            adapter_version: format!("reno-harness demo, {name}"),
            cli_verified: VersionRange {
                min: "0".to_owned(),
                verified: "unverified".to_owned(),
            },
            caps: ExecutorCaps {
                live_effort: None,
                live_model_options: None,
                steering: Steering::StepBoundary,
                resume: true,
                access_modes: vec![AccessMode::FullAccess],
                sandbox_levels: vec![SandboxLevel::Unrestricted],
                mcp_injection: true,
                structured_output: false,
                deterministic_turn_end: true,
                plan_mode: false,
                models: vec![],
                model_switch: false,
                isolates_user_config: false,
                model_aliases: vec![],
            },
        },
        health: Some(ExecutorHealth {
            installed: match version {
                Some(version) => InstallStatus::Installed {
                    path: format!("/fake/agents/{id}/bin").into(),
                    version: Some(version.to_owned()),
                },
                None => InstallStatus::NotInstalled {
                    why: NotInstalledReason::NotFound {
                        hint: format!("{name} is not added"),
                    },
                },
            },
            version_gate: VersionGate::Unchecked,
            // A registry agent is not started to find out: not checked yet.
            signed_in: SignedIn::Unknown,
            models: ModelCatalog::curated(vec![]),
            checked_at_ms: NOW_MS,
            plan_usage: None,
        }),
        maintenance: Some(Maintenance {
            setup: None,
            sign_in: None,
            updater: (installed && !own).then(|| UpdaterInfo {
                command: format!(
                    "reno executor add {}",
                    id.strip_prefix("acp.").unwrap_or(id)
                ),
                self_updating: false,
                can_pin: false,
                latest: latest.map(str::to_owned),
                checked_at_ms: Some(NOW_MS),
            }),
            managed: !own,
            auto_update: AutoUpdate {
                on: true,
                verified_only: false,
            },
        }),
        job: None,
        source: ExecutorSource::Registry,
        auth_methods: methods_of(id),
    })
}

/// What the agents the fake starts with offered in their last full probe's handshake (see
/// `signin`).
fn methods_of(id: &str) -> Vec<AuthMethod> {
    let method = |id: &str, name: &str, kind: AuthMethodKind| AuthMethod {
        id: id.to_owned(),
        name: name.to_owned(),
        description: None,
        kind,
        vars: Vec::new(),
        link: None,
        command: None,
        unusable: None,
    };
    let key = |id: &str, var: &str| AuthMethod {
        vars: vec![var.to_owned()],
        link: Some("https://example.com/keys".to_owned()),
        ..method(id, "API key", AuthMethodKind::EnvVar)
    };
    match id {
        "acp.cline" => vec![
            method("browser", "Sign in in the browser", AuthMethodKind::Agent),
            key("key", "CLINE_API_KEY"),
        ],
        "acp.amp-acp" => vec![key("key", "AMP_API_KEY")],
        "acp.old-agent" => vec![AuthMethod {
            command: Some("/fake/agents/old-agent/bin login".to_owned()),
            ..method("login", "Log in", AuthMethodKind::Terminal)
        }],
        _ => Vec::new(),
    }
}

/// The agents the fake starts with that were added from the registry (or are the person's own).
pub(super) fn seed() -> Result<Vec<ExecutorInfo>, reno_proto::IdError> {
    Ok(vec![
        added_executor("acp.cline", "Cline", Some("1.0.0"), Some("1.1.0"), false)?,
        added_executor("acp.amp-acp", "Amp", Some("0.7.0"), Some("0.7.0"), false)?,
        added_executor("acp.old-agent", "Old Agent", Some("0.3.0"), None, false)?,
        added_executor("acp.own.mine", "mine", Some("0.0.0"), None, true)?,
    ])
}

fn index_entry(listed: &Listed) -> Result<RegistryEntry, reno_proto::IdError> {
    Ok(RegistryEntry {
        id: listed.id.to_owned(),
        executor: format!("acp.{}", listed.id).parse()?,
        name: listed.name.to_owned(),
        version: listed.version.to_owned(),
        description: listed.description.to_owned(),
        authors: vec!["The Agents Project".to_owned()],
        license: Some("Apache-2.0".to_owned()),
        website: Some(format!("https://example.com/{}", listed.id)),
        repository: Some(format!("https://github.com/example/{}", listed.id)),
        icon: Some(icon(listed.id)),
        kind: listed.kind,
        readiness: listed.readiness,
        fix: listed.fix.map(str::to_owned),
        host: listed.host.map(str::to_owned),
        checksum: listed.checksum,
        built_in: listed.built_in.map(str::parse).transpose()?,
        added: None,
        adding: false,
        delisted: false,
        own: false,
        sign_in: curated(listed.id, listed.version).0,
        sign_in_from_probe: false,
        status: curated(listed.id, listed.version).1,
        checked_version: curated(listed.id, listed.version).2,
    })
}

/// What the fake's audit table says of an agent at the version the index lists: how it signs in,
/// whether it works, and the version the facts were checked at when that is another. The kinds
/// the real table has, on a few agents:
///
/// | id | says |
/// |---|---|
/// | `goose` | a terminal command, then keys; `ok` |
/// | `vtcode` | set up by hand; `broken_upstream` |
/// | `cline` | browser methods, checked at 3.0.74 (the index lists 1.1.0), so no status |
/// | `amp-acp` | a terminal command; `needs_reno_fix` |
/// | `gemini` | browser, then a key; `ok` |
/// | `fast-agent` | set up by hand; `ok` |
/// | `windows-only` | needs no sign-in; `ok` |
type Curated = (
    Vec<RegistrySignInOption>,
    Option<RegistryStatus>,
    Option<String>,
);

fn curated(id: &str, listed: &str) -> Curated {
    let env = |vars: &[&str]| RegistrySignInOption::Env {
        vars: vars.iter().map(|v| (*v).to_owned()).collect(),
    };
    let terminal = |command: &str| RegistrySignInOption::Terminal {
        command: command.to_owned(),
    };
    let browser = |methods: &[&str]| RegistrySignInOption::Browser {
        methods: methods.iter().map(|m| (*m).to_owned()).collect(),
    };
    let setup = |text: &str| RegistrySignInOption::Setup {
        text: text.to_owned(),
    };
    let (ways, status, audited) = match id {
        "goose" => (
            vec![
                terminal("goose configure"),
                env(&["GOOSE_PROVIDER", "GOOSE_MODEL"]),
            ],
            RegistryStatus::Ok,
            "1.60.1",
        ),
        "vtcode" => (
            vec![setup("provider keys come from the environment")],
            RegistryStatus::BrokenUpstream {
                reason: "exits before initialize (the demo's example of a broken agent)".to_owned(),
            },
            "0.9.0",
        ),
        "cline" => (
            vec![browser(&["cline", "cline-pass", "openai-codex"])],
            RegistryStatus::Ok,
            "3.0.74",
        ),
        "amp-acp" => (
            vec![terminal("amp-acp --setup")],
            RegistryStatus::NeedsRenoFix {
                reason: "needs a start Reno does not do yet (the demo's example)".to_owned(),
            },
            "0.7.0",
        ),
        "gemini" => (
            vec![browser(&["oauth-personal"]), env(&["GEMINI_API_KEY"])],
            RegistryStatus::Ok,
            "0.9.0",
        ),
        "fast-agent" => (
            vec![setup(
                "keys in fast-agent.secrets.yaml, or the provider's environment variables",
            )],
            RegistryStatus::Ok,
            "0.10.1",
        ),
        "windows-only" => (
            vec![RegistrySignInOption::None],
            RegistryStatus::Ok,
            "2.0.0",
        ),
        _ => return (Vec::new(), None, None),
    };
    let same = audited == listed;
    (
        ways,
        same.then_some(status),
        (!same).then(|| audited.to_owned()),
    )
}

/// The ways the agent's own handshake offered (as the daemon's list shows them).
fn probed(methods: &[reno_proto::AuthMethod]) -> Vec<RegistrySignInOption> {
    use reno_proto::AuthMethodKind;
    let mut ways = Vec::new();
    let browser: Vec<String> = methods
        .iter()
        .filter(|m| m.kind == AuthMethodKind::Agent)
        .map(|m| m.id.clone())
        .collect();
    if !browser.is_empty() {
        ways.push(RegistrySignInOption::Browser { methods: browser });
    }
    for m in methods {
        match m.kind {
            AuthMethodKind::Terminal => ways.push(RegistrySignInOption::Terminal {
                command: m.command.clone().unwrap_or_else(|| m.name.clone()),
            }),
            AuthMethodKind::EnvVar => ways.push(RegistrySignInOption::Env {
                vars: m.vars.clone(),
            }),
            AuthMethodKind::Agent => {}
        }
    }
    ways
}

impl World {
    fn registry_executor(&self, registry_id: &str) -> Option<usize> {
        let wanted = format!("acp.{registry_id}");
        self.executors.iter().position(|e| {
            e.descriptor.id.as_str() == wanted && e.source == ExecutorSource::Registry
        })
    }

    /// The version the fake index lists for the executor `id` (an add installs this version).
    pub(super) fn index_version(&self, id: &ExecutorId) -> Option<String> {
        let rid = id.as_str().strip_prefix("acp.")?;
        // (A built-in executor with a registry id, `acp.devin`, is not installed from the index.)
        self.executors
            .iter()
            .find(|e| &e.descriptor.id == id && e.source == ExecutorSource::Registry)?;
        INDEX
            .iter()
            .find(|l| l.id == rid)
            .map(|l| l.version.to_owned())
    }

    /// `executor.registry.list`.
    pub(super) fn registry_list(&self) -> Result<ExecutorRegistryListResult, ErrorObject> {
        let bad = |e: reno_proto::IdError| err(ErrorCode::Internal, e.to_string());
        let mut entries = Vec::new();
        for listed in INDEX {
            let mut entry = index_entry(listed).map_err(bad)?;
            if let Some(at) = self.registry_executor(listed.id) {
                let info = &self.executors[at];
                entry.added = info
                    .health
                    .as_ref()
                    .and_then(|h| h.installed.version())
                    .map(|v| RegistryAdded {
                        version: v.to_owned(),
                        update_available: v != listed.version,
                    });
                entry.adding = info
                    .job
                    .as_ref()
                    .is_some_and(|j| !matches!(j.phase, reno_proto::JobPhase::Failed { .. }));
                // An added agent that has had a full probe says how it signs in itself, and that
                // wins over the curated facts.
                let ways = probed(&info.auth_methods);
                if !ways.is_empty() {
                    entry.sign_in = ways;
                    entry.sign_in_from_probe = true;
                    entry.checked_version = None;
                }
            }
            entries.push(entry);
        }
        entries.sort_by_key(|e| e.name.to_ascii_lowercase());
        // Added here, not in the index: delisted, or the person's own.
        for info in &self.executors {
            let Some(rid) = info.descriptor.id.as_str().strip_prefix("acp.") else {
                continue;
            };
            if info.source != ExecutorSource::Registry || INDEX.iter().any(|l| l.id == rid) {
                continue;
            }
            let version = info
                .health
                .as_ref()
                .and_then(|h| h.installed.version())
                .unwrap_or("0.0.0")
                .to_owned();
            let own = rid.starts_with("own.");
            entries.push(RegistryEntry {
                id: rid.to_owned(),
                executor: info.descriptor.id.clone(),
                name: info
                    .descriptor
                    .adapter_version
                    .rsplit(", ")
                    .next()
                    .unwrap_or(rid)
                    .to_owned(),
                version: version.clone(),
                description: String::new(),
                authors: vec![],
                license: None,
                website: None,
                repository: None,
                icon: None,
                kind: None,
                readiness: RegistryReadiness::Ready,
                fix: None,
                host: None,
                checksum: None,
                built_in: None,
                added: Some(RegistryAdded {
                    version,
                    update_available: false,
                }),
                adding: info
                    .job
                    .as_ref()
                    .is_some_and(|j| !matches!(j.phase, reno_proto::JobPhase::Failed { .. })),
                delisted: !own,
                own,
                sign_in: Vec::new(),
                sign_in_from_probe: false,
                status: None,
                checked_version: None,
            });
        }
        Ok(ExecutorRegistryListResult {
            entries,
            fetched_at_ms: NOW_MS,
            offline: self.registry.offline,
            error: self
                .registry
                .offline
                .then(|| "could not reach cdn.agentclientprotocol.com".to_owned()),
        })
    }

    /// `executor.registry.add`: the executor is registered at once (not installed) and its
    /// install runs as a job. An agent already added at an older version updates.
    pub(super) fn registry_add(
        &mut self,
        id: &str,
        root: &CancellationToken,
    ) -> Result<(ExecutorId, Option<Plan>), ErrorObject> {
        let listed = INDEX.iter().find(|l| l.id == id).ok_or_else(|| {
            err(
                ErrorCode::NotFound,
                format!("no agent {id:?} in the registry"),
            )
        })?;
        let executor: ExecutorId = format!("acp.{id}")
            .parse()
            .map_err(|e: reno_proto::IdError| err(ErrorCode::Internal, e.to_string()))?;
        if let Some(built_in) = listed.built_in {
            return Err(err(
                ErrorCode::NothingToDo,
                format!("{} is built in: use {built_in}", listed.name),
            ));
        }
        let kind = match listed.readiness {
            RegistryReadiness::Ready => reno_proto::JobKind::Install,
            RegistryReadiness::NeedsNode => {
                return Err(err(
                    ErrorCode::Conflict,
                    format!(
                        "{} needs Node.js: {}",
                        listed.name,
                        listed.fix.unwrap_or("install Node.js")
                    ),
                ));
            }
            RegistryReadiness::NeedsUv => {
                return Err(err(
                    ErrorCode::Conflict,
                    format!(
                        "{} needs uv: {}",
                        listed.name,
                        listed.fix.unwrap_or("install uv")
                    ),
                ));
            }
            RegistryReadiness::NoBuild => {
                return Err(err(
                    ErrorCode::Conflict,
                    format!("{} has no build for this Mac", listed.name),
                ));
            }
        };
        let kind = match self.registry_executor(id) {
            Some(at) => {
                let installed = self.executors[at]
                    .health
                    .as_ref()
                    .and_then(|h| h.installed.version().map(str::to_owned));
                match installed {
                    Some(v) if v == listed.version => {
                        return Err(err(
                            ErrorCode::NothingToDo,
                            format!("{} is already added at {v}", listed.name),
                        ));
                    }
                    Some(_) => {
                        if let Some(u) = self.executors[at]
                            .maintenance
                            .as_mut()
                            .and_then(|m| m.updater.as_mut())
                        {
                            u.latest = Some(listed.version.to_owned());
                        }
                        reno_proto::JobKind::Update
                    }
                    None => reno_proto::JobKind::Install,
                }
            }
            None => {
                let info = added_executor(executor.as_str(), listed.name, None, None, false)
                    .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                self.executors.push(info);
                kind
            }
        };
        let plan = self.start_job(&executor, kind, false, root)?;
        Ok((executor, plan))
    }

    /// `executor.registry.remove`.
    pub(super) fn registry_remove(&mut self, id: &str) -> Result<ExecutorId, ErrorObject> {
        let at = self
            .registry_executor(id)
            .ok_or_else(|| err(ErrorCode::NotFound, format!("{id} is not added")))?;
        let executor = self.executors[at].descriptor.id.clone();
        let running = self.executors[at]
            .job
            .as_ref()
            .is_some_and(|j| !matches!(j.phase, reno_proto::JobPhase::Failed { .. }));
        if running || self.jobs.active_runs.get(&executor).copied().unwrap_or(0) > 0 {
            return Err(err(
                ErrorCode::Conflict,
                format!("{executor} has a run or an install in progress"),
            ));
        }
        self.executors.remove(at);
        self.registry.env.remove(id);
        Ok(executor)
    }

    /// `executor.registry.add_own`.
    pub(super) fn registry_add_own(
        &mut self,
        name: &str,
        command: &str,
        env: &str,
    ) -> Result<ExecutorId, ErrorObject> {
        let slug = slug(name);
        if slug.is_empty() || command.trim().is_empty() {
            return Err(err(
                ErrorCode::InvalidParams,
                "name the agent and its command",
            ));
        }
        let rid = format!("own.{slug}");
        if self.registry_executor(&rid).is_some() {
            return Err(err(
                ErrorCode::NothingToDo,
                format!("an agent named {name} is already added"),
            ));
        }
        let info = added_executor(
            &format!("acp.{rid}"),
            name.trim(),
            Some("0.0.0"),
            None,
            true,
        )
        .map_err(|e| err(ErrorCode::InvalidParams, e.to_string()))?;
        let executor = info.descriptor.id.clone();
        self.executors.push(info);
        self.registry.env.insert(rid, env.to_owned());
        Ok(executor)
    }

    /// `executor.registry.env_get`.
    pub(super) fn registry_env_get(&self, id: &str) -> Result<String, ErrorObject> {
        self.registry_executor(id)
            .ok_or_else(|| err(ErrorCode::NotFound, format!("{id} is not added")))?;
        Ok(self.registry.env.get(id).cloned().unwrap_or_default())
    }

    /// `executor.registry.env_set`: lines that are not `KEY=VALUE` are refused, as the daemon does.
    pub(super) fn registry_env_set(&mut self, id: &str, env: &str) -> Result<String, ErrorObject> {
        self.registry_executor(id)
            .ok_or_else(|| err(ErrorCode::NotFound, format!("{id} is not added")))?;
        for line in env
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let line = line.strip_prefix("export ").unwrap_or(line);
            if !line
                .split_once('=')
                .is_some_and(|(k, _)| !k.trim().is_empty())
            {
                // (Not the line itself: it may hold a key.)
                return Err(err(ErrorCode::InvalidParams, "a line is not KEY=VALUE"));
            }
        }
        self.registry.env.insert(id.to_owned(), env.to_owned());
        Ok(env.to_owned())
    }
}
