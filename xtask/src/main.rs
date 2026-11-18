//! `cargo xtask check`: asserts the architecture rules that the compiler cannot.
//!
//! Sans-IO layout: [`run_cargo_metadata`] and [`rust_sources`] are the only functions that touch
//! the machine. Everything else takes parsed data and returns [`Violation`]s.
//!
//! Rules:
//! - `edges`: workspace members depend on each other only along the ARCH-1 allow-list.
//! - `ownership`: ARCH-5 and friends; some external packages belong to one crate only.
//! - `wire`: ARCH-6; `reno-harness/src/wire/` contains no I/O, clock or async tokens.
//! - `kernel`: ARCH-7; `reno-kernel/src/` outside test code touches no file, process, socket or
//!   clock. `SystemTime::now` is allowed in exactly one file, the `Clock` port.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde::Deserialize;

mod bench;
mod bundle;
mod conformance;
mod live;
mod schema_diff;
mod stats;

/// Allowed normal and build dependencies between workspace members (ARCH-1).
/// A member that is not listed is itself a violation.
const ALLOWED_EDGES: &[(&str, &[&str])] = &[
    ("reno-proto", &[]),
    ("reno-store", &["reno-proto"]),
    ("reno-harness", &["reno-proto"]),
    ("reno-rpc", &["reno-proto"]),
    ("reno-kernel", &["reno-proto", "reno-store", "reno-harness"]),
    ("reno-client", &["reno-proto", "reno-rpc"]),
    // `renod` is the composition root: it opens the store and builds the executors it hands the
    // kernel, so it names `reno-store` and `reno-harness` too.
    (
        "renod",
        &[
            "reno-proto",
            "reno-store",
            "reno-harness",
            "reno-kernel",
            "reno-rpc",
        ],
    ),
    ("reno", &["reno-proto", "reno-client"]),
    (
        "reno-app",
        &[
            "reno-proto",
            "reno-client",
            "reno-markdown",
            "reno-syntax",
            "reno-notify",
        ],
    ),
    // The native macOS banner and Dock badge layer; names no workspace crate.
    ("reno-notify", &[]),
    ("reno-markdown", &[]),
    ("reno-syntax", &[]),
];

/// Extra dev-dependency edges, as `(member, dependency)`.
/// `reno-harness` names itself so its own tests run with the `conformance` feature on.
const ALLOWED_DEV_EDGES: &[(&str, &str)] =
    &[("renod", "reno-client"), ("reno-harness", "reno-harness")];

/// Members outside the layering.
const EXEMPT: &[&str] = &["xtask"];

/// External packages and the members allowed to depend on them directly.
/// A trailing `*` matches a name prefix.
///
/// `notify` is the daemon's (it watches definitions and `fs` triggers) and also the CLI's, for
/// `reno daemon logs -f`, which follows a log file by file events instead of polling it.
const OWNERS: &[(&str, &[&str])] = &[
    ("rusqlite", &["reno-store"]),
    ("libsqlite3-sys", &["reno-store"]),
    ("gpui*", &["reno-app"]),
    ("notify", &["renod", "reno"]),
    ("security-framework", &["renod"]),
    // The Objective-C bridge (and its framework bindings) is reno-notify's alone: AppKit calls
    // for banners and the Dock badge stay behind its safe API.
    ("objc2*", &["reno-notify"]),
    // Only the CLI speaks MCP (ARCH-4).
    ("rmcp", &["reno"]),
];

/// Packages that some members must never use.
const FORBIDDEN: &[(&str, &str)] = &[("tokio", "reno-proto")];

/// Tokens that `wire/` sources must not contain (ARCH-6).
const WIRE_TOKENS: &[&str] = &[
    "tokio::",
    "std::fs",
    "std::process",
    "Instant::now",
    "SystemTime::now",
    "Utc::now",
    "Local::now",
    ".await",
];

/// Tokens that the kernel's non-test sources must not contain (ARCH-7). Time goes through
/// `Clock` (and `tokio::time`, which tests pause); everything else goes through the store and
/// the executor port.
const KERNEL_TOKENS: &[&str] = &[
    "std::fs",
    "tokio::fs",
    "std::process",
    "tokio::process",
    "std::net",
    "tokio::net",
    "Instant::now",
    "Utc::now",
];

/// Forbidden everywhere in the kernel except in [`KERNEL_CLOCK_MODULE`], the `SystemClock`.
const KERNEL_CLOCK_TOKEN: &str = "SystemTime::now";

/// The one kernel source that may read the system clock, matched by path components.
const KERNEL_CLOCK_MODULE: &str = "reno-kernel/src/clock.rs";

