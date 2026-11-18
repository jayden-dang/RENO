//! What a harness declares about being installed, updated and signed in to (EX-1, EX-3, EX-11,
//! EX-12, EX-13).
//!
//! The commands are compiled in per adapter. They are data: this module runs nothing. A person
//! presses a button or turns auto-update on, the daemon looks the adapter's declaration up by
//! executor id and runs the steps it holds through a [`crate::proc::maintain::CommandRunner`].
//! No command ever comes from the wire.
//!
//! A declaration lists its updaters in order of preference with the way each one was installed
//! ([`InstalledVia`]): Codex updates a Homebrew cask with `brew` and an npm install with its own
//! `codex update`, and which one it is depends on where the probe found the binary.

use std::path::Path;

use reno_proto::{PersonStep, SetupRecipe, SetupStep, StepRunner};

use crate::HarnessId;
use crate::proc::self_update::SelfUpdateSwitch;

/// What an adapter declares. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceDecl {
    /// How to install the harness when it is not found. `None`: no recipe is known.
    pub setup: Option<SetupRecipe>,
    /// The command a person runs to sign in.
    pub sign_in: Option<PersonStep>,
    /// The updaters, the first whose [`UpdaterDecl::via`] matches the install wins.
    pub updaters: Vec<UpdaterDecl>,
    /// A Reno-managed adapter (a pinned package or archive): it updates with Reno, through
    /// `Executor::install`, not through an updater of its own.
    pub managed: bool,
}

impl MaintenanceDecl {
    /// The updater for a harness installed at `installed` (the path the probe found). The path
    /// is resolved through symlinks: Homebrew links `/opt/homebrew/bin/codex` into the Caskroom.
    pub fn updater_for(&self, installed: Option<&Path>) -> Option<&UpdaterDecl> {
        let cask = installed.is_some_and(|path| {
            std::fs::canonicalize(path)
                .unwrap_or_else(|_| path.to_path_buf())
                .components()
                .any(|c| c.as_os_str() == "Caskroom")
        });
        self.updaters.iter().find(|updater| match updater.via {
            InstalledVia::Any => true,
            InstalledVia::HomebrewCask => cask,
        })
    }
}

/// How an installed harness got there, which decides the updater.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstalledVia {
    /// Any install not matched by an earlier updater.
    Any,
    /// A Homebrew cask (the binary resolves into a `Caskroom` directory).
    HomebrewCask,
}

/// One harness updater.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdaterDecl {
    /// The installs this updater is for.
    pub via: InstalledVia,
    /// The command a person sees ("codex update").
    pub command: &'static str,
    /// The steps of an update, in order. Each is one shell command line.
    pub steps: &'static [&'static str],
    /// The command that installs a given version, with `{version}` for it. `None`: the updater
    /// cannot pin.
    pub pinned: Option<&'static str>,
    /// How to find out whether a newer version exists. `None`: it cannot be asked.
    pub check: Option<CheckDecl>,
    /// The CLI updates itself already (Claude Code, Grok).
    pub self_updating: bool,
    /// How to turn that off. `Some` exactly where `self_updating` is.
    pub switch: Option<SelfUpdateSwitch>,
}

impl UpdaterDecl {
    /// The steps that update, pinned to `version` when one is given. `None` when a pin was asked
    /// for and the updater cannot pin, or `version` is not a plain version string (it ends up in
    /// a shell command line).
    pub fn update_steps(&self, version: Option<&str>) -> Option<Vec<String>> {
        match version {
            None => Some(self.steps.iter().map(|s| (*s).to_owned()).collect()),
            Some(version) => {
                let template = self.pinned?;
                is_plain_version(version).then(|| vec![template.replace("{version}", version)])
            }
        }
    }
}

/// Only characters a version has: it is put in a command line.
fn is_plain_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

/// Whether `latest` is a newer version than `installed`. Either one that does not read as a
/// version (`1.2.3`, `v1.2.3`, `1.2`) says no: nothing is updated on a guess.
pub fn is_newer(latest: &str, installed: &str) -> bool {
    fn parse(text: &str) -> Option<semver::Version> {
        let text = text.trim().trim_start_matches('v');
        semver::Version::parse(text).ok().or_else(|| {
            let mut parts = text.split('.');
            let major = parts.next()?;
            let minor = parts.next().unwrap_or("0");
            let patch = parts.next().unwrap_or("0");
            if parts.next().is_some() {
                return None;
            }
            semver::Version::parse(&format!("{major}.{minor}.{patch}")).ok()
        })
    }
    matches!((parse(latest), parse(installed)), (Some(l), Some(i)) if l > i)
}

