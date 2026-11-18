//! opencode's HTTP and SSE server protocol (`opencode serve`), sans-IO (ARCH-6): the requests
//! Reno makes (and the guard that refuses every other), the replies it reads, the two server
//! generations' event frames folded into one vocabulary, the permission mapping, the inline
//! config, the model catalog and the turn's end as a state machine. Nothing here spawns, reads a
//! file, sleeps or reads the clock; `drivers::opencode` does all of that.
//!
//! # Two server generations
//!
//! - **v1** is the 1.x wire: `/global/health`, `/session/*`, `prompt_async`, the bus at
//!   `/global/event` (frames wrapped `{payload: {type, properties}}`).
//! - **v2** is the 2.x wire under `/api/*`: `/api/info`, `/api/session/*`, the bus at
//!   `/api/event` (frames `{id, type, data}`), rewritten by [`v2::normalize`] into the v1
//!   `{type, properties}` payloads so [`reducer::Reducer`] has one vocabulary.
//!
//! 1.19.5 serves both: its `/api/*` routes exist (`/api/health` answers `{"healthy":true}` with no
//! version, `/api/info` the web UI), so the generation is decided by the first answer that
//! carries a version and by the CLI's own version ([`version::Protocol::for_version`]).
//!
//! # What is pinned where
//!
//! Which facts were read off a real server, and which are zeron's (68e460af, observed live on
//! 1.18.x, 2.0.x and 2.1.2), is in the module docs of `drivers::opencode`.

pub(crate) mod catalog;
pub(crate) mod config;
pub(crate) mod known;
pub(crate) mod permission;
pub(crate) mod reducer;
pub(crate) mod requests;
pub(crate) mod sse;
pub(crate) mod v2;
pub(crate) mod version;
