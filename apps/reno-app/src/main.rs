//! Reno's desktop app: the shell and the Home screen on GPUI.
//!
//! `reno-app --fake` runs an in-process fake daemon. With no flag the app connects to the real
//! daemon's socket and, when nothing answers, starts one (detached, or through launchd when a
//! LaunchAgent is installed) on a 1 s to 60 s backoff, saying why on Home when it will not stay
//! up. `RENO_NO_AUTOSTART=1` turns that off, and Home then says "Daemon not running — run
//! `reno daemon start`".

mod actions;
mod app;
mod attach;
mod local;
mod nav;
mod screens;
mod store;
mod summary;
mod transcript;
mod ui;
mod vm;

#[cfg(test)]
mod composer_tests;
#[cfg(test)]
mod screen_tests;
#[cfg(test)]
mod tests;

use std::process::ExitCode;

/// The allocator is chosen by measurement (ARCHITECTURE 17.8): the `mimalloc` feature selects v2.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[allow(clippy::print_stderr)]
fn main() -> ExitCode {
    let options = match app::Options::parse(std::env::args().skip(1), |key| std::env::var(key).ok())
    {
        Ok(options) => options,
        Err(message) => {
            eprintln!("reno-app: {message}\nusage: reno-app [--fake]");
            return ExitCode::from(2);
        }
    };
    app::run(options);
    ExitCode::SUCCESS
}
