//! What Reno's audit of the ACP registry (2026-11-18, every agent run through Reno's own install,
//! launch and handshake path with no model call and nothing signed in) found out about each agent:
//! how it signs in, and whether it works. The registry's index carries none of this.
//!
//! The facts are about the version audited. [`facts`] sends them for any version of the agent,
//! marked `checked_version` when the listed one differs, and sends a `status` only for the version
//! audited: a new release may fix what was broken (or break what worked).
//!
//! The table is curated by hand from the audit. Update it with a new audit, and bump the version
//! beside each agent that was run again.

use reno_proto::{RegistrySignInOption, RegistryStatus};

/// One way to sign in.
#[derive(Debug, Clone, Copy)]
enum Way {
    None,
    Env(&'static [&'static str]),
    Terminal(&'static str),
    Browser(&'static [&'static str]),
    Setup(&'static str),
}

#[derive(Debug, Clone, Copy)]
enum State {
    Ok,
    BrokenUpstream(&'static str),
    NeedsRenoFix(&'static str),
}

struct Known {
    id: &'static str,
    /// The version audited.
    version: &'static str,
    /// Best first.
    sign_in: &'static [Way],
    state: State,
}

use State::{BrokenUpstream, NeedsRenoFix, Ok as Works};
use Way::{Browser, Env, None as Nothing, Setup, Terminal};

const fn k(
    id: &'static str,
    version: &'static str,
    sign_in: &'static [Way],
    state: State,
) -> Known {
    Known {
        id,
        version,
        sign_in,
        state,
    }
}

const KNOWN: &[Known] = &[
    k(
        "agoragentic-acp",
        "1.3.0",
        &[Env(&["AGORAGENTIC_API_KEY"])],
        BrokenUpstream("its --acp mode has no session/new (the package is an MCP server)"),
    ),
    k("amp-acp", "0.13.0", &[Terminal("amp-acp --setup")], Works),
    k(
        "antigravity-acp",
        "1.5.0",
        &[
            Browser(&["oauth-personal", "oauth-business"]),
            Env(&["GEMINI_API_KEY"]),
            Env(&["GOOGLE_API_KEY"]),
            Env(&["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_LOCATION"]),
        ],
        Works,
    ),
    k("auggie", "0.37.0", &[Terminal("auggie login")], Works),
    k("autohand", "0.9.11", &[Terminal("autohand --setup")], Works),
    k("bob", "2.1.0", &[Browser(&["sso"])], Works),
    k(
        "claude-acp",
        "0.108.0",
        &[
            Terminal("claude-agent-acp --cli auth login --claudeai"),
            Terminal("claude-agent-acp --cli auth login --console"),
        ],
        Works,
    ),
    k(
        "cline",
        "3.0.74",
        &[Browser(&["cline", "cline-pass", "openai-codex"])],
        Works,
    ),
    k(
        "codebuddy-code",
        "2.185.0",
        &[Browser(&["iOA", "external", "internal", "selfhosted"])],
        Works,
    ),
    k(
        "codex-acp",
        "2.7.0",
        &[Browser(&["chat-gpt"]), Env(&["OPENAI_API_KEY"])],
        Works,
    ),
    k(
        "cortex-code",
        "1.0.73",
        &[Setup("uses its own configuration (a Snowflake account)")],
        Works,
    ),
    k(
        "corust-agent",
        "0.6.0",
        &[Browser(&["oauth_browser"])],
        Works,
    ),
    k(
        "crow-cli",
        "0.1.49",
        &[Nothing],
        BrokenUpstream("session/new fails: its bundle lacks the crow_cli.mcp.vision module"),
    ),
    k(
        "cursor",
        "2026.11.13",
        &[
            Setup("run Cursor's `agent login` first"),
            Browser(&["cursor_login"]),
        ],
        Works,
    ),
    k(
        "deepagents",
        "0.1.7",
        &[Env(&["ANTHROPIC_API_KEY"]), Env(&["OPENAI_API_KEY"])],
        Works,
    ),
    k(
        "devin",
        "3000.14.30821",
        &[Browser(&["devin-browser"]), Terminal("devin acp --login")],
        Works,
    ),
    k(
        "dimcode",
        "0.5.29",
        &[Terminal("dimcode acp --auth-setup")],
        Works,
    ),
    k(
        "dirac",
        "0.6.1",
        &[
            Env(&["DIRAC_PROVIDER", "DIRAC_MODEL"]),
            Terminal("dirac --acp-auth"),
            Browser(&["openai-codex-oauth"]),
        ],
        Works,
    ),
    k(
        "factory-droid",
        "1.14.0",
        &[Browser(&["device-pairing"]), Env(&["FACTORY_API_KEY"])],
        Works,
    ),
    k(
        "fast-agent",
        "0.10.1",
        &[Setup(
            "keys in fast-agent.secrets.yaml, or the provider's environment variables",
        )],
        Works,
    ),
    k(
        "gemini",
        "0.68.0",
        &[Browser(&["oauth-personal"]), Env(&["GEMINI_API_KEY"])],
        Works,
    ),
    k(
        "github-copilot-cli",
        "1.0.113",
        &[Terminal("copilot login")],
        Works,
    ),
    k(
        "glm-acp-agent",
        "1.17.1",
        &[Env(&["Z_AI_API_KEY"]), Terminal("glm-acp-agent --setup")],
        Works,
    ),
    k(
        "goose",
        "1.60.1",
        &[
            Terminal("goose configure"),
            Env(&["GOOSE_PROVIDER", "GOOSE_MODEL"]),
        ],
        Works,
    ),
    k("grok-build", "1.0.89", &[Browser(&["grok.com"])], Works),
    k(
        "harn",
        "0.10.202",
        &[Nothing],
        // Was NeedsRenoFix (environmentPolicy); fixed, and re-probed ok on 2026-11-19.
        Works,
    ),
    k(
        "junie",
        "4020.10.0",
        &[Browser(&["jetbrains-account"]), Terminal("junie")],
        Works,
    ),
    k("kilo", "7.8.27", &[Terminal("kilo auth login")], Works),
    k(
        "kimchi",
        "1.35.1",
        &[
            Browser(&["kimchi-sso", "kimchi-agent"]),
            Terminal("kimchi login"),
        ],
        Works,
    ),
    k("kimi", "1.52.0", &[Terminal("kimi login")], Works),
    k("kimi-code", "2.6.3", &[Terminal("kimi-code login")], Works),
    k(
        "kiro",
        "3.0.0",
        &[Env(&["KIRO_API_KEY"]), Terminal("kiro-cli login")],
        Works,
    ),
    k(
        "minimax-code",
        "0.2.7",
        &[],
        NeedsRenoFix(
            "exits before initialize: its native better-sqlite3 binding is built by an install script Reno skips",
        ),
    ),
    k(
        "minion-code",
        "0.1.44",
        &[],
        BrokenUpstream("exits before initialize: it does not import against today's acp package"),
    ),
    k(
        "mistral-vibe",
        "3.1.1",
        &[Browser(&["browser-auth"]), Terminal("vibe-acp --setup")],
        Works,
    ),
    k("muse-code", "1.4.14", &[Browser(&["muse-login"])], Works),
    k("nova", "1.1.66", &[Terminal("nova setup")], Works),
    k(
        "opencode",
        "2.3.2",
        &[Terminal("opencode auth login")],
        Works,
    ),
    k(
        "opencode-v2",
        "2.3.2",
        &[Terminal("opencode auth login")],
        Works,
    ),
    k(
        "pi-acp",
        "0.0.34",
        &[Terminal("pi --terminal-login")],
        Works,
    ),
    k("poolside", "1.0.16", &[Terminal("pool login")], Works),
    k("qoder", "1.1.91", &[Terminal("qodercli login")], Works),
    k("qwen-code", "0.28.0", &[Env(&["OPENAI_API_KEY"])], Works),
    k("sigit", "1.7.10", &[Browser(&["sigit"])], Works),
    k(
        "stakpak",
        "0.3.88",
        &[Terminal("stakpak auth login"), Browser(&["stakpak"])],
        Works,
    ),
    k(
        "vtcode",
        "0.96.14",
        &[Setup("provider keys come from the environment")],
        Works,
    ),
];

/// What the audit knows of an agent, for the version the registry lists now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// How it signs in, best first. Empty when the agent was not audited.
    pub sign_in: Vec<RegistrySignInOption>,
    /// Whether it works: only for the version audited.
    pub status: Option<RegistryStatus>,
    /// The version the facts were checked at, when the listed one is another.
    pub checked_version: Option<String>,
}

/// The audit's facts for registry agent `id`, which the index lists at `listed_version`.
pub fn facts(id: &str, listed_version: &str) -> Facts {
    let Some(known) = KNOWN.iter().find(|k| k.id == id) else {
        return Facts::default();
    };
    let same = known.version == listed_version;
    Facts {
        sign_in: known
            .sign_in
            .iter()
            .map(|way| match *way {
                Way::None => RegistrySignInOption::None,
                Way::Env(vars) => RegistrySignInOption::Env {
                    vars: vars.iter().map(|v| (*v).to_owned()).collect(),
                },
                Way::Terminal(command) => RegistrySignInOption::Terminal {
                    command: command.to_owned(),
                },
                Way::Browser(methods) => RegistrySignInOption::Browser {
                    methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                },
                Way::Setup(text) => RegistrySignInOption::Setup {
                    text: text.to_owned(),
                },
            })
            .collect(),
        status: same.then(|| match known.state {
            State::Ok => RegistryStatus::Ok,
            State::BrokenUpstream(reason) => RegistryStatus::BrokenUpstream {
                reason: reason.to_owned(),
            },
            State::NeedsRenoFix(reason) => RegistryStatus::NeedsRenoFix {
                reason: reason.to_owned(),
            },
        }),
        checked_version: (!same).then(|| known.version.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_id_is_a_registry_id_and_every_variable_is_upper_case() {
        let mut seen = std::collections::BTreeSet::new();
        for known in KNOWN {
            assert!(
                crate::RegistryAgent::new(known.id).is_some(),
                "{} is not a registry id",
                known.id
            );
            assert!(seen.insert(known.id), "{} is listed twice", known.id);
            assert!(!known.version.is_empty(), "{} has no version", known.id);
            for way in known.sign_in {
                if let Way::Env(vars) = way {
                    assert!(
                        !vars.is_empty(),
                        "{}: an env way names no variable",
                        known.id
                    );
                    for var in *vars {
                        assert!(
                            var.chars()
                                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                                && var.starts_with(|c: char| c.is_ascii_uppercase()),
                            "{}: {var} is not an upper-case variable name",
                            known.id
                        );
                    }
                }
            }
        }
        assert_eq!(KNOWN.len(), 46, "one row per agent of the audit");
    }

    #[test]
    fn a_listed_version_that_differs_keeps_the_facts_and_drops_the_status() {
        let audited = facts("crow-cli", "0.1.49");
        assert!(matches!(
            audited.status,
            Some(RegistryStatus::BrokenUpstream { .. })
        ));
        assert_eq!(audited.checked_version, None);

        let newer = facts("crow-cli", "0.2.0");
        assert_eq!(newer.status, None, "a new release may fix it");
        assert_eq!(newer.checked_version.as_deref(), Some("0.1.49"));
        assert_eq!(newer.sign_in, audited.sign_in);
    }

    #[test]
    fn harn_runs_now_that_reno_sends_its_environment_policy() {
        // Re-probed 2026-11-19 on 0.10.202 after the environmentPolicy fix: session/new is ok.
        assert_eq!(facts("harn", "0.10.202").status, Some(RegistryStatus::Ok));
    }

    #[test]
    fn an_agent_outside_the_audit_has_no_facts() {
        assert_eq!(facts("brand-new", "1.0.0"), Facts::default());
        let gemini = facts("gemini", "0.68.0");
        assert_eq!(gemini.status, Some(RegistryStatus::Ok));
        assert_eq!(
            gemini.sign_in[1],
            RegistrySignInOption::Env {
                vars: vec!["GEMINI_API_KEY".into()]
            }
        );
    }
}
