//! The I/O shells: each driver spawns its harness, pumps stdin and stdout, and delegates frame
//! decoding to `crate::wire`.

pub mod acp;
pub mod claude;
pub mod codex;
pub mod opencode;
pub mod pi;
