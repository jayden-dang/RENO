//! The Executors list: one row per executor with what the daemon's last probe found, leading with
//! the ones that need the human (signed out, too old, needs an install). From `executors.watch`.
//!
//! Every fact the probe could not establish stays unknown on screen: "Sign-in not checkable" is
//! never shown as signed in, an untested version is never shown as fine.

use reno_client::{Feed, State};
use reno_proto::{
    AccessMode, ExecutorHealth, ExecutorInfo, InstallStatus, ModelSource, NotInstalledReason,
    RunId, RunStatus, SandboxLevel, SignedIn, StartRefusal, StopDetail, VersionGate,
};

use super::{Banner, Section, banner};
use crate::summary;

/// How a fact reads: the colour of its dot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Good,
    /// Works, with a caveat.
    Warn,
    /// Blocks a run.
    Bad,
    /// Not known, or not applicable.
    Quiet,
}

/// One line of an executor's health: what it is and the detail under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fact {
    pub(crate) level: Level,
    pub(crate) text: &'static str,
    pub(crate) detail: Option<String>,
}

impl Fact {
    fn new(level: Level, text: &'static str, detail: impl Into<Option<String>>) -> Self {
        Self {
            level,
            text,
            detail: detail.into(),
        }
    }
}

/// What the human has to do, most pressing first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Need {
    SignIn,
    Update,
    /// The daemon can fetch it: the Install button.
    Install,
    /// Not there and not the daemon's to fetch (not on the path, or unusable).
    Setup,
}

impl Need {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::SignIn => "signed out",
            Self::Update => "too old",
            Self::Install => "needs install",
            Self::Setup => "not installed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecutorRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) need: Option<Need>,
    /// `None` until the first probe has finished.
    pub(crate) installed: Fact,
    /// `None` when there is nothing installed to have a version.
    pub(crate) version: Option<Fact>,
    /// `None` when there is nothing installed to sign in to.
    pub(crate) signed_in: Option<Fact>,
    pub(crate) access: Vec<&'static str>,
    pub(crate) sandbox: Vec<&'static str>,
    /// "2 models, from the CLI" / "1 model, built-in list" / "no models listed".
    pub(crate) models: String,
    /// "checked 3m ago" / "not checked yet".
    pub(crate) checked: String,
    /// The Install button: the probe says `needs_install`.
    pub(crate) can_install: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecutorsView {
    pub(crate) banner: Option<Banner>,
    pub(crate) rows: Section<ExecutorRow>,
    pub(crate) lede: String,
    pub(crate) detail: String,
    /// How many need the human.
    pub(crate) needing: usize,
    /// Whether the daemon has `executor.install`.
    pub(crate) installs: bool,
}

pub(crate) const fn access_label(mode: AccessMode) -> &'static str {
    match mode {
        AccessMode::Supervised => "Supervised",
        AccessMode::AutoAcceptEdits => "Auto-accept edits",
        AccessMode::FullAccess => "Full access",
    }
}

pub(crate) const fn sandbox_label(level: SandboxLevel) -> &'static str {
    match level {
        SandboxLevel::ReadOnly => "Read-only",
        SandboxLevel::WorkspaceWrite => "Workspace write",
        SandboxLevel::Unrestricted => "Unrestricted",
    }
}

fn installed_fact(health: Option<&ExecutorHealth>) -> Fact {
    let Some(health) = health else {
        return Fact::new(Level::Quiet, "Not checked yet", None);
    };
    match &health.installed {
        InstallStatus::Installed { path, version } => Fact::new(
            Level::Good,
            "Installed",
            Some(match version {
                Some(version) => format!("{} \u{b7} {version}", path.display()),
                None => format!("{} \u{b7} version unknown", path.display()),
            }),
        ),
        InstallStatus::NotInstalled { why } => match why {
            NotInstalledReason::NotFound { hint } => {
                Fact::new(Level::Bad, "Not installed", Some(hint.clone()))
            }
            NotInstalledReason::NeedsInstall { what } => Fact::new(
                Level::Bad,
                "Needs install",
                Some(format!("{what} is fetched by Install, never by a run")),
            ),
            NotInstalledReason::Unusable { detail } => {
                Fact::new(Level::Bad, "Cannot be launched", Some(detail.clone()))
            }
            NotInstalledReason::ProbeFailed { detail } => {
                Fact::new(Level::Quiet, "Could not be checked", Some(detail.clone()))
            }
        },
    }
}

