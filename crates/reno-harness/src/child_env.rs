//! The environment an agent child gets: an explicit allowlist, not the daemon's whole
//! environment (T1.13).
//!
//! A daemon started by a shell or launchd carries whatever its parent had: tokens for other
//! tools, cloud credentials, a developer's `GITHUB_TOKEN`. An agent runs arbitrary commands, so
//! everything it inherits is something a prompt-injected run can print or send. Each run spawn
//! (`restrict`) therefore clears the child's environment and puts back only:
//!
//! 1. the **base set** ([`BASE`]): the variables every CLI needs to run at all (`HOME`, `USER`,
//!    `LANG`/`LC_*`, `TMPDIR`, `TERM`, `SHELL`, the TLS-trust, proxy and SSH-agent variables the
//!    login-shell snapshot also captures, and `XDG_*`); `PATH` is composed separately
//!    ([`crate::compose_child_path`]) from the login shell's PATH;
//! 2. the **driver's own set** ([`for_harness`]): the variables that agent needs to find its
//!    configuration and to sign in (its config-dir variable, its provider API keys);
//! 3. what the run asks for explicitly, applied afterwards by [`crate::apply_run_env`]
//!    (`ExecSpec::env`): the bot's secrets and Reno's own `RENO_*` run variables
//!    (`RENO_RUN_ID`, `RENO_BOT`).
//!
//! Everything else in the daemon's environment is dropped, including every other `RENO_*`
//! variable. What a driver sets itself (`CLAUDE_CODE_ENTRYPOINT`, `GEMINI_HOME`, a registry
//! agent's own `env`) is applied after this and is not subject to it.
//!
//! Codex's app-server (runs and its account probe) is restricted the same way. The version and
//! `claude auth status` probes, and the interactive sign-in flows, are not runs and are not
//! restricted: they run the user's own CLI as the user, to learn whether it works, and receive
//! no secret.
//!
//! The per-driver lists are a best reading, not a verified one. Only `CLAUDE_CONFIG_DIR`, the
//! `ANTHROPIC_*` family, `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CODE_USE_BEDROCK`,
//! `CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_GIT_BASH_PATH`, `CODEX_HOME` and `GEMINI_HOME` are named
//! by this repository's own code. Every other name (the AWS and Google cloud credentials, the
//! OpenAI, xAI, Nous, Devin and Gemini keys, `GROK_HOME`, `HERMES_HOME`) is from general knowledge
//! of those CLIs and has not been checked against a live one. A variable a CLI needs that is
//! missing here can be given to one bot as a secret (`secrets = ["NAME"]`), which reaches only
//! that bot's runs. A variable that only a
//! secret supplies does not satisfy a sign-in probe, which reads the daemon's own environment: an
//! executor whose probe says it is signed out is still refused before any secret is applied.

use std::ffi::OsString;

use crate::agent_id::HarnessId;
use crate::process::Command;

/// What a driver allows beyond [`BASE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverEnv {
    /// Exact variable names.
    pub names: &'static [&'static str],
    /// Variables whose name starts with one of these.
    pub prefixes: &'static [&'static str],
}

/// The base set: exact names every agent child may inherit.
pub const BASE: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TERM",
    "TZ",
    "LANG",
    "LANGUAGE",
    "__CF_USER_TEXT_ENCODING",
    // TLS trust, SSH agent and mise's PATH record: the login-shell snapshot's variables.
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "SSH_AUTH_SOCK",
    "__MISE_DIFF",
    // Proxies: an agent behind one cannot reach its provider without them.
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
];

/// Prefixes of the base set: `LC_ALL`, `LC_CTYPE` and the other locale categories.
pub const BASE_PREFIXES: &[&str] = &["LC_"];

