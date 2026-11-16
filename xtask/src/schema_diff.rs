//! `cargo xtask codex-schema-diff <DIR>`: what changed between the Codex schema snapshot in the
//! repository and the schema the installed CLI generates.
//!
//! ```text
//! codex app-server generate-json-schema --out /tmp/codex-schema
//! cargo xtask codex-schema-diff /tmp/codex-schema
//! ```
//!
//! Only the files the snapshot keeps are compared (the four union files and the replies the
//! driver sends). Doc strings (`description`) are ignored: they change every release and say
//! nothing about the wire. The report names added, removed and changed methods of the union
//! files, added, removed and changed definitions, and any other difference by JSON path. It
//! exits 0 when nothing differs, 1 when something does, and 2 when it could not look.
//!
//! [`diff_files`] is pure; the shell only reads the two directories.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::Value;

pub(crate) const USAGE: &str = "usage: cargo xtask codex-schema-diff <DIR of `codex app-server generate-json-schema --out DIR`>";

/// Where the snapshot lives, relative to the workspace root.
const SNAPSHOT: &str = "crates/reno-harness/tests/fixtures/codex/schema";

/// The files that list methods in a `oneOf`.
const UNIONS: &[&str] = &[
    "ClientRequest.json",
    "ClientNotification.json",
    "ServerRequest.json",
    "ServerNotification.json",
];

/// How many path-level differences one file lists before it says "and more".
const PATH_LIMIT: usize = 20;

/// `value` without the doc strings, which change every release.
fn strip_docs(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, v)| !(key.as_str() == "description" && v.is_string()))
                .map(|(key, v)| (key.clone(), strip_docs(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(strip_docs).collect()),
        other => other.clone(),
    }
}

/// The variants of a union file by method name.
fn variants(file: &Value) -> BTreeMap<String, Value> {
    file.get("oneOf")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .enumerate()
                .map(|(index, variant)| {
                    let name = variant
                        .pointer("/properties/method/enum/0")
                        .and_then(Value::as_str)
                        .map_or_else(|| format!("<variant {index}>"), str::to_owned);
                    (name, strip_docs(variant))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn definitions(file: &Value) -> BTreeMap<String, Value> {
    file.get("definitions")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(name, v)| (name.clone(), strip_docs(v)))
                .collect()
        })
        .unwrap_or_default()
}

/// Names added, removed and changed between two maps.
fn compare_maps(
    what: &str,
    old: &BTreeMap<String, Value>,
    new: &BTreeMap<String, Value>,
    out: &mut Vec<String>,
) {
    for name in new.keys().filter(|n| !old.contains_key(*n)) {
        out.push(format!("  + {what} {name} (new in the CLI)"));
    }
    for name in old.keys().filter(|n| !new.contains_key(*n)) {
        out.push(format!("  - {what} {name} (gone from the CLI)"));
    }
    for (name, was) in old {
        if let Some(is) = new.get(name)
            && was != is
        {
            out.push(format!("  ~ {what} {name} changed"));
        }
    }
}

/// Every JSON path at which `old` and `new` differ, up to `limit`.
fn paths(path: &str, old: &Value, new: &Value, limit: usize, out: &mut Vec<String>) {
    if out.len() >= limit || old == new {
        return;
    }
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let here = format!("{path}/{key}");
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => paths(&here, x, y, limit, out),
                    (Some(_), None) => out.push(format!("  - {here}")),
                    (None, Some(_)) => out.push(format!("  + {here}")),
                    (None, None) => {}
                }
            }
        }
        _ => out.push(format!("  ~ {path}")),
    }
}

