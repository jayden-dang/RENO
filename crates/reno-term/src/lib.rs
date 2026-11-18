//! The terminal emulator core with no renderer or I/O: [`emulator`] folds PTY bytes into a grid;
//! [`predict`] paints typing ahead of a remote host's echo once the host has proven it echoes.
//!
//! The state machine underneath is `alacritty_terminal` (Apache-2.0); provenance is recorded in
//! `docs/internal/UPSTREAM-ui.md`.

pub mod emulator;
pub mod predict;
