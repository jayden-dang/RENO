//! The wire contract snapshot (ARCH-13). See the crate docs for how to update it.

#![allow(clippy::expect_used)]

use std::collections::HashSet;

use reno_proto::protocol_schema;

#[test]
fn protocol_schema_matches_snapshot() {
    let mut settings = insta::Settings::clone_current();
    // Stable output even if a dependency turns on serde_json's `preserve_order`.
    settings.set_sort_maps(true);
    settings.bind(|| insta::assert_json_snapshot!("protocol_schema", protocol_schema()));
}

#[test]
fn names_are_unique_and_dot_namespaced() {
    let schema = protocol_schema();
    let mut seen = HashSet::new();
    for section in ["methods", "notifications", "subscriptions"] {
        let entries = schema[section].as_array().expect("section is an array");
        assert!(!entries.is_empty(), "{section} is empty");
        for entry in entries {
            let name = entry["name"].as_str().expect("entry has a name");
            let parts: Vec<&str> = name.split('.').collect();
            assert!(parts.len() >= 2, "{name} is not namespaced");
            assert!(
                parts.iter().all(
                    |p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                ),
                "{name} must be lowercase dot-separated words"
            );
            assert!(seen.insert(name.to_owned()), "duplicate name {name}");
        }
    }
}