fn version_fact(health: &ExecutorHealth) -> Option<Fact> {
    let InstallStatus::Installed { .. } = &health.installed else {
        return None;
    };
    Some(match &health.version_gate {
        VersionGate::Ok => Fact::new(Level::Good, "Version supported", None),
        VersionGate::TooOld { min } => Fact::new(
            Level::Bad,
            "Version too old",
            Some(format!("needs {min} or newer")),
        ),
        VersionGate::Untested { verified } => Fact::new(
            Level::Warn,
            "Version untested",
            Some(if verified == "unverified" {
                "no version of this agent is verified yet".to_owned()
            } else {
                format!("verified up to {verified}")
            }),
        ),
        VersionGate::Unchecked => Fact::new(
            Level::Quiet,
            "Version not checked",
            Some("the CLI did not report one".to_owned()),
        ),
    })
}

fn signed_in_fact(health: &ExecutorHealth) -> Option<Fact> {
    let InstallStatus::Installed { .. } = &health.installed else {
        return None;
    };
    Some(match &health.signed_in {
        SignedIn::Yes { account } => Fact::new(Level::Good, "Signed in", account.clone()),
        SignedIn::No { hint } => Fact::new(Level::Bad, "Signed out", Some(hint.clone())),
        SignedIn::Unknown => Fact::new(
            Level::Quiet,
            "Sign-in not checkable",
            Some("only a run would find out".to_owned()),
        ),
    })
}

/// What the human has to do for this executor, if anything.
fn need_of(health: Option<&ExecutorHealth>) -> Option<Need> {
    let health = health?;
    match &health.installed {
        InstallStatus::NotInstalled {
            why: NotInstalledReason::NeedsInstall { .. },
        } => Some(Need::Install),
        InstallStatus::NotInstalled {
            why: NotInstalledReason::NotFound { .. } | NotInstalledReason::Unusable { .. },
        } => Some(Need::Setup),
        // A probe that could not finish says nothing about the harness.
        InstallStatus::NotInstalled {
            why: NotInstalledReason::ProbeFailed { .. },
        } => None,
        InstallStatus::Installed { .. } => {
            if matches!(health.signed_in, SignedIn::No { .. }) {
                Some(Need::SignIn)
            } else if matches!(health.version_gate, VersionGate::TooOld { .. }) {
                Some(Need::Update)
            } else {
                None
            }
        }
    }
}

fn models_text(info: &ExecutorInfo) -> String {
    let count = info.descriptor.caps.models.len();
    if count == 0 {
        return "no models listed".to_owned();
    }
    let noun = if count == 1 { "model" } else { "models" };
    match info.health.as_ref().map(|h| h.models.source) {
        Some(ModelSource::Live) => format!("{count} {noun}, from the CLI"),
        Some(ModelSource::Curated) | None => format!("{count} {noun}, built-in list"),
    }
}

fn row(info: &ExecutorInfo, installs: bool, now_ms: u64) -> ExecutorRow {
    let health = info.health.as_ref();
    let caps = &info.descriptor.caps;
    let id = info.descriptor.id.as_str();
    ExecutorRow {
        id: id.to_owned(),
        name: summary::executor_name(id).to_owned(),
        need: need_of(health),
        installed: installed_fact(health),
        version: health.and_then(version_fact),
        signed_in: health.and_then(signed_in_fact),
        access: caps.access_modes.iter().map(|m| access_label(*m)).collect(),
        sandbox: caps
            .sandbox_levels
            .iter()
            .map(|l| sandbox_label(*l))
            .collect(),
        models: models_text(info),
        checked: health.map_or_else(
            || "not checked yet".to_owned(),
            |h| {
                format!(
                    "checked {}",
                    summary::ago_label(now_ms.saturating_sub(h.checked_at_ms))
                )
            },
        ),
        can_install: installs && need_of(health) == Some(Need::Install),
    }
}

