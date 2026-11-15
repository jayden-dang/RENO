//! The only crate that knows SQL: the SQLite bindings, migrations, the writer thread, the read
//! pool, run journals and artifacts.
//!
//! Owns ARCH-5: no other crate depends on the SQLite bindings. Owns ARCH-8: SQLite is the only
//! source of truth, and every state change commits in the same transaction as the event that
//! records it. Owns ARCH-9: claims return fences, and fenced writes are checked here.