#[derive(Debug, Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
    workspace_root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Package {
    id: String,
    name: String,
    dependencies: Vec<Dependency>,
}

#[derive(Debug, Deserialize)]
struct Dependency {
    name: String,
    /// `None` for normal dependencies, otherwise `"dev"` or `"build"`.
    kind: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct Violation {
    rule: &'static str,
    krate: String,
    detail: String,
}

impl Violation {
    fn new(rule: &'static str, krate: &str, detail: String) -> Self {
        Self {
            rule,
            krate: krate.to_owned(),
            detail,
        }
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} -> {}", self.rule, self.krate, self.detail)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("check") => check(),
        Some("bundle") => bundle::run(&args[1..]),
        Some("bench") => bench::run(&args[1..]),
        Some("conformance-live") => live::run(&args[1..]),
        Some("codex-schema-diff") => schema_diff::run(&args[1..]),
        _ => {
            eprintln!(
                "usage: cargo xtask check | {}\n       {}\n       {}\n       {}",
                &bundle::USAGE["usage: ".len()..],
                &bench::USAGE["usage: ".len()..],
                &live::USAGE["usage: ".len()..],
                &schema_diff::USAGE["usage: ".len()..]
            );
            ExitCode::from(2)
        }
    }
}

fn check() -> ExitCode {
    let metadata = match run_cargo_metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("xtask: {error}");
            return ExitCode::from(2);
        }
    };
    let mut violations = edge_violations(&metadata);
    violations.extend(ownership_violations(&metadata));
    let wire_dir = metadata.workspace_root.join("crates/reno-harness/src/wire");
    let kernel_dir = metadata.workspace_root.join("crates/reno-kernel/src");
    for (dir, is_wire) in [(&wire_dir, true), (&kernel_dir, false)] {
        match rust_sources(dir) {
            Ok(sources) => {
                for (path, source) in sources {
                    violations.extend(if is_wire {
                        wire_violations(&path, &source)
                    } else {
                        kernel_violations(&path, &source)
                    });
                }
            }
            Err(error) => {
                eprintln!("xtask: reading {}: {error}", dir.display());
                return ExitCode::from(2);
            }
        }
    }
    for violation in &violations {
        println!("{violation}");
    }
    if violations.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// I/O: runs `cargo metadata --no-deps` (workspace members and their declared dependencies only;
/// no resolution, no network) and parses its output.
fn run_cargo_metadata() -> Result<Metadata, String> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|error| format!("cannot run cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|error| format!("bad metadata: {error}"))
}

/// I/O: every `.rs` file under `dir`, as `(path, contents)`. Empty when `dir` does not exist.
fn rust_sources(dir: &Path) -> std::io::Result<Vec<(PathBuf, String)>> {
    let mut found = Vec::new();
    if !dir.exists() {
        return Ok(found);
    }
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source = fs::read_to_string(&path)?;
                found.push((path, source));
            }
        }
    }
    found.sort();
    Ok(found)
}

fn members(metadata: &Metadata) -> impl Iterator<Item = &Package> {
    metadata
        .packages
        .iter()
        .filter(|package| metadata.workspace_members.contains(&package.id))
}

fn is_member(metadata: &Metadata, name: &str) -> bool {
    members(metadata).any(|package| package.name == name)
}

/// Rule `edges`: internal dependencies follow the allow-list.
fn edge_violations(metadata: &Metadata) -> Vec<Violation> {
    let mut violations = Vec::new();
    for package in members(metadata) {
        let name = package.name.as_str();
        if EXEMPT.contains(&name) {
            continue;
        }
        let Some((_, allowed)) = ALLOWED_EDGES.iter().find(|(member, _)| *member == name) else {
            violations.push(Violation::new(
                "edges",
                name,
                "not in the allow-list; add it to ALLOWED_EDGES deliberately".to_owned(),
            ));
            continue;
        };
        for dependency in &package.dependencies {
            if !is_member(metadata, &dependency.name) {
                continue;
            }
            let dev = dependency.kind.as_deref() == Some("dev");
            let permitted = allowed.contains(&dependency.name.as_str())
                || (dev
                    && ALLOWED_DEV_EDGES
                        .iter()
                        .any(|(from, to)| *from == name && *to == dependency.name));
            if !permitted {
                let kind = if dev { "dev-dependency" } else { "dependency" };
                violations.push(Violation::new(
                    "edges",
                    name,
                    format!("forbidden {kind} on {}", dependency.name),
                ));
            }
        }
    }
    violations
}

