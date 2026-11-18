//! Pi's JSONL RPC (`pi --mode rpc`), sans-IO (ARCH-6): the command frames Reno sends, the
//! reply and `get_state` shapes, the model catalog, the event normalizer and the extension
//! dialogs as data. Nothing here spawns, reads a file, sleeps or reads the clock; the driver in
//! `drivers::pi` does all of that.
//!
//! The wire is JSONL, not JSON-RPC: one JSON object per LF-terminated line each way. Replies and
//! session events share one ordered stdout channel, which is what lets the driver settle a run
//! without a timer. The reference is Pi's own `docs/rpc.md`, `rpc-commands.md` and `json.md`;
//! the 1.5.0 differences from the 0.85 to 0.87 wire this was imported from are in
//! `docs/internal/UPSTREAM.md`.

pub(crate) mod catalog;
pub(crate) mod dialog;
pub(crate) mod frames;
pub(crate) mod known;
pub(crate) mod normalize;
