//! The typed client over `reno-rpc`, plus a GPUI-free state mirror with a pure `apply(event)`.
//!
//! Owns ARCH-2: the app and the CLI know the daemon only through this crate, and never link the
//! kernel, the store or the harness layer. Every feature the app has, the CLI can reach through it.
//! Its consumers are the CLI, the MCP server and the app (ARCH-3).
