//! Which wire a server speaks, from the versions it and its CLI report.

use serde_json::Value;

/// The server's wire generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Protocol {
    /// 1.x: the global namespace (`/global/*`, `/session/*`, `prompt_async`).
    V1,
    /// 2.x: the `/api/*` surface.
    V2,
}

/// The health probes, newest first. A probe counts only when its answer is JSON that carries a
/// version: the web UI catch-all answers 200 HTML on routes it does not own, and 1.19.5's
/// `/api/health` answers `{"healthy":true}` with no version.
pub(crate) const PROBES: [(&str, Protocol); 4] = [
    ("/api/info", Protocol::V2),
    ("/api/status", Protocol::V2),
    ("/api/health", Protocol::V2),
    ("/global/health", Protocol::V1),
];

impl Protocol {
    /// The generation a CLI of `version` is expected to speak: 2.x and later speak v2.
    pub(crate) fn for_version(version: &semver::Version) -> Self {
        if version.major >= 2 {
            Self::V2
        } else {
            Self::V1
        }
    }

    /// The route whose answer proves the server is alive, for the bus's silence check.
    pub(crate) fn alive_path(self) -> &'static str {
        match self {
            Self::V1 => "/global/health",
            Self::V2 => "/api/info",
        }
    }

    /// The bus route.
    pub(crate) fn event_path(self) -> &'static str {
        match self {
            Self::V1 => "/global/event",
            Self::V2 => "/api/event",
        }
    }
}

/// A server's version as it reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServerVersion {
    /// The text it sent.
    pub(crate) raw: String,
    /// `(major, minor, patch)`, when the text has them.
    pub(crate) number: Option<(u64, u64, u64)>,
}

impl ServerVersion {
    /// Parse `2.3.2`, `v2.3.2`, `1.19.5-rc.1`.
    pub(crate) fn parse(raw: &str) -> Self {
        let number = (|| {
            let start = raw.find(|c: char| c.is_ascii_digit())?;
            let mut parts = raw[start..].split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            let patch = parts
                .next()?
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()?;
            Some((major, minor, patch))
        })();
        Self {
            raw: raw.to_owned(),
            number,
        }
    }

    /// Whether this is at least `(major, minor, patch)`. Unknown is no.
    pub(crate) fn at_least(&self, want: (u64, u64, u64)) -> bool {
        self.number.is_some_and(|n| n >= want)
    }
}

/// The version a health answer carries (`{"version": ..}` or `{"data": {"version": ..}}`), if
/// it is one: a probe answer without it is not a health answer.
pub(crate) fn health_version(body: &Value) -> Option<ServerVersion> {
    body.get("version")
        .and_then(Value::as_str)
        .or_else(|| body.pointer("/data/version").and_then(Value::as_str))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ServerVersion::parse)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn only_an_answer_with_a_version_is_a_health_answer() {
        assert!(health_version(&json!({"healthy": true})).is_none());
        let v1 = health_version(&json!({"healthy": true, "version": "1.19.5"})).unwrap();
        assert_eq!(v1.number, Some((1, 19, 5)));
        let v2 = health_version(&json!({"version": "2.3.2", "pid": 3})).unwrap();
        assert!(v2.at_least((2, 0, 4)));
        assert!(health_version(&json!({"data": {"version": " "}})).is_none());
        assert!(health_version(&json!({"data": {"version": "2.0.3"}})).is_some());
    }

    #[test]
    fn two_x_and_later_speak_the_api_wire() {
        assert_eq!(
            Protocol::for_version(&semver::Version::new(2, 3, 2)),
            Protocol::V2
        );
        assert_eq!(
            Protocol::for_version(&semver::Version::new(1, 19, 5)),
            Protocol::V1
        );
        assert_eq!(ServerVersion::parse("1.19.5-rc.1").number, Some((1, 19, 5)));
        assert!(!ServerVersion::parse("dev").at_least((0, 0, 0)));
    }
}