/// What the Executors screen shows.
pub(crate) fn build(state: &State, now_ms: u64) -> ExecutorsView {
    let installs = state.supports(reno_proto::FEATURE_EXECUTOR_INSTALL);
    let mut rows: Vec<ExecutorRow> = state
        .executors()
        .iter()
        .map(|info| row(info, installs, now_ms))
        .collect();
    // Who needs the human first, in the order of how pressing it is; the rest by name.
    rows.sort_by(|a, b| {
        let rank = |r: &ExecutorRow| r.need.map_or((1, None), |n| (0, Some(n)));
        rank(a).cmp(&rank(b)).then_with(|| a.id.cmp(&b.id))
    });
    let needing = rows.iter().filter(|r| r.need.is_some()).count();
    let (lede, detail) = match (needing, rows.len()) {
        (0, 0) => (
            "No executors".to_owned(),
            "Reno drives agent CLIs; the daemon lists the ones it can drive here.".to_owned(),
        ),
        (0, _) => (
            "Every executor is ready".to_owned(),
            "Reno does not run models, it drives harnesses. A run goes only to one that is installed, supported and signed in."
                .to_owned(),
        ),
        (1, _) => (
            "1 executor needs you".to_owned(),
            "Runs are not sent to an executor that is missing, too old or signed out. Fix it, then check again."
                .to_owned(),
        ),
        (n, _) => (
            format!("{n} executors need you"),
            "Runs are not sent to an executor that is missing, too old or signed out. Fix them, then check again."
                .to_owned(),
        ),
    };
    ExecutorsView {
        banner: banner(state),
        rows: Section::of_state(state, state.feed(Feed::Executors), rows),
        lede,
        detail,
        needing,
        installs,
    }
}

/// Why a failed run's executor is worth a look, for the Run screen: the run failed because its
/// executor could not be used, and what the daemon knows about that executor now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FailedExecutor {
    pub(crate) row: ExecutorRow,
}

/// Whether the run's typed stop reason says its executor could not take it: not installed, not
/// signed in, or not eligible (an access mode or sandbox it cannot honour, a health gate). The
/// daemon says so in `RunSummary::stop`; the diagnostic's text is never read.
pub(crate) fn is_executor_refusal(reason: &StopDetail) -> bool {
    matches!(
        reason,
        StopDetail::StartRefused {
            failure: StartRefusal::NotInstalled
                | StartRefusal::NotSignedIn
                | StartRefusal::NoEligibleExecutor,
            ..
        }
    )
}