/// Claude Code: its config dir, every `ANTHROPIC_*` setting (API key, auth token, base URL,
/// model overrides), its subscription token, and the Bedrock / Vertex switches with the cloud
/// credentials those providers read.
pub const CLAUDE: DriverEnv = DriverEnv {
    names: &[
        "CLAUDE_CONFIG_DIR",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_GIT_BASH_PATH",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
        "CLAUDE_CODE_SKIP_VERTEX_AUTH",
        "AWS_PROFILE",
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_BEARER_TOKEN_BEDROCK",
        "AWS_SHARED_CREDENTIALS_FILE",
        "AWS_CONFIG_FILE",
        "CLOUD_ML_REGION",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "GOOGLE_CLOUD_PROJECT",
    ],
    prefixes: &["ANTHROPIC_"],
};

/// Codex: its home, and the OpenAI credentials and endpoint settings its sign-in reads.
pub const CODEX: DriverEnv = DriverEnv {
    names: &[
        "CODEX_HOME",
        "CODEX_API_KEY",
        "CODEX_CA_CERTIFICATE",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "OPENAI_ORG_ID",
        "OPENAI_ORGANIZATION",
        "OPENAI_PROJECT",
    ],
    prefixes: &[],
};

/// Pi: where it keeps its settings, credentials and sessions, its offline and telemetry
/// switches, and the provider credentials and endpoints its own `--help` lists (read off Pi
/// 1.5.0, which is more than the other lists here can say). Pi reaches many providers, and a
/// run's model decides which one; a variable of a provider the owner did not sign in to is
/// simply absent.
pub const PI: DriverEnv = DriverEnv {
    names: &[
        "PI_CODING_AGENT_DIR",
        "PI_CODING_AGENT_SESSION_DIR",
        "PI_PACKAGE_DIR",
        "PI_OFFLINE",
        "PI_TELEMETRY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_OAUTH_TOKEN",
        "ANT_LING_API_KEY",
        "OPENAI_API_KEY",
        "AZURE_OPENAI_API_KEY",
        "AZURE_OPENAI_BASE_URL",
        "AZURE_OPENAI_RESOURCE_NAME",
        "AZURE_OPENAI_API_VERSION",
        "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
        "DEEPSEEK_API_KEY",
        "NVIDIA_API_KEY",
        "GEMINI_API_KEY",
        "GROQ_API_KEY",
        "CEREBRAS_API_KEY",
        "XAI_API_KEY",
        "FIREWORKS_API_KEY",
        "TOGETHER_API_KEY",
        "BASETEN_API_KEY",
        "OPENROUTER_API_KEY",
        "AI_GATEWAY_API_KEY",
        "ZAI_API_KEY",
        "ZAI_CODING_CN_API_KEY",
        "MISTRAL_API_KEY",
        "MINIMAX_API_KEY",
        "MOONSHOT_API_KEY",
        "OPENCODE_API_KEY",
        "KIMI_API_KEY",
        "META_API_KEY",
        "CLOUDFLARE_API_KEY",
        "CLOUDFLARE_ACCOUNT_ID",
        "CLOUDFLARE_GATEWAY_ID",
        "QWEN_TOKEN_PLAN_API_KEY",
        "QWEN_TOKEN_PLAN_CN_API_KEY",
        "XIAOMI_API_KEY",
        "XIAOMI_TOKEN_PLAN_CN_API_KEY",
        "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
        "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
        "AWS_PROFILE",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_BEARER_TOKEN_BEDROCK",
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
    ],
    prefixes: &[],
};

/// Muse Code: nothing beyond the base set, on purpose. Its sign-in is the owner's own login (a
/// file under `XDG_CONFIG_HOME`, which the base set carries, or the Keychain, which Muse reads
/// itself). `META_API_KEY` is NOT here: it takes priority over the login, so one exported to the
/// daemon would silently replace the owner's account for every run. A bot that is meant to run on
/// a key lists it as a secret, which reaches only its runs. Exact names only: a `MUSE_` prefix
/// would pass `MUSE_PERMISSION_PROFILE_FAIL_OPEN`, `MUSE_LAUNCHER_INSTALL`, `MUSE_SYNC_UPDATE` and
/// the like, each of which changes what the launcher or the permission system does.
pub const MUSE: DriverEnv = DriverEnv {
    names: &[],
    prefixes: &[],
};

