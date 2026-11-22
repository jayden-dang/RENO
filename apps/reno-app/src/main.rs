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
mod banners;
mod bench;
mod desktop;
mod folder_picker;
mod lifecycle;
mod links;
mod local;
mod nav;
mod notify;
mod panel;
mod prefs;
mod screens;
mod store;
mod summary;
mod terminal;
mod transcript;
mod ui;
mod vm;

#[cfg(test)]
mod activity_tests;
#[cfg(test)]
mod composer_surface_tests;
#[cfg(test)]
mod composer_tests;
#[cfg(test)]
mod draft33_tests;
#[cfg(test)]
#[cfg(test)]
mod new_thread_tests;
#[cfg(test)]
mod notify_tests;
#[cfg(test)]
mod palette_tests;
#[cfg(test)]
mod panel_tests;
#[cfg(test)]
mod question_tests;
#[cfg(test)]
mod renderers_tests;
#[cfg(test)]
mod screen_tests;
#[cfg(test)]
mod settings_pages_tests;
#[cfg(test)]
mod setup_tests;
#[cfg(test)]
mod shell_tests;
#[cfg(test)]
mod sidebar_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod thread_tests;

use std::process::ExitCode;

/// The allocator is chosen by measurement (ARCHITECTURE 17.8): the `mimalloc` feature selects v2.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[allow(clippy::print_stderr)]
fn main() -> ExitCode {
    let started = std::time::Instant::now();
    let options = match app::Options::parse(std::env::args().skip(1), |key| std::env::var(key).ok())
    {
        Ok(options) => options,
        Err(message) => {
            eprintln!("reno-app: {message}\nusage: reno-app [--fake]");
            return ExitCode::from(2);
        }
    };
    bench::init(options.bench, started);
    app::run(options);
    ExitCode::SUCCESS
}
