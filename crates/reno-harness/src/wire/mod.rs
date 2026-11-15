//! The sans-IO layer (ARCH-6): frame types, normalizers, permission mapping and name tables.
//! Nothing here spawns a process, touches a file, sleeps or reads the clock.

pub mod acp;
pub mod claude;
pub mod codex;
