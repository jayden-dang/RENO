//! `cargo xtask check`: asserts the architecture rules that the compiler cannot.
//!
//! Sans-IO layout: [`run_cargo_metadata`] and [`wire_sources`] are the only functions that touch
//! the machine. Everything else takes parsed data and returns [`Violation`]s.
//!
//! Rules:
//! - `edges`: workspace members depend on each other only along the ARCH-1 allow-list.
//! - `ownership`: ARCH-5 and friends; some external packages belong to one crate only.
//! - `wire`: ARCH-6; `reno-harness/src/wire/` contains no I/O, clock or async tokens.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde::Deserialize;

/// Allowed normal and build dependencies between workspace members (ARCH-1).
/// A member that is not listed is itself a violation.
const ALLOWED_EDGES: &[(&str, &[&str])] = &[
    ("reno-proto", &[]),
    ("reno-store", &["reno-proto"]),
    ("reno-harness", &["reno-proto"]),
    ("reno-rpc", &["reno-proto"]),
    ("reno-kernel", &["reno-proto", "reno-store", "reno-harness"]),
    ("reno-client", &["reno-proto", "reno-rpc"]),
    ("renod", &["reno-proto", "reno-kernel", "reno-rpc"]),
    ("reno", &["reno-proto", "reno-client"]),
    (
        "reno-app",
        &["reno-proto", "reno-client", "reno-markdown", "reno-syntax"],
    ),
    ("reno-markdown", &[]),
    ("reno-syntax", &[]),
];

/// Extra dev-dependency edges, as `(member, dependency)`.
const ALLOWED_DEV_EDGES: &[(&str, &str)] = &[("renod", "reno-client")];

/// Members outside the layering.
const EXEMPT: &[&str] = &["xtask"];

/// External packages and the only member allowed to depend on them directly.
/// A trailing `*` matches a name prefix.
const OWNERS: &[(&str, &str)] = &[
    ("rusqlite", "reno-store"),
    ("libsqlite3-sys", "reno-store"),
    ("gpui*", "reno-app"),
    ("notify", "renod"),
    ("security-framework", "renod"),
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
    ".await",
];

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
    match std::env::args().nth(1).as_deref() {
        Some("check") => check(),
        _ => {
            eprintln!("usage: cargo xtask check");
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
    match wire_sources(&wire_dir) {
        Ok(sources) => {
            for (path, source) in sources {
                violations.extend(wire_violations(&path, &source));
            }
        }
        Err(error) => {
            eprintln!("xtask: reading {}: {error}", wire_dir.display());
            return ExitCode::from(2);
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
fn wire_sources(dir: &Path) -> std::io::Result<Vec<(PathBuf, String)>> {
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
            for (pattern, owner) in OWNERS {
                if pattern_matches(pattern, used) && name != *owner {
                    violations.push(Violation::new(
                        "ownership",
                        name,
                        format!("{used} is only allowed in {owner}"),
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
///
/// Approach: a line scan, not a parser. Text after `//` is ignored, so comments and doc comments
/// may name the tokens. An item that follows `#[cfg(test)]` is skipped by counting braces from its
/// first `{` to the matching `}`; this assumes braces do not appear in string literals there.
fn wire_violations(path: &Path, source: &str) -> Vec<Violation> {
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
        for token in WIRE_TOKENS {
            if code.contains(token) {
                violations.push(Violation::new(
                    "wire",
                    "reno-harness",
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
        ]);
        assert_eq!(
            rendered(&ownership_violations(&m)),
            [
                "ownership: reno-kernel -> rusqlite is only allowed in reno-store",
                "ownership: reno-kernel -> gpui_tokio is only allowed in reno-app",
                "ownership: reno-proto -> tokio is not allowed here",
                "ownership: reno -> notify is only allowed in renod",
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
        let sources = wire_sources(Path::new("/nonexistent/reno/wire"));
        assert!(sources.is_ok_and(|found| found.is_empty()));
    }
}