/// Rule `ownership`: some external packages belong to one crate, some are banned from one.
fn ownership_violations(metadata: &Metadata) -> Vec<Violation> {
    let mut violations = Vec::new();
    for package in members(metadata) {
        let name = package.name.as_str();
        for dependency in &package.dependencies {
            let used = dependency.name.as_str();
            for (pattern, owners) in OWNERS {
                if pattern_matches(pattern, used) && !owners.contains(&name) {
                    violations.push(Violation::new(
                        "ownership",
                        name,
                        format!("{used} is only allowed in {}", owners.join(", ")),
                    ));
                }
            }
            for (banned, member) in FORBIDDEN {
                if used == *banned && name == *member {
                    violations.push(Violation::new(
                        "ownership",
                        name,
                        format!("{used} is not allowed here"),
                    ));
                }
            }
        }
    }
    violations
}

fn pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

/// Rule `wire`: forbidden tokens in one `wire/` source file.
fn wire_violations(path: &Path, source: &str) -> Vec<Violation> {
    scan_tokens("wire", "reno-harness", path, source, WIRE_TOKENS)
}

/// Rule `kernel`: forbidden tokens in one `reno-kernel/src` source file.
///
/// A file that opens with the inner attribute `#![cfg(test)]` (after blank lines and comments)
/// is compiled only in tests and is skipped whole: `#[cfg(test)] mod tests;` in its parent names
/// no braces for the scan to count. `SystemTime::now` is allowed in the clock module only,
/// decided by path.
fn kernel_violations(path: &Path, source: &str) -> Vec<Violation> {
    if is_test_only_file(source) {
        return Vec::new();
    }
    let mut violations = scan_tokens("kernel", "reno-kernel", path, source, KERNEL_TOKENS);
    if !path.ends_with(KERNEL_CLOCK_MODULE) {
        violations.extend(scan_tokens(
            "kernel",
            "reno-kernel",
            path,
            source,
            &[KERNEL_CLOCK_TOKEN],
        ));
    }
    violations
}

/// Whether the first item of `source` is the inner attribute `#![cfg(test)]`.
fn is_test_only_file(source: &str) -> bool {
    source
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("//"))
        == Some("#![cfg(test)]")
}

/// The line scan behind the `wire` and `kernel` rules.
///
/// Approach: a line scan, not a parser. Text after `//` is ignored, so comments and doc comments
/// may name the tokens. An item that follows `#[cfg(test)]` is skipped by counting braces from its
/// first `{` to the matching `}`; this assumes braces do not appear in string literals there.
fn scan_tokens(
    rule: &'static str,
    krate: &str,
    path: &Path,
    source: &str,
    tokens: &[&str],
) -> Vec<Violation> {
    let mut violations = Vec::new();
    let mut skip_depth: Option<i32> = None;
    let mut pending_cfg_test = false;
    for (index, line) in source.lines().enumerate() {
        let code = line.split("//").next().unwrap_or_default();
        if let Some(depth) = skip_depth.as_mut() {
            *depth += brace_delta(code);
            if *depth <= 0 {
                skip_depth = None;
            }
            continue;
        }
        if code.trim() == "#[cfg(test)]" {
            pending_cfg_test = true;
            continue;
        }
        if pending_cfg_test {
            let delta = brace_delta(code);
            if delta > 0 {
                skip_depth = Some(delta);
            }
            if delta > 0 || code.contains(';') {
                pending_cfg_test = false;
            }
            continue;
        }
        for token in tokens {
            if code.contains(token) {
                violations.push(Violation::new(
                    rule,
                    krate,
                    format!("{}:{} contains `{token}`", path.display(), index + 1),
                ));
            }
        }
    }
    violations
}

