//! The wire types of the Terminal tab (RP2): `term.open`, `term.attach`, `term.write`,
//! `term.resize`, `term.close`, `term.status` and `term.list`.
//!
//! A terminal is a shell on a PTY that the daemon owns for a person. The daemon only moves bytes:
//! the app owns the emulator. Output is numbered; a bounded replay window ([`TERM_REPLAY_BYTES`])
//! lets a viewer that detached come back with `after_seq` and see what it missed. These are
//! **person-only** methods: a run's token is refused. Terminals die with the daemon; a restart
//! ends every shell, and a terminal id from before it is `NotFound`.
//!
//! # The sandbox (D7)
//!
//! A terminal opened while the run is live starts under a macOS sandbox profile that denies
//! writes to the paths the protected-path guard watches ([`crate::defs::PROTECTED_DIRS`]) and to the worktree's private git directory, so the guard's
//! premise (only the run writes there) holds by construction. `sandboxed` says so. A sandbox
//! cannot be lifted from a running process: a terminal keeps its limits when the run ends, and a
//! viewer offers "reopen without limits", which is a new `term.open` after the run ended. A
//! terminal on a live run is never opened unsandboxed: when the sandbox cannot start,
//! `term.open` fails with [`ErrorCode::SandboxUnavailable`](crate::ErrorCode).
//!
//! Terminal output is **not redacted**: it is the person's own shell. The run's secrets are not
//! in its environment, but anything the person prints is shown as it is.
//!
//! Needs feature [`FEATURE_TERMINALS`].

use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{RunId, TermId};

/// Feature: `term.open`, `term.attach`, `term.write`, `term.resize`, `term.close`,
/// `term.status` and `term.list` exist.
pub const FEATURE_TERMINALS: &str = "terminals";

/// The most terminals one daemon holds at once, exited ones that are not yet reaped included.
pub const TERM_MAX_TERMINALS: usize = 16;
/// How much output one terminal keeps for replay, in bytes of terminal output.
pub const TERM_REPLAY_BYTES: usize = 1024 * 1024;
/// The most bytes one `term.write` carries (before base64).
pub const TERM_MAX_INPUT_BYTES: usize = 64 * 1024;
/// The most bytes of output one [`TermEvent::Data`] carries (before base64).
pub const TERM_MAX_CHUNK_BYTES: usize = 64 * 1024;
/// How long a terminal whose shell exited stays listed and attachable, in seconds.
pub const TERM_EXITED_TTL_SECS: u64 = 30 * 60;
/// The widest and tallest a terminal may be, in cells; a larger size is cut to this.
pub const TERM_MAX_DIM: u16 = 1000;

/// Params of `term.open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TermOpenParams {
    /// The run whose worktree the shell starts in (its thread's worktree; for a run with none,
    /// its own folder, flagged `inPlace`).
    pub run_id: RunId,
    /// The width, in cells.
    pub cols: u16,
    /// The height, in cells.
    pub rows: u16,
    /// A command to run in the shell once it starts (an action): the shell stays open when it
    /// ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// Result of `term.open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TermOpened {
    /// The terminal.
    pub id: TermId,
    /// Where the shell started: the real path of the worktree.
    pub cwd: String,
    /// The shell's name (`zsh`, `bash`), for the tab's label.
    pub shell: String,
    /// The shell runs under the D7 sandbox: it cannot write protected paths. Opened on a live
    /// run, it keeps this for good.
    pub sandboxed: bool,
    /// The run has no worktree of its own and the shell is in its folder, which is the person's
    /// own. Never sandboxed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_place: bool,
}

/// One terminal, as `term.list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TermInfo {
    /// The terminal.
    pub id: TermId,
    /// Where the shell started.
    pub cwd: String,
    /// The shell's name.
    pub shell: String,
    /// See [`TermOpened::sandboxed`]. Still true after the run ended.
    pub sandboxed: bool,
    /// See [`TermOpened::in_place`].
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_place: bool,
    /// The shell has exited; the terminal stays for [`TERM_EXITED_TTL_SECS`] so a viewer can show
    /// its last output.
    pub exited: bool,
    /// When it was opened, in milliseconds since the Unix epoch.
    pub opened_at_ms: u64,
}

/// Params of `term.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TermListParams {
    /// The run whose worktree's terminals to list: every terminal in that tree, whichever run of
    /// the thread opened it.
    pub run_id: RunId,
}

/// Result of `term.list`, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TermListResult {
    /// The terminals.
    pub terminals: Vec<TermInfo>,
}

