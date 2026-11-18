//! Error codes that cross the socket (ARCH-14).
//!
//! # Stability
//!
//! Codes are a wire contract. A code's number and meaning never change once released; a new
//! condition gets a new code. Codes are never reused. Clients must tolerate unknown codes, which
//! decode to [`ErrorCode::Other`].
//!
//! # Ranges
//!
//! - `-32700`, `-32600` to `-32603`: the JSON-RPC 2.0 standard errors.
//! - `-32001` to `-32099`: Reno errors, inside the range JSON-RPC 2.0 reserves for
//!   implementation-defined server errors. `-32000` is left unused.
//!
//! Internal error enums are not part of the contract; each maps to exactly one code.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A JSON-RPC error code. Serialised as its integer value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// -32700: invalid JSON was received.
    ParseError,
    /// -32600: the request is not a valid JSON-RPC request.
    InvalidRequest,
    /// -32601: the method does not exist.
    MethodNotFound,
    /// -32602: the params are invalid for the method.
    InvalidParams,
    /// -32603: JSON-RPC internal error, raised by the framing layer.
    InternalError,
    /// -32001: the named entity does not exist.
    NotFound,
    /// -32002: the request conflicts with current state, such as an illegal run transition.
    Conflict,
    /// -32003: a write carried a stale lease fence.
    StaleFence,
    /// -32004: the caller may not do this.
    Unauthorized,
    /// -32005: the method needs a feature that was not negotiated in `daemon.hello`.
    UnsupportedFeature,
    /// -32006: client and daemon share no protocol version.
    ProtocolVersionMismatch,
    /// -32007: the daemon cannot serve the request right now; retrying may succeed.
    Unavailable,
    /// -32008: an unexpected failure inside the daemon.
    Internal,
    /// -32009: a budget refuses the request: the bot has used its token ceiling for the day.
    BudgetExceeded,
    /// -32010: the executor already has an install or update job.
    JobRunning,
    /// -32011: the executor has nothing to install or update.
    NothingToDo,
    /// -32012: the executor has no updater to run.
    NoUpdater,
    /// -32013: the run's files cannot be shown: its worktree was removed, is gone, or the run
    /// has no folder.
    WorkspaceUnavailable,
    /// -32014: the path is refused by the sandbox: it climbs out of the root (`..`), is absolute,
    /// leaves the root through a symlink, or reaches into `.git`.
    PathRejected,
    /// A code this version does not know. Never produced by this crate's own servers.
    Other(i32),
}

impl ErrorCode {
    /// The integer that goes on the wire.
    pub const fn code(self) -> i32 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::InternalError => -32603,
            Self::NotFound => -32001,
            Self::Conflict => -32002,
            Self::StaleFence => -32003,
            Self::Unauthorized => -32004,
            Self::UnsupportedFeature => -32005,
            Self::ProtocolVersionMismatch => -32006,
            Self::Unavailable => -32007,
            Self::Internal => -32008,
            Self::BudgetExceeded => -32009,
            Self::JobRunning => -32010,
            Self::NothingToDo => -32011,
            Self::NoUpdater => -32012,
            Self::WorkspaceUnavailable => -32013,
            Self::PathRejected => -32014,
            Self::Other(code) => code,
        }
    }

    /// Decodes a wire integer. Unknown values become [`ErrorCode::Other`].
    pub const fn from_code(code: i32) -> Self {
        match code {
            -32700 => Self::ParseError,
            -32600 => Self::InvalidRequest,
            -32601 => Self::MethodNotFound,
            -32602 => Self::InvalidParams,
            -32603 => Self::InternalError,
            -32001 => Self::NotFound,
            -32002 => Self::Conflict,
            -32003 => Self::StaleFence,
            -32004 => Self::Unauthorized,
            -32005 => Self::UnsupportedFeature,
            -32006 => Self::ProtocolVersionMismatch,
            -32007 => Self::Unavailable,
            -32008 => Self::Internal,
            -32009 => Self::BudgetExceeded,
            -32010 => Self::JobRunning,
            -32011 => Self::NothingToDo,
            -32012 => Self::NoUpdater,
            -32013 => Self::WorkspaceUnavailable,
            -32014 => Self::PathRejected,
            other => Self::Other(other),
        }
    }
}

impl Serialize for ErrorCode {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i32(self.code())
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        i32::deserialize(d).map(Self::from_code)
    }
}

impl JsonSchema for ErrorCode {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ErrorCode")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "integer", "format": "int32" })
    }
}

/// The JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorObject {
    /// Stable error code.
    pub code: ErrorCode,
    /// Short human-readable message; not a contract.
    pub message: String,
    /// Optional structured detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl ErrorObject {
    /// An error with no `data`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ErrorCode;

    /// The released numbers. This table must only ever grow.
    const STABLE: &[(ErrorCode, i32)] = &[
        (ErrorCode::ParseError, -32700),
        (ErrorCode::InvalidRequest, -32600),
        (ErrorCode::MethodNotFound, -32601),
        (ErrorCode::InvalidParams, -32602),
        (ErrorCode::InternalError, -32603),
        (ErrorCode::NotFound, -32001),
        (ErrorCode::Conflict, -32002),
        (ErrorCode::StaleFence, -32003),
        (ErrorCode::Unauthorized, -32004),
        (ErrorCode::UnsupportedFeature, -32005),
        (ErrorCode::ProtocolVersionMismatch, -32006),
        (ErrorCode::Unavailable, -32007),
        (ErrorCode::Internal, -32008),
        (ErrorCode::BudgetExceeded, -32009),
        (ErrorCode::JobRunning, -32010),
        (ErrorCode::NothingToDo, -32011),
        (ErrorCode::NoUpdater, -32012),
        (ErrorCode::WorkspaceUnavailable, -32013),
        (ErrorCode::PathRejected, -32014),
    ];

    #[test]
    fn codes_are_stable_and_round_trip() {
        for &(code, n) in STABLE {
            assert_eq!(code.code(), n);
            assert_eq!(ErrorCode::from_code(n), code);
        }
        assert_eq!(ErrorCode::from_code(7), ErrorCode::Other(7));
        assert_eq!(ErrorCode::Other(7).code(), 7);
    }
}