/// The health of `run`'s executor to show inline when the run failed for an executor reason.
/// `None` for any other run, and when the daemon has not listed that executor.
pub(crate) fn failed_executor(state: &State, run: &RunId, now_ms: u64) -> Option<FailedExecutor> {
    let summary = state.run(run)?;
    if summary.status != RunStatus::Failed
        || !summary.stop.as_ref().is_some_and(is_executor_refusal)
    {
        return None;
    }
    let info = state
        .executors()
        .iter()
        .find(|e| e.descriptor.id == summary.executor)?;
    let installs = state.supports(reno_proto::FEATURE_EXECUTOR_INSTALL);
    Some(FailedExecutor {
        row: row(info, installs, now_ms),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_client::{FeedState, Update};
    use reno_proto::{
        ExecutorCaps, ExecutorDescriptor, ModelCatalog, ModelInfo, Steering, VersionRange,
    };

    use super::*;

    const NOW: u64 = 1_000 * 3_600_000;
    const MIN: u64 = 60_000;

    fn healthy() -> ExecutorHealth {
        ExecutorHealth {
            installed: InstallStatus::Installed {
                path: "/usr/local/bin/claude".into(),
                version: Some("2.1.325".into()),
            },
            version_gate: VersionGate::Ok,
            signed_in: SignedIn::Yes {
                account: Some("ada@example.com".into()),
            },
            models: ModelCatalog::curated(vec![]),
            checked_at_ms: NOW - 3 * MIN,
        }
    }

    fn info(id: &str, health: Option<ExecutorHealth>) -> ExecutorInfo {
        let models: Vec<ModelInfo> = health
            .as_ref()
            .map(|h| h.models.models.clone())
            .unwrap_or_default();
        ExecutorInfo {
            descriptor: ExecutorDescriptor {
                id: id.parse().unwrap(),
                contract_version: 1,
                adapter_version: "t".into(),
                cli_verified: VersionRange {
                    min: "1".into(),
                    verified: "2".into(),
                },
                caps: ExecutorCaps {
                    steering: Steering::None,
                    resume: false,
                    access_modes: vec![AccessMode::Supervised, AccessMode::AutoAcceptEdits],
                    sandbox_levels: vec![SandboxLevel::ReadOnly, SandboxLevel::Unrestricted],
                    mcp_injection: false,
                    structured_output: false,
                    deterministic_turn_end: false,
                    plan_mode: false,
                    models,
                    isolates_user_config: false,
                    model_aliases: vec![],
                },
            },
            health,
        }
    }

    fn with(health: impl FnOnce(&mut ExecutorHealth)) -> ExecutorHealth {
        let mut h = healthy();
        health(&mut h);
        h
    }

    fn state_of(list: Vec<ExecutorInfo>) -> State {
        let mut state = State::new();
        state.apply(Update::Executors(list));
        state
    }

    fn daemon_with(features: &[&str]) -> reno_client::DaemonInfo {
        reno_client::DaemonInfo {
            hello: reno_proto::HelloResult {
                server_version: "t".into(),
                protocol_version: reno_proto::PROTOCOL_VERSION,
                features: features.iter().map(|f| (*f).to_owned()).collect(),
                pid: 1,
            },
            status: None,
        }
    }

    fn only(view: &ExecutorsView) -> &ExecutorRow {
        view.rows.rows().first().unwrap()
    }

    #[test]
    fn loading_empty_unavailable_and_failed_say_which() {
        let mut state = State::new();
        assert_eq!(build(&state, NOW).rows, Section::Loading);
        state.apply(Update::Executors(vec![]));
        let view = build(&state, NOW);
        assert_eq!(view.rows, Section::Empty);
        assert_eq!(view.lede, "No executors");
        state.apply(Update::Feed(Feed::Executors, FeedState::Unavailable));
        assert_eq!(build(&state, NOW).rows, Section::Unavailable);
        state.apply(Update::Feed(
            Feed::Executors,
            FeedState::Failed("boom".into()),
        ));
        assert_eq!(build(&state, NOW).rows, Section::Failed("boom".into()));
        // Not connected: the banner says so, over whatever is known.
        assert!(build(&state, NOW).banner.is_some());
    }

    #[test]
    fn a_healthy_executor_shows_everything_it_found() {
        let state = state_of(vec![info("claude-code", Some(healthy()))]);
        let view = build(&state, NOW);
        assert_eq!(view.lede, "Every executor is ready");
        assert_eq!(view.needing, 0);
        let row = only(&view);
        assert_eq!(row.name, "Claude Code");
        assert_eq!(row.need, None);
        assert_eq!(
            row.installed,
            Fact::new(
                Level::Good,
                "Installed",
                Some("/usr/local/bin/claude \u{b7} 2.1.325".to_owned())
            )
        );
        assert_eq!(row.version.as_ref().unwrap().text, "Version supported");
        assert_eq!(
            row.signed_in,
            Some(Fact::new(
                Level::Good,
                "Signed in",
                Some("ada@example.com".to_owned())
            ))
        );
        assert_eq!(row.access, ["Supervised", "Auto-accept edits"]);
        assert_eq!(row.sandbox, ["Read-only", "Unrestricted"]);
        assert_eq!(row.checked, "checked 3m ago");
        assert!(!row.can_install);
    }

    #[test]
    fn signed_out_is_a_need_with_the_hint() {
        let signed_out = with(|h| {
            h.signed_in = SignedIn::No {
                hint: "run `codex login`".into(),
            }
        });
        let state = state_of(vec![info("codex", Some(signed_out))]);
        let view = build(&state, NOW);
        assert_eq!(view.lede, "1 executor needs you");
        let row = only(&view);
        assert_eq!(row.need, Some(Need::SignIn));
        assert_eq!(
            row.signed_in,
            Some(Fact::new(
                Level::Bad,
                "Signed out",
                Some("run `codex login`".to_owned())
            ))
        );
        assert!(!row.can_install);
    }

    #[test]
    fn every_version_gate_reads_as_what_it_is() {
        let gate = |gate: VersionGate| {
            let state = state_of(vec![info("codex", Some(with(|h| h.version_gate = gate)))]);
            let view = build(&state, NOW);
            (only(&view).version.clone().unwrap(), only(&view).need)
        };
        assert_eq!(gate(VersionGate::Ok).0.level, Level::Good);
        let (old, need) = gate(VersionGate::TooOld {
            min: "0.9.0".into(),
        });
        assert_eq!(
            (old.level, old.text, old.detail.as_deref()),
            (Level::Bad, "Version too old", Some("needs 0.9.0 or newer"))
        );
        assert_eq!(need, Some(Need::Update));
        let (untested, need) = gate(VersionGate::Untested {
            verified: "0.169.0".into(),
        });
        assert_eq!(
            (untested.level, untested.detail.as_deref()),
            (Level::Warn, Some("verified up to 0.169.0"))
        );
        assert_eq!(need, None, "untested is a caveat, not a need");
        let (unverified, _) = gate(VersionGate::Untested {
            verified: "unverified".into(),
        });
        assert_eq!(
            unverified.detail.as_deref(),
            Some("no version of this agent is verified yet")
        );
        let (unchecked, need) = gate(VersionGate::Unchecked);
        assert_eq!((unchecked.level, need), (Level::Quiet, None));
    }

    #[test]
    fn an_unknown_sign_in_is_never_shown_as_signed_in() {
        let unknown = with(|h| h.signed_in = SignedIn::Unknown);
        let state = state_of(vec![info("acp.grok", Some(unknown))]);
        let view = build(&state, NOW);
        let fact = only(&view).signed_in.clone().unwrap();
        assert_eq!(
            (fact.level, fact.text),
            (Level::Quiet, "Sign-in not checkable")
        );
        assert_eq!(only(&view).need, None);
    }

    #[test]
    fn not_installed_comes_in_four_kinds() {
        let not = |why: NotInstalledReason| {
            let mut state = state_of(vec![info(
                "acp.hermes",
                Some(with(|h| h.installed = InstallStatus::NotInstalled { why })),
            )]);
            state.apply(Update::Daemon(daemon_with(&[
                "executors",
                "executor.install",
            ])));
            build(&state, NOW)
        };
        let view = not(NotInstalledReason::NeedsInstall {
            what: "the Hermes adapter (npm)".into(),
        });
        let row = only(&view);
        assert_eq!(row.need, Some(Need::Install));
        assert!(row.can_install, "the daemon offers executor.install");
        assert_eq!(row.installed.text, "Needs install");
        assert!(row.version.is_none() && row.signed_in.is_none());

        let view = not(NotInstalledReason::NotFound {
            hint: "install it with brew".into(),
        });
        let row = only(&view);
        assert_eq!(row.need, Some(Need::Setup));
        assert!(!row.can_install, "not the daemon's to fetch");
        assert_eq!(
            row.installed.detail.as_deref(),
            Some("install it with brew")
        );

        let view = not(NotInstalledReason::Unusable {
            detail: "not executable".into(),
        });
        assert_eq!(only(&view).need, Some(Need::Setup));

        // A probe that could not finish says nothing: no need, quiet.
        let view = not(NotInstalledReason::ProbeFailed {
            detail: "timed out".into(),
        });
        let row = only(&view);
        assert_eq!(row.need, None);
        assert_eq!(row.installed.level, Level::Quiet);
        assert_eq!(view.lede, "Every executor is ready");
    }

    #[test]
    fn an_executor_nobody_has_probed_says_so() {
        let state = state_of(vec![info("codex", None)]);
        let view = build(&state, NOW);
        let row = only(&view);
        assert_eq!(row.installed.text, "Not checked yet");
        assert_eq!(row.checked, "not checked yet");
        assert_eq!(row.need, None);
        assert!(row.version.is_none() && row.signed_in.is_none());
        assert_eq!(row.models, "no models listed");
    }

    #[test]
    fn the_model_catalog_says_where_it_came_from() {
        let models = |source_live: bool, n: usize| {
            let list: Vec<ModelInfo> = (0..n)
                .map(|i| ModelInfo {
                    id: format!("m{i}"),
                    display_name: None,
                })
                .collect();
            let catalog = if source_live {
                ModelCatalog::live(list)
            } else {
                ModelCatalog::curated(list)
            };
            let state = state_of(vec![info("codex", Some(with(|h| h.models = catalog)))]);
            only(&build(&state, NOW)).models.clone()
        };
        assert_eq!(models(true, 3), "3 models, from the CLI");
        assert_eq!(models(false, 1), "1 model, built-in list");
        assert_eq!(models(false, 0), "no models listed");
    }

    #[test]
    fn what_needs_you_leads_and_the_rest_follows_by_name() {
        let installed = |id: &str| info(id, Some(healthy()));
        let out = info(
            "codex",
            Some(with(|h| h.signed_in = SignedIn::No { hint: "x".into() })),
        );
        let old = info(
            "acp.grok",
            Some(with(|h| {
                h.version_gate = VersionGate::TooOld { min: "9".into() }
            })),
        );
        let needs_install = info(
            "acp.hermes",
            Some(with(|h| {
                h.installed = InstallStatus::NotInstalled {
                    why: NotInstalledReason::NeedsInstall { what: "x".into() },
                }
            })),
        );
        let state = state_of(vec![
            installed("mock"),
            needs_install,
            installed("claude-code"),
            old,
            out,
        ]);
        let view = build(&state, NOW);
        let order: Vec<&str> = view.rows.rows().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            order,
            ["codex", "acp.grok", "acp.hermes", "claude-code", "mock"]
        );
        assert_eq!(view.needing, 3);
        assert_eq!(view.lede, "3 executors need you");
    }

    #[test]
    fn install_is_offered_only_when_the_daemon_has_it() {
        use reno_proto::{HelloResult, PROTOCOL_VERSION};
        let needs_install = info(
            "acp.hermes",
            Some(with(|h| {
                h.installed = InstallStatus::NotInstalled {
                    why: NotInstalledReason::NeedsInstall { what: "x".into() },
                }
            })),
        );
        let mut state = State::new();
        state.apply(Update::Executors(vec![needs_install]));
        assert!(!only(&build(&state, NOW)).can_install, "no daemon yet");
        let hello = |features: &[&str]| reno_client::DaemonInfo {
            hello: HelloResult {
                server_version: "t".into(),
                protocol_version: PROTOCOL_VERSION,
                features: features.iter().map(|f| (*f).to_owned()).collect(),
                pid: 1,
            },
            status: None,
        };
        state.apply(Update::Daemon(hello(&["executors"])));
        let view = build(&state, NOW);
        assert!(!view.installs && !only(&view).can_install);
        state.apply(Update::Daemon(hello(&["executors", "executor.install"])));
        let view = build(&state, NOW);
        assert!(view.installs && only(&view).can_install);
    }

    #[test]
    fn a_failed_run_shows_its_executors_health_only_for_an_executor_reason() {
        use reno_proto::RunSummary;
        let refused = |failure: StartRefusal| StopDetail::StartRefused {
            failure,
            detail: "words for a person, never read".into(),
        };
        let run = |status: RunStatus, stop: Option<StopDetail>| RunSummary {
            run_id: RunId::from_suffix("r1").unwrap(),
            task_id: "task_t1".parse().unwrap(),
            title: "x".into(),
            executor: "codex".parse().unwrap(),
            status,
            attempt: 1,
            created_at_ms: NOW,
            updated_at_ms: NOW,
            model: None,
            prompt: None,
            bot: None,
            stop_reason: None,
            stop,
            not_resumed: None,
            session: None,
        };
        let signed_out = with(|h| {
            h.signed_in = SignedIn::No {
                hint: "login".into(),
            }
        });
        let mut state = State::new();
        state.apply(Update::Executors(vec![info("codex", Some(signed_out))]));
        let id = RunId::from_suffix("r1").unwrap();
        let shown = |state: &State| failed_executor(state, &id, NOW);

        // The three executor reasons, whatever the words say.
        for failure in [
            StartRefusal::NotSignedIn,
            StartRefusal::NotInstalled,
            StartRefusal::NoEligibleExecutor,
        ] {
            state.apply(Update::Runs(vec![run(
                RunStatus::Failed,
                Some(refused(failure)),
            )]));
            let found = shown(&state).unwrap_or_else(|| panic!("{failure:?}"));
            assert_eq!(found.row.need, Some(Need::SignIn));
        }
        // Other failures are not about the executor, even when the words say they are.
        for stop in [
            Some(refused(StartRefusal::WorkspaceRefused)),
            Some(refused(StartRefusal::Invalid)),
            Some(StopDetail::Crashed),
            None,
        ] {
            state.apply(Update::Runs(vec![run(RunStatus::Failed, stop)]));
            assert!(shown(&state).is_none());
        }
        // A run that did not fail, or whose executor is not listed, shows nothing.
        let refusal = Some(refused(StartRefusal::NotSignedIn));
        state.apply(Update::Runs(vec![run(
            RunStatus::Succeeded,
            refusal.clone(),
        )]));
        assert!(shown(&state).is_none());
        state.apply(Update::Runs(vec![run(RunStatus::Failed, refusal)]));
        state.apply(Update::Executors(vec![]));
        assert!(shown(&state).is_none());
    }
}