/// Params of the `term.attach` subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TermAttachParams {
    /// The terminal.
    pub id: TermId,
    /// The last sequence number the viewer has seen; output after it is replayed first, then
    /// live. Absent or 0: everything the replay window holds. When that is older than the window,
    /// the first item is [`TermEvent::Gap`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_seq: Option<u64>,
}

/// One item of `term.attach`. Data and exit items are numbered 1, 2, 3 ... per terminal; a `Gap`
/// carries the number of the item that follows it. The stream ends after `Exit`. Detaching
/// leaves the shell running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum TermEvent {
    /// Output of the shell. `data` is standard base64: terminal output is bytes, not UTF-8.
    Data {
        /// Its place in the terminal's output.
        seq: u64,
        /// The bytes, base64.
        data: String,
    },
    /// The shell exited.
    Exit {
        /// Its place in the terminal's output.
        seq: u64,
        /// The exit code; absent when a signal ended the shell, or the status was unreadable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        /// The signal that ended it, by name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
    },
    /// Output from `lostFrom` up to `seq - 1` is no longer held (the viewer asked from before
    /// the replay window, or fell behind it): its screen is unreliable. The item after the gap
    /// has sequence number `seq`.
    Gap {
        /// The first sequence number the viewer missed (`afterSeq + 1`).
        lost_from: u64,
        /// The sequence number of the first item that follows.
        seq: u64,
    },
}

impl TermEvent {
    /// The event's sequence number.
    pub fn seq(&self) -> u64 {
        match self {
            Self::Data { seq, .. } | Self::Exit { seq, .. } | Self::Gap { seq, .. } => *seq,
        }
    }

    /// The bytes of a [`TermEvent::Data`]. `None` for another event, or invalid base64.
    pub fn bytes(&self) -> Option<Vec<u8>> {
        match self {
            Self::Data { data, .. } => decode_bytes(data),
            _ => None,
        }
    }
}

/// Standard base64 of `bytes`, as `data` carries them.
pub fn encode_bytes(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The bytes of standard base64 `data`; `None` when it is not valid.
pub fn decode_bytes(data: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

/// Params of `term.write`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TermWriteParams {
    /// The terminal.
    pub id: TermId,
    /// The input, standard base64, at most [`TERM_MAX_INPUT_BYTES`] bytes decoded. Writes to one
    /// terminal are applied one at a time, in the order the daemon accepts them; requests of one
    /// connection are handled concurrently, so a viewer keeps one write in flight and awaits it
    /// before sending the next.
    pub data: String,
}

/// Params of `term.resize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TermResizeParams {
    /// The terminal.
    pub id: TermId,
    /// The width, in cells.
    pub cols: u16,
    /// The height, in cells.
    pub rows: u16,
}

/// Params of `term.close` and `term.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TermRef {
    /// The terminal.
    pub id: TermId,
}

/// Result of `term.write`, `term.resize` and `term.close`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TermDone {}

/// Result of `term.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TermStatusResult {
    /// A program other than the shell is in the foreground, so closing the terminal would end
    /// it.
    pub busy: bool,
    /// The shell has exited.
    pub exited: bool,
    /// See [`TermOpened::sandboxed`].
    pub sandboxed: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn events_are_tagged_and_carry_base64() {
        let data = TermEvent::Data {
            seq: 3,
            data: encode_bytes(b"hi\x1b[0m"),
        };
        let text = serde_json::to_string(&data).unwrap();
        assert_eq!(text, r#"{"type":"data","seq":3,"data":"aGkbWzBt"}"#);
        assert_eq!(data.bytes().as_deref(), Some(&b"hi\x1b[0m"[..]));
        let exit = TermEvent::Exit {
            seq: 4,
            code: Some(0),
            signal: None,
        };
        assert_eq!(
            serde_json::to_string(&exit).unwrap(),
            r#"{"type":"exit","seq":4,"code":0}"#
        );
        let gap: TermEvent =
            serde_json::from_str(r#"{"type":"gap","lostFrom":2,"seq":9}"#).unwrap();
        assert_eq!(gap.seq(), 9);
    }

    #[test]
    fn absent_optionals_decode() {
        let params: TermAttachParams = serde_json::from_str(r#"{"id":"term_a1"}"#).unwrap();
        assert_eq!(params.after_seq, None);
        let open: TermOpenParams =
            serde_json::from_str(r#"{"runId":"run_a","cols":80,"rows":24}"#).unwrap();
        assert_eq!(open.command, None);
    }
}
