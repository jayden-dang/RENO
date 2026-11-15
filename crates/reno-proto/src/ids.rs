//! Identifier newtypes.
//!
//! # Id format
//!
//! Every id except [`ExecutorId`] and [`Seq`] is a string `<prefix>_<suffix>`: a fixed
//! lowercase prefix per type (`run_`, `task_`, `bot_`, ...) and a suffix of 1 to 64 ASCII
//! letters or digits. The prefix makes ids self-describing in logs and rejects a `TaskId` passed
//! where a `RunId` is expected. This crate never mints ids (it has no entropy or clock); the
//! daemon picks the suffix, for example a ULID. Parsing and deserialising validate the shape.
//!
//! [`ExecutorId`] names an installed harness (`claude-code`, `codex`, `acp.grok`), so it is a
//! slug of 1 to 64 characters from `[A-Za-z0-9._-]` with no prefix. [`Seq`] is a plain `u64`.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const MAX_SUFFIX: usize = 64;

/// An id string that does not match the format of its type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} {input:?}: expected {expected}")]
pub struct IdError {
    kind: &'static str,
    input: String,
    expected: &'static str,
}

fn is_suffix(s: &str) -> bool {
    (1..=MAX_SUFFIX).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn has_prefix(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
        .is_some_and(is_suffix)
}

fn is_slug(s: &str) -> bool {
    (1..=MAX_SUFFIX).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident, $check:expr, $pattern:expr, $expected:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// The id as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, IdError> {
                if ($check)(s) {
                    Ok(Self(s.to_owned()))
                } else {
                    Err(IdError {
                        kind: stringify!($name),
                        input: s.to_owned(),
                        expected: $expected,
                    })
                }
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(s: String) -> Result<Self, IdError> {
                s.parse()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed(stringify!($name))
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({ "type": "string", "pattern": $pattern })
            }
        }
    };
}

macro_rules! prefixed_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        string_id!(
            $(#[$meta])*
            $name,
            |s: &str| has_prefix(s, $prefix),
            concat!("^", $prefix, "_[A-Za-z0-9]{1,64}$"),
            concat!("`", $prefix, "_` followed by 1 to 64 ASCII letters or digits")
        );

        impl $name {
            /// Builds an id from its suffix, adding the type's prefix.
            pub fn from_suffix(suffix: &str) -> Result<Self, IdError> {
                format!("{}_{suffix}", $prefix).parse()
            }
        }
    };
}

prefixed_id!(
    /// One attempt at a task by one executor session (`run_...`).
    RunId, "run"
);
prefixed_id!(
    /// A unit of work (`task_...`).
    TaskId, "task"
);
prefixed_id!(
    /// A named bot identity (`bot_...`).
    BotId, "bot"
);
prefixed_id!(
    /// A team of bots (`team_...`).
    TeamId, "team"
);
prefixed_id!(
    /// A versioned flow definition (`flow_...`).
    FlowId, "flow"
);
prefixed_id!(
    /// One instance of a flow (`flowrun_...`).
    FlowRunId, "flowrun"
);
prefixed_id!(
    /// A node inside a flow (`node_...`).
    NodeId, "node"
);
prefixed_id!(
    /// A message envelope (`msg_...`).
    MessageId, "msg"
);
prefixed_id!(
    /// A stored approval request (`appr_...`).
    ApprovalId, "appr"
);
prefixed_id!(
    /// A candidate change to a definition (`prop_...`).
    ProposalId, "prop"
);
prefixed_id!(
    /// A live subscription on one connection (`sub_...`).
    SubscriptionId, "sub"
);

string_id!(
    /// An installed harness, such as `claude-code`, `codex` or `acp.grok`. A slug, not prefixed.
    ExecutorId,
    is_slug,
    "^[A-Za-z0-9._-]{1,64}$",
    "1 to 64 characters from A-Z a-z 0-9 . _ -"
);

/// A per-run event sequence number. A run's journal numbers its events from 1 without gaps; a
/// `from` of `Seq(0)` therefore means "from the start".
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct Seq(pub u64);

impl Seq {
    /// The sequence number after this one, or `None` on overflow.
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for Seq {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixed_ids_round_trip() {
        let id: RunId = "run_01HZX9".parse().unwrap();
        assert_eq!(id.to_string(), "run_01HZX9");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"run_01HZX9\"");
        let back: RunId = serde_json::from_str("\"run_01HZX9\"").unwrap();
        assert_eq!(back, id);
        assert_eq!(RunId::from_suffix("01HZX9").unwrap(), id);
    }

    #[test]
    fn wrong_prefix_and_bad_shapes_are_rejected() {
        assert!("task_abc".parse::<RunId>().is_err());
        assert!("run_".parse::<RunId>().is_err());
        assert!("run".parse::<RunId>().is_err());
        assert!("run_a-b".parse::<RunId>().is_err());
        assert!("runx_abc".parse::<RunId>().is_err());
        assert!(format!("run_{}", "a".repeat(65)).parse::<RunId>().is_err());
        assert!(serde_json::from_str::<RunId>("\"nope\"").is_err());
        assert!(serde_json::from_str::<RunId>("7").is_err());
    }

    #[test]
    fn executor_ids_are_slugs() {
        for ok in ["claude-code", "codex", "acp.grok", "mock"] {
            assert_eq!(ok.parse::<ExecutorId>().unwrap().as_str(), ok);
        }
        for bad in ["", "has space", "a/b", &"a".repeat(65)] {
            assert!(bad.parse::<ExecutorId>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn seq_is_a_bare_number() {
        assert_eq!(serde_json::to_string(&Seq(7)).unwrap(), "7");
        assert_eq!("7".parse::<Seq>().unwrap(), Seq(7));
        assert_eq!(Seq(7).next(), Some(Seq(8)));
        assert_eq!(Seq(u64::MAX).next(), None);
    }
}
