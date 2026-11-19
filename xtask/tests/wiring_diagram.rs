//! The wiring diagram's data is the graph the page renders.
//!
//! Feature names are the README's. Endpoints are the first-party `.rs` files at the
//! branch point recorded in the page. Knot callouts name a line of that revision.

use std::path::PathBuf;
use std::process::Command;

const FEATURES: &[&str] = &[
    "Bots",
    "Teams",
    "Routing",
    "Approvals",
    "Flows",
    "Blackboards",
    "Agent-to-agent messages",
    "Evaluations",
    "Self-improvement under a gate",
    "Triggers",
    "A git worktree per thread",
];

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .to_path_buf()
}

fn page_source() -> String {
    let path = workspace().join("docs/wiring.html");
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn wiring(source: &str) -> serde_json::Value {
    let start = source
        .find(r#"<script type="application/json" id="wiring">"#)
        .expect("wiring data");
    let from = start + r#"<script type="application/json" id="wiring">"#.len();
    let end = source[from..].find("</script>").expect("wiring script end");
    let raw = &source[from..from + end];
    assert!(
        source.contains(r#"JSON.parse(document.getElementById("wiring").textContent)"#),
        "the page must render the wiring data"
    );
    assert!(source.contains("knot.file"), "callouts must show the file");
    assert!(source.contains("knot.line"), "callouts must show the line");
    assert!(source.contains("knot.how"), "callouts must show how to untangle");
    assert!(source.contains("edge.reason"), "each line must carry its reason");
    serde_json::from_str(raw).expect("wiring json")
}

fn git_show(sha: &str, file: &str) -> String {
    let output = Command::new("git")
        .args(["show", &format!("{sha}:{file}")])
        .current_dir(workspace())
        .output()
        .expect("git show");
    assert!(output.status.success(), "git show {sha}:{file} failed");
    String::from_utf8(output.stdout).expect("utf8")
}

fn rust_at(sha: &str) -> std::collections::BTreeSet<String> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", sha])
        .current_dir(workspace())
        .output()
        .expect("git ls-tree");
    assert!(output.status.success(), "git ls-tree failed");
    String::from_utf8(output.stdout)
        .expect("utf8")
        .lines()
        .filter(|line| line.ends_with(".rs"))
        .filter(|line| {
            line.starts_with("apps/") || line.starts_with("crates/") || line.starts_with("xtask/")
        })
        .filter(|line| !line.contains("/target/"))
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_diagram_wires_every_rust_file_and_names_each_knot() {
    let source = page_source();
    let data = wiring(&source);
    let features: Vec<String> = data["features"]
        .as_array()
        .expect("features")
        .iter()
        .map(|value| value.as_str().expect("feature name").to_owned())
        .collect();
    assert_eq!(features, FEATURES);

    let sha = data["base"].as_str().expect("base sha");
    assert_eq!(sha.len(), 40, "branch point sha");
    let on_disk = rust_at(sha);

    let listed: std::collections::BTreeSet<String> = data["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|value| value.as_str().expect("file").to_owned())
        .collect();
    assert_eq!(listed, on_disk, "endpoint set is the .rs list at the branch point");

    let edges = data["edges"].as_array().expect("edges");
    let mut endpoints = std::collections::BTreeSet::new();
    let mut by_feature: std::collections::BTreeMap<&str, std::collections::BTreeSet<&str>> =
        std::collections::BTreeMap::new();
    for edge in edges {
        let feature = edge["feature"].as_str().expect("edge feature");
        let file = edge["file"].as_str().expect("edge file");
        let reason = edge["reason"].as_str().expect("edge reason");
        assert!(FEATURES.contains(&feature), "unknown feature {feature}");
        assert!(!reason.trim().is_empty(), "empty reason for {feature} -> {file}");
        assert!(on_disk.contains(file), "edge leaves the .rs set: {file}");
        endpoints.insert(file.to_owned());
        by_feature.entry(feature).or_default().insert(file);
    }
    assert_eq!(
        endpoints,
        on_disk,
        "every .rs file is an endpoint, and every endpoint is an .rs file"
    );
    for feature in FEATURES {
        let touched = by_feature.get(feature).map(std::collections::BTreeSet::len).unwrap_or(0);
        assert!(touched >= 1, "{feature} touches no file");
        assert!(touched < on_disk.len(), "{feature} touches every file");
    }

    let knots = data["knots"].as_array().expect("knots");
    let mut numbers = Vec::new();
    for knot in knots {
        let number = knot["n"].as_u64().expect("knot number");
        let file = knot["file"].as_str().expect("knot file");
        let line = knot["line"].as_u64().expect("knot line");
        let how = knot["how"].as_str().expect("knot how");
        let needle = knot["needle"].as_str().expect("knot needle");
        assert!(number >= 1, "knot numbers start at 1");
        assert!(!how.trim().is_empty(), "knot {number} has no how-to");
        assert!(on_disk.contains(file), "knot {number} is not a first-party .rs file");
        let shown = git_show(sha, file);
        let lines: Vec<&str> = shown.lines().collect();
        let index = usize::try_from(line).expect("line") - 1;
        assert!(index < lines.len(), "knot {number} line {line} is past the end of {file}");
        assert!(
            lines[index].contains(needle),
            "knot {number} {file}:{line} does not contain {needle:?}: {}",
            lines[index]
        );
        numbers.push(number);
    }
    numbers.sort_unstable();
    let expected: Vec<u64> = (1..=numbers.len() as u64).collect();
    assert_eq!(numbers, expected, "knot numbers are 1..n with no gaps");
}
