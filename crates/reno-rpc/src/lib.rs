//! The only crate that knows JSON-RPC framing: ndjson over a Unix socket, the id-to-reply map,
//! subscriptions with resume, and an in-memory transport for tests.
//!
//! Owns ARCH-4 (framing half): no other crate parses or writes JSON-RPC frames. Owns ARCH-12: a
//! slow reader never costs unbounded memory, because subscriptions are bounded per client.
