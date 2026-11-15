//! The vocabulary every other crate shares: ids, domain types, `ExecEvent` and the typed
//! protocol (method, notification and subscription traits, `PROTOCOL_VERSION`, error codes).
//!
//! Owns ARCH-1: it sits at the bottom of the dependency graph and depends on no other Reno crate.
//! It does no I/O and has no async runtime. Owns ARCH-13: protocol types generate the schema that
//! CI compares with the committed snapshot, and changes within a version are additive only.