/// The differences between the snapshot's `old` files and the generated `new` ones, as report
/// lines; empty when nothing differs. `new` may hold more files than `old`: they are ignored.
pub(crate) fn diff_files(
    old: &BTreeMap<String, Value>,
    new: &BTreeMap<String, Value>,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (name, was) in old {
        let Some(is) = new.get(name) else {
            lines.push(format!("{name}: missing from the generated schema"));
            continue;
        };
        let mut found = Vec::new();
        if UNIONS.contains(&name.as_str()) {
            compare_maps("method", &variants(was), &variants(is), &mut found);
        }
        compare_maps(
            "definition",
            &definitions(was),
            &definitions(is),
            &mut found,
        );
        let rest = |v: &Value| {
            let mut v = strip_docs(v);
            if let Value::Object(map) = &mut v {
                map.remove("oneOf");
                map.remove("definitions");
            }
            v
        };
        let before = found.len();
        paths("", &rest(was), &rest(is), before + PATH_LIMIT, &mut found);
        if found.len() >= before + PATH_LIMIT {
            found.push("  ... and more".to_owned());
        }
        if !found.is_empty() {
            lines.push(format!("{name}:"));
            lines.extend(found);
        }
    }
    lines
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// The `.json` files of `dir`, by name.
fn read_dir_json(
    dir: &Path,
    only: Option<&BTreeMap<String, Value>>,
) -> Result<BTreeMap<String, Value>, String> {
    let mut files = BTreeMap::new();
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        if !name.ends_with(".json") || only.is_some_and(|keep| !keep.contains_key(&name)) {
            continue;
        }
        files.insert(name, read_json(&path)?);
    }
    Ok(files)
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    let [dir] = args else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf);
    let snapshot_dir: PathBuf = root.unwrap_or_default().join(SNAPSHOT);
    let result = read_dir_json(&snapshot_dir, None)
        .and_then(|old| read_dir_json(Path::new(dir), Some(&old)).map(|new| (old, new)));
    let (old, new) = match result {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("xtask: {error}");
            return ExitCode::from(2);
        }
    };
    let version = fs::read_to_string(snapshot_dir.join("VERSION"))
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|l| !l.starts_with('#') && !l.trim().is_empty())
                .map(|l| l.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let lines = diff_files(&old, &new);
    if lines.is_empty() {
        println!("the generated schema matches the snapshot of codex {version}");
        return ExitCode::SUCCESS;
    }
    println!("the generated schema differs from the snapshot of codex {version}:\n");
    for line in &lines {
        println!("{line}");
    }
    println!(
        "\nNext: docs/KEEPING-UP.md, \"Regenerate the Codex schema\". Run the schema test against \
         the new directory first:\n  RENO_CODEX_SCHEMA_DIR={dir} cargo test -p reno-harness --test codex_schema"
    );
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn files(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(n, v)| ((*n).to_owned(), v.clone()))
            .collect()
    }

    fn request(method: &str, field: &str) -> Value {
        json!({
            "description": "docs change every release",
            "properties": {
                "method": {"enum": [method], "type": "string"},
                "params": {"$ref": format!("#/definitions/{field}")},
            },
        })
    }

    #[test]
    fn identical_schemas_and_changed_doc_strings_are_no_difference() {
        let a = json!({"oneOf": [request("a", "A")], "definitions": {"A": {"type": "object", "description": "one"}}});
        let mut b = a.clone();
        b["definitions"]["A"]["description"] = json!("two, reworded");
        b["oneOf"][0]["description"] = json!("other docs");
        let old = files(&[("ServerRequest.json", a)]);
        assert!(diff_files(&old, &files(&[("ServerRequest.json", b)])).is_empty());
    }

    #[test]
    fn methods_and_definitions_are_listed_as_added_removed_and_changed() {
        let old = files(&[(
            "ServerNotification.json",
            json!({
                "oneOf": [request("keep", "K"), request("gone", "G"), request("moved", "M")],
                "definitions": {"K": {"type": "object"}, "G": {"type": "string"}},
            }),
        )]);
        let new = files(&[(
            "ServerNotification.json",
            json!({
                "oneOf": [request("keep", "K"), request("new/one", "N"), request("moved", "M2")],
                "definitions": {"K": {"type": "object", "required": ["x"]}, "N": {"type": "string"}},
            }),
        )]);
        let lines = diff_files(&old, &new);
        assert_eq!(
            lines,
            [
                "ServerNotification.json:",
                "  + method new/one (new in the CLI)",
                "  - method gone (gone from the CLI)",
                "  ~ method moved changed",
                "  + definition N (new in the CLI)",
                "  - definition G (gone from the CLI)",
                "  ~ definition K changed",
            ]
        );
    }

    #[test]
    fn a_reply_schema_differs_by_path_and_a_missing_file_is_named() {
        let old = files(&[
            (
                "ToolRequestUserInputResponse.json",
                json!({"type": "object", "required": ["answers"], "properties": {"answers": {"type": "object"}}}),
            ),
            ("ClientRequest.json", json!({"oneOf": []})),
        ]);
        let new = files(&[(
            "ToolRequestUserInputResponse.json",
            json!({"type": "object", "required": ["answers", "id"], "properties": {"answers": {"type": "array"}, "extra": {}}}),
        )]);
        let lines = diff_files(&old, &new);
        assert_eq!(
            lines[0],
            "ClientRequest.json: missing from the generated schema"
        );
        assert_eq!(lines[1], "ToolRequestUserInputResponse.json:");
        assert!(
            lines.contains(&"  ~ /properties/answers/type".to_owned()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"  + /properties/extra".to_owned()),
            "{lines:?}"
        );
        assert!(lines.contains(&"  ~ /required".to_owned()), "{lines:?}");
    }

    #[test]
    fn files_the_snapshot_does_not_keep_are_ignored() {
        let old = files(&[("ClientRequest.json", json!({"oneOf": []}))]);
        let new = files(&[
            ("ClientRequest.json", json!({"oneOf": []})),
            ("Unrelated.json", json!({"anything": true})),
        ]);
        assert!(diff_files(&old, &new).is_empty());
    }
}