/// Cursor (its SDK, through the shim): the one variable that signs it in. The SDK also reads
/// `CURSOR_BACKEND_URL` and `CURSOR_WEBSITE_URL` (`auth/login.d.ts`, 1.0.32) to reach another
/// backend; they are not passed, so a key minted for the default backend is never sent elsewhere.
/// The SDK's stored login (`~/.cursor/sdk/auth.json`) is a file, not a variable.
pub const CURSOR: DriverEnv = DriverEnv {
    names: &["CURSOR_API_KEY"],
    prefixes: &[],
};

/// opencode: the provider credentials and endpoints models.dev lists for its providers, read off
/// the `env` of each provider of a real 1.19.5's `/provider` (227 names; the ones that name a
/// credential or an endpoint of a major provider are kept, not the long tail). A provider the
/// owner signed in to through `opencode auth login` needs none: its credential is in
/// opencode's own auth file. NOT allowed, on purpose: `OPENCODE_CONFIG`, `OPENCODE_CONFIG_CONTENT`
/// and `OPENCODE_PERMISSION` (they would layer a config of someone else's under Reno's rules),
/// `OPENCODE_SERVER_PASSWORD` and the generic `GITHUB_TOKEN` / `GITLAB_TOKEN`.
pub const OPENCODE: DriverEnv = DriverEnv {
    names: &[
        "OPENCODE_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "GOOGLE_GENERATIVE_AI_API_KEY",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "GOOGLE_CLOUD_PROJECT",
        "OPENROUTER_API_KEY",
        "GROQ_API_KEY",
        "XAI_API_KEY",
        "MISTRAL_API_KEY",
        "DEEPSEEK_API_KEY",
        "TOGETHER_API_KEY",
        "FIREWORKS_API_KEY",
        "CEREBRAS_API_KEY",
        "COHERE_API_KEY",
        "DEEPINFRA_API_KEY",
        "BASETEN_API_KEY",
        "NVIDIA_API_KEY",
        "AI_GATEWAY_API_KEY",
        "AZURE_API_KEY",
        "AZURE_RESOURCE_NAME",
        "AWS_PROFILE",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_BEARER_TOKEN_BEDROCK",
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
        "DATABRICKS_HOST",
        "DATABRICKS_TOKEN",
    ],
    prefixes: &[],
};

const DEVIN: DriverEnv = DriverEnv {
    names: &["DEVIN_API_KEY", "WINDSURF_API_KEY"],
    prefixes: &[],
};

const GROK: DriverEnv = DriverEnv {
    names: &["XAI_API_KEY", "GROK_API_KEY", "GROK_HOME"],
    prefixes: &[],
};

const HERMES: DriverEnv = DriverEnv {
    names: &[
        "HERMES_HOME",
        "NOUS_API_KEY",
        "OPENROUTER_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
    ],
    prefixes: &[],
};

const FX: DriverEnv = DriverEnv {
    names: &["FX_API_KEY", "AI_GATEWAY_API_KEY"],
    prefixes: &[],
};

const ANTIGRAVITY: DriverEnv = DriverEnv {
    names: &[
        "GEMINI_HOME",
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "GOOGLE_CLOUD_PROJECT",
        "GOOGLE_GENAI_USE_VERTEXAI",
    ],
    prefixes: &[],
};

const GEMINI_REGISTRY: DriverEnv = DriverEnv {
    names: &[
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "GOOGLE_CLOUD_PROJECT",
        "GOOGLE_GENAI_USE_VERTEXAI",
    ],
    prefixes: &[],
};

/// Nothing beyond the base set: a registry agent takes its environment from its registry entry
/// (applied by the driver) and from the bot's secrets.
const NONE: DriverEnv = DriverEnv {
    names: &[],
    prefixes: &[],
};

