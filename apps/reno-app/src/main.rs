//! Reno's desktop app: the shell and the Home screen on GPUI.
//!
//! `reno-app --fake` runs an in-process fake daemon. With no flag the app looks for the daemon
//! socket and, until `renod` exists, shows a "daemon not running" Home.

mod actions;
mod app;
mod nav;
mod route;
mod screens;
mod store;
mod summary;
mod transcript;
mod ui;

#[cfg(test)]
mod tests;

use std::process::ExitCode;

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
