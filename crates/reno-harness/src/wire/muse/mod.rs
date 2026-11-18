//! Muse Code's Muse Session Protocol (MSP), sans-IO (ARCH-6): the frames Reno sends and reads,
//! the launch arguments, the model catalog, approvals and questions, and the normalizer from
//! session items to `ExecEvent`s, as data. Nothing here spawns, reads a file, sleeps or reads the
//! clock; the driver in `drivers::muse` does all of that, and mints the ids (a command id is a
//! UUIDv7, which needs the clock).
//!
//! MSP is JSON-RPC 2.0, one object per LF-terminated line each way, spoken by `muse serve` on
//! stdio. The reference is the schema the installed CLI exports about itself (`muse schema
//! generate-json-schema [--experimental]`), which is exact for that binary; the 1.4.23 differences
//! from the 1.4.3 wire this was imported from are in `docs/internal/UPSTREAM.md`.
//!
//! - [`frames`]: the inbound frame kinds, the one list of methods Reno may send (an allowlist, and
//!   the named list of methods that write Muse's own configuration, which no run may send).
//! - [`launch`]: `muse serve` arguments, the environment switch that stops the launcher's
//!   self-update, the run's posture (approval mode and permission profile) and what must come
//!   back for it to be believed, the session MCP servers.
//! - [`known`]: the notifications, requests and item kinds the driver knows by name.
//! - [`normalize`]: items and deltas to events, tool names to action classes, how a turn ended.
//! - [`approval`]: an approval request as a subject, the choice that answers it, and a question
//!   as `Question`s with its answer back.
//! - [`catalog`]: `model/list` to `ModelInfo`s and the effort words.
//! - [`account`]: `account/read` as a sign-in state.
pub(crate) mod account;
pub(crate) mod approval;
pub(crate) mod catalog;
pub(crate) mod frames;
pub(crate) mod known;
pub(crate) mod launch;
pub(crate) mod normalize;
