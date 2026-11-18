//! The sans-IO layer (ARCH-6): frame types, normalizers, permission mapping and name tables.
//! Nothing here spawns a process, touches a file, sleeps or reads the clock.

pub(crate) mod acp;
pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod cursor;
pub(crate) mod muse;
pub(crate) mod pi;
