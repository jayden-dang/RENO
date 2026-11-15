//! The only crate that knows a coding-agent harness: its executors, drivers, process handling
//! and wire formats.
//!
//! Owns ARCH-6: the `wire/` modules are sans-IO. They turn bytes or frames into events and never
//! spawn processes, touch files, sleep or read the clock. Owns ARCH-15: an executor that cannot
//! honour the requested access mode is ineligible and is never run with more access than asked.
