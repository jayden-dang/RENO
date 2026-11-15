//! The identity of the agent behind an ACP-style driver.
//!
//! [`HarnessId`] names the built-in agents the drivers special-case (their launch flags, session
//! quirks and install paths differ), and [`RegistryAgent`] is an agent added from the ACP
//! registry. Both are `Copy` and carry a stable slug.

/// The wire prefix of an agent added from the ACP registry: its harness
/// slug is `acp.<registry id>` (`acp.gemini`), kept apart from the built-in
/// slugs (the registry also lists agents Reno drives itself, `cursor`,
/// `devin`, …) and safe as a directory name on every platform.
pub const REGISTRY_SLUG_PREFIX: &str = "acp.";

/// An agent added from the ACP registry, by its interned slug
/// (`acp.<registry id>`). Interned so [`HarnessId`] stays `Copy` and its
/// slug `&'static`: the set is bounded by what a device has ever added or
/// been asked about (the registry lists tens of agents), so the leak is too.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RegistryAgent(&'static str);

impl RegistryAgent {
    /// The agent for a registry id (`gemini`), or `None` for one that isn't
    /// a registry id (the registry's ids are lowercase letters, digits, `-`,
    /// `_` and `.`, at most 64 long).
    pub fn new(registry_id: &str) -> Option<Self> {
        let valid = !registry_id.is_empty()
            && registry_id.len() <= 64
            && registry_id.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
            })
            && registry_id.as_bytes()[0].is_ascii_alphanumeric();
        valid.then(|| Self(intern(&format!("{REGISTRY_SLUG_PREFIX}{registry_id}"))))
    }

    /// From a harness slug (`acp.gemini`).
    pub fn from_slug(slug: &str) -> Option<Self> {
        Self::new(slug.strip_prefix(REGISTRY_SLUG_PREFIX)?)
    }

    /// The registry's own id (`gemini`).
    pub fn registry_id(self) -> &'static str {
        &self.0[REGISTRY_SLUG_PREFIX.len()..]
    }

    /// The harness slug (`acp.gemini`).
    pub fn slug(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Debug for RegistryAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

fn intern(slug: &str) -> &'static str {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SLUGS: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut slugs = SLUGS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(known) = slugs.get(slug) {
        return known;
    }
    let leaked: &'static str = Box::leak(slug.to_owned().into_boxed_str());
    slugs.insert(leaked);
    leaked
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HarnessId {
    ClaudeCode,
    Codex,
    /// Cognition's Devin agent, driven over ACP (`devin acp`).
    Devin,
    /// xAI's Grok Build agent, driven over ACP (`grok agent stdio`).
    Grok,
    /// Nous Research's Hermes Agent, driven over ACP (`hermes acp`).
    Hermes,
    /// Vercel fx, driven by its native ACP server.
    Fx,
    /// google's antigravity agent over acp (`agy_acp_server`, installed from
    /// its pinned release archive).
    Antigravity,
    /// An agent added from the ACP registry (`acp.<registry id>`), driven by
    /// the generic ACP harness from its registry entry: every registry
    /// agent goes through this one path.
    Registry(RegistryAgent),
}

impl HarnessId {
    /// Every built-in harness, in declaration order (a registry agent is
    /// one a device added: the engine's registry lists those).
    pub const ALL: [HarnessId; 7] = [
        HarnessId::ClaudeCode,
        HarnessId::Codex,
        HarnessId::Devin,
        HarnessId::Grok,
        HarnessId::Hermes,
        HarnessId::Fx,
        HarnessId::Antigravity,
    ];

    /// The wire slug (`claude-code`, `codex`, ...) — what serde writes.
    pub fn as_str(self) -> &'static str {
        match self {
            HarnessId::ClaudeCode => "claude-code",
            HarnessId::Codex => "codex",
            HarnessId::Devin => "devin",
            HarnessId::Grok => "grok",
            HarnessId::Hermes => "hermes",
            HarnessId::Fx => "fx",
            HarnessId::Antigravity => "antigravity",
            HarnessId::Registry(agent) => agent.slug(),
        }
    }

    /// Inverse of [`Self::as_str`]; `None` for a slug this build does not know.
    pub fn from_slug(slug: &str) -> Option<HarnessId> {
        if let Some(agent) = RegistryAgent::from_slug(slug) {
            return Some(HarnessId::Registry(agent));
        }
        Self::ALL.into_iter().find(|id| id.as_str() == slug)
    }

    /// The registry agent this is, if it is one.
    pub fn registry(self) -> Option<RegistryAgent> {
        match self {
            HarnessId::Registry(agent) => Some(agent),
            _ => None,
        }
    }
}

#[cfg(test)]
mod registry_slug_tests {
    use super::*;

    #[test]
    fn registry_agents_round_trip_as_namespaced_slugs() {
        let id = HarnessId::from_slug("acp.gemini").expect("a registry slug");
        assert_eq!(id.as_str(), "acp.gemini");
        assert_eq!(
            id.registry().map(RegistryAgent::registry_id),
            Some("gemini")
        );
        // Interned: the same slug is the same id, whichever way it came.
        assert_eq!(
            RegistryAgent::new("gemini").map(HarnessId::Registry),
            Some(id)
        );
        // A built-in's slug stays the built-in, never a registry agent.
        assert_eq!(HarnessId::from_slug("codex"), Some(HarnessId::Codex));
        assert_eq!(
            HarnessId::from_slug("claude-code"),
            Some(HarnessId::ClaudeCode)
        );
        // Not a registry id: refused, as an unknown slug always was.
        for bad in [
            "acp.",
            "acp.Gemini",
            "acp.../x",
            "acp.-x",
            "gemini",
            "acp.a/b",
        ] {
            assert_eq!(HarnessId::from_slug(bad), None, "{bad}");
        }
        assert_eq!(HarnessId::from_slug("nope"), None);
    }
}