/// What `harness` is allowed beyond the base set.
pub fn for_harness(harness: HarnessId) -> DriverEnv {
    match harness {
        HarnessId::ClaudeCode => CLAUDE,
        HarnessId::Codex => CODEX,
        HarnessId::Devin => DEVIN,
        HarnessId::Grok => GROK,
        HarnessId::Hermes => HERMES,
        HarnessId::Fx => FX,
        HarnessId::Antigravity => ANTIGRAVITY,
        HarnessId::Pi => PI,
        HarnessId::Muse => MUSE,
        HarnessId::Cursor => CURSOR,
        HarnessId::Opencode => OPENCODE,
        HarnessId::Registry(agent) if agent.registry_id() == "gemini" => GEMINI_REGISTRY,
        HarnessId::Registry(_) => NONE,
    }
}

/// Whether a child of `driver` may inherit the variable `name`.
pub fn allows(driver: DriverEnv, name: &str) -> bool {
    BASE.contains(&name)
        || driver.names.contains(&name)
        || BASE_PREFIXES
            .iter()
            .chain(driver.prefixes)
            .any(|prefix| name.starts_with(prefix))
}

/// The variables of `daemon_env` a child of `driver` inherits.
pub fn allowed_from(
    driver: DriverEnv,
    daemon_env: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    daemon_env
        .into_iter()
        .filter(|(key, _)| key.to_str().is_some_and(|key| allows(driver, key)))
        .collect()
}

