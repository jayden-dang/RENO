//! Golden JSON of History and the Review upgrade (RP3).

#![allow(clippy::unwrap_used)]

use std::fmt::Debug;

use reno_proto::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn golden<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T, wire: Value) {
    assert_eq!(serde_json::to_value(value).unwrap(), wire, "serialise");
    let parsed: T = serde_json::from_str(&wire.to_string()).unwrap();
    assert_eq!(&parsed, value, "deserialise");
}

fn run() -> RunId {
    "run_01HZX9".parse().unwrap()
}

#[test]
fn the_history_feature_is_offered_and_its_methods_are_named() {
    assert!(FEATURES.contains(&FEATURE_HISTORY));
    assert_eq!(FEATURE_HISTORY, "history");
    assert_eq!(<GitLog as Method>::NAME, "git.log");
    assert_eq!(<GitLogSearch as Method>::NAME, "git.log_search");
    assert_eq!(
        <WorkspaceFileSources as Method>::NAME,
        "workspace.file_sources"
    );
}

#[test]
fn log_params_and_result_have_their_wire_shape() {
    golden(
        &GitLogParams {
            run_id: run(),
            cursor: Some("1.abc.50".into()),
            limit: Some(20),
            rev: Some(GitLogRev::Thread),
        },
        json!({"runId": "run_01HZX9", "cursor": "1.abc.50", "limit": 20, "rev": "thread"}),
    );
    golden(
        &GitLogParams {
            run_id: run(),
            cursor: None,
            limit: None,
            rev: None,
        },
        json!({"runId": "run_01HZX9"}),
    );
    golden(
        &GitLogResult {
            commits: vec![GitCommit {
                sha: "a".repeat(40),
                parents: vec!["b".repeat(40), "c".repeat(40)],
                subject: "merge".into(),
                author_name: "Ada".into(),
                author_key: author_key("Ada@Example.com "),
                time_ms: 1_700_000_000_000,
                refs: vec![
                    GitRef {
                        name: "HEAD".into(),
                        kind: GitRefKind::Head,
                    },
                    GitRef {
                        name: "origin/main".into(),
                        kind: GitRefKind::Remote,
                    },
                    GitRef {
                        name: "v1".into(),
                        kind: GitRefKind::Tag,
                    },
                ],
            }],
            next_cursor: Some("1.abc.1".into()),
            head: Some("a".repeat(40)),
            repo: true,
            shallow: true,
            in_place: false,
        },
        json!({"commits": [{
            "sha": "a".repeat(40), "parents": ["b".repeat(40), "c".repeat(40)],
            "subject": "merge", "authorName": "Ada", "authorKey": author_key("ada@example.com"),
            "timeMs": 1_700_000_000_000u64,
            "refs": [{"name": "HEAD", "kind": "head"}, {"name": "origin/main", "kind": "remote"},
                     {"name": "v1", "kind": "tag"}]}],
            "nextCursor": "1.abc.1", "head": "a".repeat(40), "repo": true, "shallow": true}),
    );
    // The author key is the email's hash, whatever its case, and never the email.
    assert_eq!(author_key("A@B.c"), author_key(" a@b.c"));
    assert!(!author_key("a@b.c").contains('@'));
}

#[test]
fn search_and_file_sources_have_their_wire_shape() {
    golden(
        &GitLogSearchParams {
            run_id: run(),
            query: "fix(".into(),
            limit: None,
            rev: None,
        },
        json!({"runId": "run_01HZX9", "query": "fix("}),
    );
    golden(
        &GitLogSearchResult {
            commits: Vec::new(),
            truncated: true,
            repo: true,
            in_place: true,
        },
        json!({"commits": [], "truncated": true, "repo": true, "inPlace": true}),
    );
    golden(
        &FileSourcesParams {
            run_id: run(),
            path: "src/lib.rs".into(),
            scope: Some(DiffScope::WorkingTree),
            checksum: Some("c1".into()),
        },
        json!({"runId": "run_01HZX9", "path": "src/lib.rs",
               "scope": {"type": "working_tree"}, "checksum": "c1"}),
    );
    golden(
        &FileSourcesResult {
            path: "b.rs".into(),
            old_path: Some("a.rs".into()),
            old_text: Some("old".into()),
            new_text: None,
            old_hash: Some("h1".into()),
            new_hash: None,
            checksum: "c2".into(),
            stale: true,
            binary: false,
            truncated: true,
        },
        json!({"path": "b.rs", "oldPath": "a.rs", "oldText": "old", "oldHash": "h1",
               "checksum": "c2", "stale": true, "truncated": true}),
    );
}

#[test]
fn the_diff_checksum_is_additive_and_changes_with_the_diff() {
    // An older daemon sends none: it reads as empty, and an empty one is not sent back out.
    let old: WorkspaceDiffResult = serde_json::from_value(json!({
        "runId": "run_01HZX9", "branch": "b", "baseSha": "s", "commits": 0, "files": []
    }))
    .unwrap();
    assert_eq!(old.checksum, "");
    assert!(
        serde_json::to_value(&old)
            .unwrap()
            .get("checksum")
            .is_none()
    );

    let file = |additions| ChangedFile {
        path: "a.rs".into(),
        from: None,
        kind: ChangeKind::Modified,
        additions,
        deletions: 0,
        binary: false,
    };
    let one = diff_checksum(&[file(1)], Some("patch"), false);
    assert_eq!(one, diff_checksum(&[file(1)], Some("patch"), false));
    assert_ne!(one, diff_checksum(&[file(2)], Some("patch"), false));
    assert_ne!(one, diff_checksum(&[file(1)], Some("patch2"), false));
    assert_ne!(one, diff_checksum(&[file(1)], None, false));
    assert_ne!(one, diff_checksum(&[file(1)], Some("patch"), true));
    assert_eq!(one.len(), 64);
}

#[test]
fn the_working_tree_scope_is_a_new_variant_of_the_diff_scope() {
    golden(&DiffScope::WorkingTree, json!({"type": "working_tree"}));
}