/// The JSON object in `text`, from its first `{` to its last `}`: a CLI may print a notice
/// before or after it.
fn json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| &text[start..=end])
}

/// A command that finds the newest version, and how to read its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckDecl {
    /// The command line.
    pub command: &'static str,
    /// What it prints.
    pub parse: CheckParse,
}

/// How to read a check's standard output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckParse {
    /// `grok update --check --json`: `{"currentVersion", "latestVersion", "updateAvailable"}`.
    GrokJson,
    /// `brew outdated --cask --json=v2 <cask>`: lists the cask only when it is outdated.
    BrewOutdatedJson,
    /// `npm view <package> version`: the version alone.
    PlainVersion,
}

impl CheckParse {
    /// The newest version `stdout` says there is. `installed` is the version running now: a
    /// check that lists nothing (Homebrew) says it is the newest.
    pub fn latest(self, stdout: &str, installed: Option<&str>) -> Option<String> {
        match self {
            Self::PlainVersion => {
                let version = stdout.lines().map(str::trim).rfind(|l| !l.is_empty())?;
                is_plain_version(version).then(|| version.to_owned())
            }
            Self::GrokJson => {
                let found: serde_json::Value = serde_json::from_str(json_object(stdout)?).ok()?;
                let latest = found.get("latestVersion")?.as_str()?.trim();
                is_plain_version(latest).then(|| latest.to_owned())
            }
            Self::BrewOutdatedJson => {
                let found: serde_json::Value = serde_json::from_str(json_object(stdout)?).ok()?;
                let casks = found.get("casks")?.as_array()?;
                match casks.first() {
                    Some(cask) => {
                        let latest = cask.get("current_version")?.as_str()?.trim();
                        is_plain_version(latest).then(|| latest.to_owned())
                    }
                    None => installed.map(str::to_owned),
                }
            }
        }
    }
}

fn step(label: &str, command: &str, runner: StepRunner) -> SetupStep {
    SetupStep {
        label: label.to_owned(),
        command: command.to_owned(),
        runner,
    }
}

fn person(label: &str, command: &str) -> PersonStep {
    PersonStep {
        label: label.to_owned(),
        command: command.to_owned(),
    }
}

