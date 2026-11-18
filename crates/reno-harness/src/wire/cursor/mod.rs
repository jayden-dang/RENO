//! The wire to Cursor's SDK shim, sans-IO (ARCH-6): the frames Reno sends it, the frames it
//! sends back and what they mean as [`reno_proto::ExecEvent`]s, the model catalog, the stored
//! login's shape and the session ids. Nothing here spawns, reads a file, sleeps or reads the
//! clock; `drivers::cursor` does all of that.
//!
//! There is no Cursor wire to speak. The `@cursor/sdk` package is Cursor's agent runtime
//! bundled in-process, speaking Cursor's own protocol to its backend, so Reno owns a small Node
//! program over the pinned SDK (`drivers/cursor/shim.mjs`) and talks to that, one JSON object
//! per line each way. Everything in this module is that shim's protocol, not Cursor's.

pub(crate) mod auth;
pub(crate) mod catalog;
pub(crate) mod frames;
pub(crate) mod known;
pub(crate) mod normalize;
pub(crate) mod state;
