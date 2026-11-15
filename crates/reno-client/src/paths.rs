//! Where the daemon keeps its files by default, so the app, the CLI and `renod` agree.
//!
//! - macOS: `~/Library/Application Support/Reno`
//! - elsewhere: `$XDG_DATA_HOME/reno`, or `~/.local/share/reno` when that is unset
//!
//! `RENO_DATA_DIR` overrides the directory and `RENO_SOCKET` the socket path. The socket is
//! `reno.sock` inside the data directory unless `RENO_SOCKET` says otherwise.

use std::ffi::OsString;
use std::path::PathBuf;

/// The environment variable that overrides the data directory.
pub const DATA_DIR_ENV: &str = "RENO_DATA_DIR";
/// The environment variable that overrides the socket path.
pub const SOCKET_ENV: &str = "RENO_SOCKET";

/// The socket's file name inside the data directory.
pub const SOCKET_FILE: &str = "reno.sock";

/// The daemon's data directory, or `None` when `HOME` is not set and nothing overrides it.
pub fn default_data_dir() -> Option<PathBuf> {
    data_dir_from(|key| std::env::var_os(key))
}

/// The daemon's socket path (see the module docs), or `None` when it cannot be worked out.
pub fn default_socket_path() -> Option<PathBuf> {
    socket_path_from(|key| std::env::var_os(key))
}

fn non_empty(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn data_dir_from(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(dir) = non_empty(env(DATA_DIR_ENV)) {
        return Some(dir);
    }
    let home = non_empty(env("HOME"));
    if cfg!(target_os = "macos") {
        return home.map(|home| home.join("Library/Application Support/Reno"));
    }
    match non_empty(env("XDG_DATA_HOME")) {
        Some(xdg) => Some(xdg.join("reno")),
        None => home.map(|home| home.join(".local/share/reno")),
    }
}

fn socket_path_from(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    non_empty(env(SOCKET_ENV)).or_else(|| data_dir_from(&env).map(|dir| dir.join(SOCKET_FILE)))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<_, _> = pairs.iter().copied().collect();
        move |key| map.get(key).map(OsString::from)
    }

    #[test]
    fn the_data_dir_override_wins() {
        let vars = env(&[(DATA_DIR_ENV, "/tmp/r"), ("HOME", "/home/u")]);
        assert_eq!(data_dir_from(&vars), Some(PathBuf::from("/tmp/r")));
        assert_eq!(
            socket_path_from(&vars),
            Some(PathBuf::from("/tmp/r/reno.sock"))
        );
    }

    #[test]
    fn the_socket_override_wins_over_the_data_dir() {
        let vars = env(&[(SOCKET_ENV, "/run/x.sock"), (DATA_DIR_ENV, "/tmp/r")]);
        assert_eq!(socket_path_from(&vars), Some(PathBuf::from("/run/x.sock")));
    }

    #[test]
    fn empty_values_count_as_unset() {
        let vars = env(&[(DATA_DIR_ENV, ""), (SOCKET_ENV, "")]);
        assert_eq!(data_dir_from(&vars), None);
        assert_eq!(socket_path_from(&vars), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_application_support() {
        let vars = env(&[("HOME", "/Users/u"), ("XDG_DATA_HOME", "/ignored")]);
        assert_eq!(
            data_dir_from(&vars),
            Some(PathBuf::from("/Users/u/Library/Application Support/Reno"))
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_uses_xdg_then_the_home_fallback() {
        let xdg = env(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "/data")]);
        assert_eq!(data_dir_from(&xdg), Some(PathBuf::from("/data/reno")));
        let home = env(&[("HOME", "/home/u")]);
        assert_eq!(
            data_dir_from(&home),
            Some(PathBuf::from("/home/u/.local/share/reno"))
        );
    }
}