/// The declaration of the harness `id`.
///
/// The updaters and sign-in commands of Claude Code, Codex and Grok, and Devin's sign-in, were
/// verified on a real machine. The install recipes of Devin, Fx and Hermes are UNVERIFIED: they
/// are the commands their hints have always named, and Reno has not run them yet.
pub fn declaration(id: &HarnessId) -> Option<MaintenanceDecl> {
    let none = MaintenanceDecl {
        setup: None,
        sign_in: None,
        updaters: Vec::new(),
        managed: false,
    };
    Some(match id {
        HarnessId::ClaudeCode => MaintenanceDecl {
            sign_in: Some(person("Sign in", "claude auth login")),
            updaters: vec![UpdaterDecl {
                via: InstalledVia::Any,
                command: "claude update",
                steps: &["claude update"],
                pinned: Some("claude install {version}"),
                // Claude Code has no check-only command.
                check: None,
                self_updating: true,
                switch: Some(SelfUpdateSwitch::ClaudeSettings),
            }],
            ..none
        },
        HarnessId::Codex => MaintenanceDecl {
            sign_in: Some(person("Sign in", "codex login")),
            updaters: vec![
                // Whether `codex update` runs unattended on a cask is UNVERIFIED; Homebrew owns
                // the files, so it is Homebrew that upgrades them.
                UpdaterDecl {
                    via: InstalledVia::HomebrewCask,
                    command: "brew upgrade --cask codex",
                    steps: &["brew upgrade --cask codex"],
                    pinned: None,
                    check: Some(CheckDecl {
                        // `brew outdated` exits 1 exactly when something IS outdated (seen on the
                        // owner's Mac, 2026-11-18), and the check counts only exit 0 as an answer.
                        // `|| true` keeps the answer; a real failure prints no JSON, so the parser
                        // still says nothing.
                        command: "brew outdated --cask --json=v2 codex || true",
                        parse: CheckParse::BrewOutdatedJson,
                    }),
                    self_updating: false,
                    switch: None,
                },
                UpdaterDecl {
                    via: InstalledVia::Any,
                    command: "codex update",
                    steps: &["codex update"],
                    pinned: None,
                    check: Some(CheckDecl {
                        command: "npm view @openai/codex version",
                        parse: CheckParse::PlainVersion,
                    }),
                    self_updating: false,
                    switch: None,
                },
            ],
            ..none
        },
        HarnessId::Grok => MaintenanceDecl {
            sign_in: Some(person("Sign in", "grok login")),
            updaters: vec![UpdaterDecl {
                via: InstalledVia::Any,
                command: "grok update",
                steps: &["grok update"],
                pinned: Some("grok update --version {version}"),
                check: Some(CheckDecl {
                    command: "grok update --check --json",
                    parse: CheckParse::GrokJson,
                }),
                self_updating: true,
                switch: Some(SelfUpdateSwitch::GrokConfig),
            }],
            ..none
        },
        HarnessId::Devin => MaintenanceDecl {
            // UNVERIFIED recipe.
            setup: Some(SetupRecipe {
                source: "Homebrew cask devin-cli".to_owned(),
                steps: vec![
                    step("Install", "brew install --cask devin-cli", StepRunner::Reno),
                    step("Sign in", "devin auth login", StepRunner::Person),
                ],
            }),
            sign_in: Some(person("Sign in", "devin auth login")),
            ..none
        },
        HarnessId::Fx => MaintenanceDecl {
            // UNVERIFIED recipe.
            setup: Some(SetupRecipe {
                source: "fx.sh".to_owned(),
                steps: vec![
                    step(
                        "Install",
                        "curl -fsSL https://fx.sh/setup.sh | bash",
                        StepRunner::Reno,
                    ),
                    step("Pick a provider", "fx", StepRunner::Person),
                ],
            }),
            ..none
        },
        HarnessId::Hermes => MaintenanceDecl {
            // UNVERIFIED recipe.
            setup: Some(SetupRecipe {
                source: "hermes-agent.nousresearch.com".to_owned(),
                steps: vec![
                    step(
                        "Install",
                        "curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash",
                        StepRunner::Reno,
                    ),
                    step(
                        "Add the ACP server",
                        "cd ~/.hermes/hermes-agent && uv pip install -e '.[acp]'",
                        StepRunner::Reno,
                    ),
                ],
            }),
            ..none
        },
        HarnessId::Antigravity => MaintenanceDecl {
            managed: true,
            ..none
        },
        HarnessId::Registry(_) => return None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_needs_an_updater_that_can_pin_and_a_plain_version() {
        let claude = declaration(&HarnessId::ClaudeCode).unwrap();
        let updater = &claude.updaters[0];
        assert_eq!(
            updater.update_steps(Some("2.1.325")),
            Some(vec!["claude install 2.1.325".to_owned()])
        );
        assert_eq!(
            updater.update_steps(None),
            Some(vec!["claude update".to_owned()])
        );
        // A version is put in a shell command: anything else is refused.
        assert_eq!(updater.update_steps(Some("1; rm -rf ~")), None);
        assert_eq!(updater.update_steps(Some("")), None);
        let codex = declaration(&HarnessId::Codex).unwrap();
        assert_eq!(codex.updaters[0].update_steps(Some("0.1.0")), None);
    }

    #[test]
    fn newer_means_a_higher_version_and_never_a_guess() {
        assert!(is_newer("0.171.0", "0.170.0"));
        assert!(is_newer("v1.2", "1.1.9"));
        assert!(!is_newer("0.170.0", "0.170.0"));
        assert!(!is_newer("0.169.0", "0.170.0"));
        assert!(!is_newer("nightly", "0.170.0"));
        assert!(!is_newer("0.171.0", "mock"));
    }

    #[test]
    fn grok_pins_with_the_update_command() {
        let grok = declaration(&HarnessId::Grok).unwrap();
        assert_eq!(
            grok.updaters[0].update_steps(Some("1.0.4")),
            Some(vec!["grok update --version 1.0.4".to_owned()])
        );
    }

    #[test]
    fn checks_are_read_from_what_each_command_prints() {
        let grok = r#"{"currentVersion":"1.0.4","latestVersion":"1.0.46","updateAvailable":true}"#;
        assert_eq!(
            CheckParse::GrokJson.latest(grok, Some("1.0.4")).as_deref(),
            Some("1.0.46")
        );
        let outdated = r#"{"formulae":[],"casks":[{"name":"codex","installed_versions":["0.170.0"],"current_version":"0.171.0"}]}"#;
        assert_eq!(
            CheckParse::BrewOutdatedJson
                .latest(outdated, Some("0.170.0"))
                .as_deref(),
            Some("0.171.0")
        );
        // Homebrew's real answer on the owner's Mac (pretty-printed, exit 1 under `|| true`).
        let real = "{\n  \"formulae\": [],\n  \"casks\": [\n    {\n      \"name\": \"codex\",\n      \"installed_versions\": [\n        \"0.169.0\"\n      ],\n      \"current_version\": \"0.170.0\",\n      \"pinned\": false,\n      \"pinned_version\": null\n    }\n  ]\n}\n";
        assert_eq!(
            CheckParse::BrewOutdatedJson
                .latest(real, Some("0.169.0"))
                .as_deref(),
            Some("0.170.0")
        );
        // A brew that failed (and was let through by `|| true`) prints no JSON: no answer.
        assert_eq!(
            CheckParse::BrewOutdatedJson
                .latest("Error: Cask 'codex' is not installed.\n", Some("0.169.0")),
            None
        );
        // Homebrew lists nothing when the cask is current: the newest is what is installed.
        assert_eq!(
            CheckParse::BrewOutdatedJson
                .latest(r#"{"formulae":[],"casks":[]}"#, Some("0.170.0"))
                .as_deref(),
            Some("0.170.0")
        );
        assert_eq!(
            CheckParse::PlainVersion
                .latest("npm notice\n0.171.0\n", None)
                .as_deref(),
            Some("0.171.0")
        );
        // A notice before the JSON does not hide it.
        assert_eq!(
            CheckParse::GrokJson
                .latest(&format!("checking...\n{grok}\n"), None)
                .as_deref(),
            Some("1.0.46")
        );
        assert_eq!(CheckParse::GrokJson.latest("not json", None), None);
        assert_eq!(CheckParse::PlainVersion.latest("<html>", None), None);
    }

    #[test]
    fn codex_picks_its_updater_by_where_the_binary_resolves() {
        let codex = declaration(&HarnessId::Codex).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cask = dir.path().join("Caskroom/codex/0.170.0/bin");
        std::fs::create_dir_all(&cask).unwrap();
        std::fs::write(cask.join("codex"), "").unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(cask.join("codex"), bin.join("codex")).unwrap();
        // The probe reports the symlink; the cask is what it resolves to.
        assert_eq!(
            codex.updater_for(Some(&bin.join("codex"))).unwrap().command,
            "brew upgrade --cask codex"
        );
        let npm = dir.path().join("npm/codex");
        std::fs::create_dir_all(npm.parent().unwrap()).unwrap();
        std::fs::write(&npm, "").unwrap();
        assert_eq!(
            codex.updater_for(Some(&npm)).unwrap().command,
            "codex update"
        );
        assert_eq!(codex.updater_for(None).unwrap().command, "codex update");
    }

    #[test]
    fn recipes_name_who_runs_each_step() {
        let hermes = declaration(&HarnessId::Hermes).unwrap();
        let steps = hermes.setup.unwrap().steps;
        assert!(steps.iter().all(|s| s.runner == StepRunner::Reno));
        let devin = declaration(&HarnessId::Devin).unwrap().setup.unwrap();
        assert_eq!(devin.steps[1].runner, StepRunner::Person);
        assert!(declaration(&HarnessId::Antigravity).unwrap().managed);
        assert!(
            declaration(&HarnessId::Antigravity)
                .unwrap()
                .setup
                .is_none()
        );
    }
}