fn brace_delta(code: &str) -> i32 {
    code.chars().fold(0, |depth, ch| match ch {
        '{' => depth + 1,
        '}' => depth - 1,
        _ => depth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str, dependencies: &[(&str, Option<&str>)]) -> Package {
        Package {
            id: name.to_owned(),
            name: name.to_owned(),
            dependencies: dependencies
                .iter()
                .map(|(name, kind)| Dependency {
                    name: (*name).to_owned(),
                    kind: kind.map(str::to_owned),
                })
                .collect(),
        }
    }

    fn metadata(packages: Vec<Package>) -> Metadata {
        Metadata {
            workspace_members: packages.iter().map(|p| p.id.clone()).collect(),
            packages,
            workspace_root: PathBuf::new(),
        }
    }

    fn rendered(violations: &[Violation]) -> Vec<String> {
        violations.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn allowed_edges_pass() {
        let m = metadata(vec![
            package("reno-proto", &[]),
            package("reno-store", &[("reno-proto", None), ("rusqlite", None)]),
        ]);
        assert!(edge_violations(&m).is_empty());
        assert!(ownership_violations(&m).is_empty());
    }

    #[test]
    fn forbidden_edge_is_reported() {
        let m = metadata(vec![
            package("reno-proto", &[("reno-store", None)]),
            package("reno-store", &[]),
        ]);
        assert_eq!(
            rendered(&edge_violations(&m)),
            ["edges: reno-proto -> forbidden dependency on reno-store"]
        );
    }

    #[test]
    fn build_dependency_follows_the_same_list() {
        let m = metadata(vec![
            package("reno-proto", &[]),
            package("reno-store", &[]),
            package("reno-rpc", &[("reno-store", Some("build"))]),
        ]);
        assert_eq!(edge_violations(&m).len(), 1);
    }

    #[test]
    fn dev_edge_is_allowed_only_for_renod_on_reno_client() {
        let m = metadata(vec![
            package("reno-client", &[]),
            package("renod", &[("reno-client", Some("dev"))]),
            package("reno-kernel", &[("reno-client", Some("dev"))]),
        ]);
        assert_eq!(
            rendered(&edge_violations(&m)),
            ["edges: reno-kernel -> forbidden dev-dependency on reno-client"]
        );
    }

    #[test]
    fn renod_composes_the_store_and_the_harness_but_the_cli_and_app_never_do() {
        let composed = [
            "reno-proto",
            "reno-store",
            "reno-harness",
            "reno-kernel",
            "reno-rpc",
        ];
        let mut packages: Vec<Package> = composed.iter().map(|name| package(name, &[])).collect();
        packages.push(package("renod", &composed.map(|name| (name, None))));
        packages.push(package("reno-client", &[]));
        assert!(edge_violations(&metadata(packages)).is_empty());

        // ARCH-2: the CLI and the app reach the daemon only through `reno-client`.
        for front in ["reno", "reno-app"] {
            for banned in ["reno-kernel", "reno-store", "reno-harness"] {
                let m = metadata(vec![
                    package(banned, &[]),
                    package(front, &[(banned, None)]),
                ]);
                assert_eq!(edge_violations(&m).len(), 1, "{front} -> {banned}");
            }
        }
    }

    #[test]
    fn normal_edge_on_reno_client_in_renod_is_forbidden() {
        let m = metadata(vec![
            package("reno-client", &[]),
            package("renod", &[("reno-client", None)]),
        ]);
        assert_eq!(edge_violations(&m).len(), 1);
    }

    #[test]
    fn unlisted_member_is_a_violation_but_xtask_is_exempt() {
        let m = metadata(vec![
            package("reno-new", &[]),
            package("xtask", &[("serde", None)]),
        ]);
        assert_eq!(
            rendered(&edge_violations(&m)),
            ["edges: reno-new -> not in the allow-list; add it to ALLOWED_EDGES deliberately"]
        );
    }

    #[test]
    fn external_ownership_is_enforced() {
        let m = metadata(vec![
            package("reno-kernel", &[("rusqlite", None), ("gpui_tokio", None)]),
            package("reno-app", &[("gpui", None)]),
            package("reno-proto", &[("tokio", None)]),
            package("renod", &[("notify", None), ("security-framework", None)]),
            package("reno", &[("notify", None)]),
            package("reno-client", &[("notify", None)]),
        ]);
        assert_eq!(
            rendered(&ownership_violations(&m)),
            [
                "ownership: reno-kernel -> rusqlite is only allowed in reno-store",
                "ownership: reno-kernel -> gpui_tokio is only allowed in reno-app",
                "ownership: reno-proto -> tokio is not allowed here",
                "ownership: reno-client -> notify is only allowed in renod, reno",
            ]
        );
    }

    #[test]
    fn dependencies_outside_the_workspace_are_not_edges() {
        let m = metadata(vec![package("reno-proto", &[("serde", None)])]);
        assert!(edge_violations(&m).is_empty());
    }

    #[test]
    fn wire_tokens_are_reported_with_location() {
        let source = "fn a() {\n    let t = std::time::Instant::now();\n    f().await;\n}\n";
        let found = rendered(&wire_violations(Path::new("w.rs"), source));
        assert_eq!(
            found,
            [
                "wire: reno-harness -> w.rs:2 contains `Instant::now`",
                "wire: reno-harness -> w.rs:3 contains `.await`",
            ]
        );
    }

    #[test]
    fn wire_ignores_comments_and_cfg_test_modules() {
        let source = "\
//! Never calls std::fs.
fn a() {} // not tokio::spawn
#[cfg(test)]
mod tests {
    fn t() {
        let _ = std::fs::read(\"x\");
    }
}
fn b() {}
";
        assert!(wire_violations(Path::new("w.rs"), source).is_empty());
    }

    #[test]
    fn wire_checks_code_after_a_cfg_test_module() {
        let source = "#[cfg(test)]\nmod tests {\n}\nuse std::process::Command;\n";
        assert_eq!(wire_violations(Path::new("w.rs"), source).len(), 1);
    }

    #[test]
    fn missing_wire_directory_yields_no_sources() {
        let sources = rust_sources(Path::new("/nonexistent/reno/wire"));
        assert!(sources.is_ok_and(|found| found.is_empty()));
    }

    fn kernel(path: &str, source: &str) -> Vec<String> {
        rendered(&kernel_violations(Path::new(path), source))
    }

    #[test]
    fn kernel_io_and_clock_tokens_are_reported_with_location() {
        let source = "\
fn a() {
    let _ = std::fs::read(\"x\");
    let _ = tokio::fs::read(\"x\");
    let _ = std::process::id();
    let _ = tokio::process::Command::new(\"x\");
    let _ = std::net::TcpStream::connect(\"x\");
    let _ = tokio::net::TcpStream::connect(\"x\");
    let _ = tokio::time::Instant::now();
    let _ = chrono::Utc::now();
}
";
        let found = kernel("crates/reno-kernel/src/a.rs", source);
        assert_eq!(found.len(), 8, "{found:?}");
        assert_eq!(
            found[0],
            "kernel: reno-kernel -> crates/reno-kernel/src/a.rs:2 contains `std::fs`"
        );
        assert!(
            found[6].ends_with(":8 contains `Instant::now`"),
            "{found:?}"
        );
        assert!(found[7].ends_with(":9 contains `Utc::now`"), "{found:?}");
    }

    #[test]
    fn system_time_is_allowed_only_in_the_clock_module() {
        let source = "fn now() { let _ = std::time::SystemTime::now(); }\n";
        assert!(kernel("crates/reno-kernel/src/clock.rs", source).is_empty());
        assert!(kernel("/abs/checkout/crates/reno-kernel/src/clock.rs", source).is_empty());
        for other in [
            "crates/reno-kernel/src/lib.rs",
            "crates/reno-kernel/src/sub/clock.rs",
            "crates/reno-kernel/src/clock_extra.rs",
            "crates/other/src/clock.rs",
        ] {
            assert_eq!(
                kernel(other, source),
                [format!(
                    "kernel: reno-kernel -> {other}:1 contains `SystemTime::now`"
                )]
            );
        }
    }

    #[test]
    fn the_clock_module_is_still_held_to_the_other_tokens() {
        let source = "fn a() { let _ = std::fs::read(\"x\"); }\n";
        assert_eq!(kernel("crates/reno-kernel/src/clock.rs", source).len(), 1);
    }

    #[test]
    fn kernel_ignores_comments_and_inline_cfg_test_modules() {
        let source = "\
//! Never calls std::fs or Instant::now.
/// Reads no SystemTime::now.
fn a() {} // not std::process
#[cfg(test)]
mod tests {
    fn t() {
        let _ = std::process::Command::new(\"sleep\");
    }
}
fn b() { let _ = tokio::net::lookup_host(\"x\"); }
";
        let found = kernel("crates/reno-kernel/src/a.rs", source);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains(":10 contains `tokio::net`"), "{found:?}");
    }

    #[test]
    fn a_file_that_opens_with_cfg_test_is_skipped_whole() {
        let source = "//! Tests.\n\n#![cfg(test)]\nuse std::process::Command;\nuse std::fs;\n";
        assert!(kernel("crates/reno-kernel/src/tests.rs", source).is_empty());
        assert!(is_test_only_file(source));
        // Only when it is the first item: later in a file it proves nothing.
        let late = "use std::fs;\n#![cfg(test)]\n";
        assert!(!is_test_only_file(late));
        assert_eq!(kernel("crates/reno-kernel/src/x.rs", late).len(), 1);
        // A stray `#[cfg(test)] mod tests;` does not hide the file it names.
        assert!(!is_test_only_file("#[cfg(test)]\nmod tests;\n"));
    }

    #[test]
    fn wire_and_kernel_rules_do_not_share_their_token_lists() {
        // `.await` is the kernel's daily bread; `std::fs` is forbidden in both.
        let source = "async fn a() { f().await; }\n";
        assert!(kernel("crates/reno-kernel/src/a.rs", source).is_empty());
        assert_eq!(wire_violations(Path::new("w.rs"), source).len(), 1);
    }
}