/// Clears `cmd`'s environment and puts back what a child of `driver` may inherit from this
/// process. Call it right after `Command::new`, before anything else sets a variable.
pub(crate) fn restrict(cmd: &mut Command, driver: DriverEnv) {
    cmd.env_clear();
    cmd.envs(allowed_from(driver, std::env::vars_os()));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::ffi::OsStr;

    use super::*;
    use crate::agent_id::RegistryAgent;

    fn name_of(key: &OsStr) -> String {
        key.to_string_lossy().into_owned()
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    fn kept(driver: DriverEnv, pairs: &[(&str, &str)]) -> Vec<String> {
        allowed_from(driver, env(pairs))
            .iter()
            .map(|(k, _)| name_of(k))
            .collect()
    }

    #[test]
    fn an_arbitrary_daemon_variable_is_dropped() {
        let daemon = [
            ("HOME", "/Users/me"),
            ("PATH", "/usr/bin"),
            ("LC_ALL", "en_US.UTF-8"),
            ("GITHUB_TOKEN", "x"),
            ("AWS_SECRET_ACCESS_KEY", "x"),
            ("RENO_DATA_DIR", "/d"),
            ("RENO_SOCKET", "/s"),
            ("OPENAI_API_KEY", "x"),
            ("MY_APP_PASSWORD", "x"),
        ];
        assert_eq!(
            kept(NONE, &daemon),
            ["HOME", "PATH", "LC_ALL"],
            "only the base set survives for a driver with no extras"
        );
    }

    #[test]
    fn each_driver_keeps_its_own_sign_in_variables_and_not_anothers() {
        let daemon = [
            ("CLAUDE_CONFIG_DIR", "/c"),
            ("ANTHROPIC_API_KEY", "k"),
            ("ANTHROPIC_BASE_URL", "u"),
            ("CODEX_HOME", "/x"),
            ("OPENAI_API_KEY", "k"),
            ("XAI_API_KEY", "k"),
        ];
        assert_eq!(
            kept(for_harness(HarnessId::ClaudeCode), &daemon),
            [
                "CLAUDE_CONFIG_DIR",
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_BASE_URL"
            ]
        );
        assert_eq!(
            kept(for_harness(HarnessId::Codex), &daemon),
            ["CODEX_HOME", "OPENAI_API_KEY"]
        );
        assert_eq!(kept(for_harness(HarnessId::Grok), &daemon), ["XAI_API_KEY"]);
        assert_eq!(
            kept(for_harness(HarnessId::Pi), &daemon),
            ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "XAI_API_KEY"]
        );
    }

    #[test]
    fn registry_agents_get_only_the_base_set_but_gemini_keeps_its_keys() {
        let daemon = [("GEMINI_API_KEY", "k"), ("OPENAI_API_KEY", "k")];
        let other = RegistryAgent::new("goose").unwrap();
        assert!(kept(for_harness(HarnessId::Registry(other)), &daemon).is_empty());
        let gemini = RegistryAgent::new("gemini").unwrap();
        assert_eq!(
            kept(for_harness(HarnessId::Registry(gemini)), &daemon),
            ["GEMINI_API_KEY"]
        );
    }

    #[test]
    fn a_pi_child_never_inherits_a_claude_session_or_a_reno_variable() {
        let daemon = [
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "t"),
            ("RENO_PI_MCP", "/x"),
            ("PI_CODING_AGENT_DIR", "/d"),
            ("GITHUB_TOKEN", "g"),
        ];
        assert_eq!(kept(PI, &daemon), ["PI_CODING_AGENT_DIR"]);
    }

    #[test]
    fn a_muse_child_gets_the_base_set_and_never_a_key_that_outranks_the_owners_login() {
        let daemon = [
            ("META_API_KEY", "k"),
            ("MUSE_NO_AUTO_UPDATE", "0"),
            ("MUSE_LAUNCHER_INSTALL", "1"),
            ("MUSE_SYNC_UPDATE", "1"),
            ("MUSE_PERMISSION_PROFILE_FAIL_OPEN", "1"),
            ("TBH_CREDENTIAL_BACKEND", "x"),
            ("XDG_CONFIG_HOME", "/c"),
            ("GITHUB_TOKEN", "g"),
        ];
        assert_eq!(kept(MUSE, &daemon), ["XDG_CONFIG_HOME"]);
        assert_eq!(for_harness(HarnessId::Muse), MUSE);
    }

    #[test]
    fn an_opencode_child_keeps_provider_keys_and_never_a_config_or_a_password_of_the_daemons() {
        let daemon = [
            ("ANTHROPIC_API_KEY", "k"),
            ("OPENCODE_CONFIG_CONTENT", "{}"),
            ("OPENCODE_CONFIG", "/c.json"),
            ("OPENCODE_PERMISSION", "{}"),
            ("OPENCODE_SERVER_PASSWORD", "p"),
            ("GITHUB_TOKEN", "g"),
            ("XDG_CONFIG_HOME", "/x"),
            ("RENO_RUN_ID", "r"),
        ];
        assert_eq!(
            kept(OPENCODE, &daemon),
            ["ANTHROPIC_API_KEY", "XDG_CONFIG_HOME"]
        );
    }

    #[test]
    fn reno_variables_never_come_from_the_daemon() {
        for harness in HarnessId::ALL {
            assert!(!allows(for_harness(harness), "RENO_RUN_ID"));
            assert!(!allows(for_harness(harness), "RENO_DATA_DIR"));
        }
    }

    #[test]
    fn a_restricted_child_sees_only_the_allowed_variables() {
        let mut cmd = std::process::Command::new("/usr/bin/env");
        cmd.env("LEAKY_PARENT_VAR", "leaked");
        cmd.env_clear();
        cmd.envs(allowed_from(
            CLAUDE,
            env(&[
                ("LEAKY_PARENT_VAR", "leaked"),
                ("HOME", "/h"),
                ("ANTHROPIC_API_KEY", "k"),
            ]),
        ));
        cmd.env("BOT_TOKEN", "t");
        let output = cmd.output().unwrap();
        let printed = String::from_utf8_lossy(&output.stdout);
        let mut lines: Vec<&str> = printed.lines().collect();
        lines.sort_unstable();
        assert_eq!(lines, ["ANTHROPIC_API_KEY=k", "BOT_TOKEN=t", "HOME=/h"]);
    }
}
