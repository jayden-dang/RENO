//! The fake daemon's workspaces: what `workspace.get` and `workspace.diff` answer for each run.
//!
//! Which runs have what is fixed by their id, so a test knows what to expect:
//!
//! - `scout1` has a workspace with no changes;
//! - `builder0` had one, and it was removed;
//! - `reviewer1` and `architect1` never had one (`workspace.get` is `not_found`);
//! - every other run (`builder1`, `keeper1`, the runs `task.run` makes) has a workspace holding
//!   six changed files over three commits: one of each kind, and a binary one.

use std::path::PathBuf;

use reno_proto::{
    ChangeKind, ChangedFile, RunId, RunStatus, WorkspaceChanges, WorkspaceDiffResult,
    WorkspaceInfo, WorkspaceStatus,
};

use super::data::NOW_MS;

/// What a run's workspace is like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Fixture {
    /// Changed files over three commits.
    Full,
    /// A workspace nothing was changed in.
    Empty,
    /// A workspace that has been removed.
    Removed,
    /// The run never had one.
    Missing,
}

pub(super) fn fixture(run: &RunId) -> Fixture {
    let id = run.as_str();
    if id.ends_with("scout1") {
        Fixture::Empty
    } else if id.ends_with("builder0") {
        Fixture::Removed
    } else if id.ends_with("reviewer1") || id.ends_with("architect1") {
        Fixture::Missing
    } else {
        Fixture::Full
    }
}

const BASE: &str = "3f9c2a1b7d4e5f60718293a4b5c6d7e8f9012345";
/// Where a follow-up run's branch stood when it started.
pub(super) const FOLLOW_UP_START: &str = "7c1e90ab23d45f6718293a4b5c6d7e8f90123456";
const COMMITS: u32 = 3;

type FileRow = (
    &'static str,
    Option<&'static str>,
    ChangeKind,
    u32,
    u32,
    bool,
);

/// The full fixture, by path.
const FILES: [FileRow; 6] = [
    ("Cargo.toml", None, ChangeKind::Modified, 1, 1, false),
    (
        "apps/renod/src/main.rs",
        Some("apps/renod/src/start.rs"),
        ChangeKind::Renamed,
        3,
        1,
        false,
    ),
    ("assets/logo.png", None, ChangeKind::Modified, 0, 0, true),
    (
        "crates/reno-kernel/src/lease.rs",
        None,
        ChangeKind::Modified,
        6,
        2,
        false,
    ),
    (
        "crates/reno-kernel/src/lease_tests.rs",
        None,
        ChangeKind::Added,
        5,
        0,
        false,
    ),
    ("docs/old-notes.md", None, ChangeKind::Deleted, 0, 4, false),
];

fn files() -> Vec<ChangedFile> {
    FILES
        .iter()
        .map(
            |&(path, from, kind, additions, deletions, binary)| ChangedFile {
                path: path.to_owned(),
                from: from.map(str::to_owned),
                kind,
                additions,
                deletions,
                binary,
            },
        )
        .collect()
}

fn branch(run: &RunId) -> String {
    format!("reno/{}", run.as_str().trim_start_matches("run_"))
}

/// `workspace.get`, `None` when the run never had a workspace.
pub(super) fn info(run: &RunId, status: RunStatus) -> Option<WorkspaceInfo> {
    let fixture = fixture(run);
    let state = match fixture {
        Fixture::Missing => return None,
        Fixture::Removed => WorkspaceStatus::Removed,
        _ if status.is_terminal() => WorkspaceStatus::Kept,
        _ => WorkspaceStatus::Active,
    };
    let full = fixture == Fixture::Full;
    Some(WorkspaceInfo {
        run_id: run.clone(),
        repo: PathBuf::from("/demo/reno"),
        repo_key: "demo-reno".to_owned(),
        path: PathBuf::from(format!("/demo/reno-workspaces/{}", run.as_str())),
        subdir: String::new(),
        branch: branch(run),
        base_sha: BASE.to_owned(),
        status: state,
        changes: (fixture != Fixture::Removed).then_some(WorkspaceChanges {
            commits: if full { COMMITS } else { 0 },
            dirty: full,
        }),
        created_at_ms: NOW_MS - 3_600_000,
        updated_at_ms: NOW_MS - 300_000,
    })
}

/// `workspace.diff`: the files, and the patch of `path` when asked and it has one. `None` when
/// the run never had a workspace.
pub(super) fn diff(run: &RunId, path: Option<&str>) -> Option<WorkspaceDiffResult> {
    let fixture = fixture(run);
    if fixture == Fixture::Missing {
        return None;
    }
    let full = fixture == Fixture::Full;
    let files = if full { files() } else { vec![] };
    let patch = path
        .and_then(|p| files.iter().find(|f| f.path == p))
        .filter(|f| !f.binary)
        .and_then(|f| patch_of(&f.path));
    let mut result = WorkspaceDiffResult {
        run_id: run.clone(),
        branch: branch(run),
        base_sha: BASE.to_owned(),
        commits: if full { COMMITS } else { 0 },
        files,
        patch,
        patch_truncated: false,
        checksum: String::new(),
    };
    seal(&mut result);
    Some(result)
}

/// What the working-tree scope of a run's worktree has: the last two files, nothing committed,
/// against the run's `HEAD`.
pub(super) const HEAD: &str = "e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6";

/// Sets the checksum of a diff the way the daemon does: of what is sent.
pub(super) fn seal(result: &mut WorkspaceDiffResult) {
    result.checksum = reno_proto::diff_checksum(
        &result.files,
        result.patch.as_deref(),
        result.patch_truncated,
    );
}

fn patch_of(path: &str) -> Option<String> {
    let text = match path {
        "Cargo.toml" => {
            "diff --git a/Cargo.toml b/Cargo.toml\n\
index 1a2b3c4..5d6e7f8 100644\n\
--- a/Cargo.toml\n\
+++ b/Cargo.toml\n\
@@ -14,7 +14,7 @@ members = [\n\
\x20    \"crates/reno-kernel\",\n\
\x20    \"crates/reno-proto\",\n\
\x20]\n\
-resolver = \"1\"\n\
+resolver = \"3\"\n\
\x20\n\
\x20[workspace.package]\n\
\x20edition = \"2024\"\n"
        }
        "apps/renod/src/main.rs" => {
            "diff --git a/apps/renod/src/start.rs b/apps/renod/src/main.rs\n\
similarity index 91%\n\
rename from apps/renod/src/start.rs\n\
rename to apps/renod/src/main.rs\n\
index 9a8b7c6..0d1e2f3 100644\n\
--- a/apps/renod/src/start.rs\n\
+++ b/apps/renod/src/main.rs\n\
@@ -1,5 +1,7 @@\n\
-fn start() {\n\
+fn main() {\n\
+    let args = std::env::args();\n\
\x20    run();\n\
+    drop(args);\n\
\x20}\n\
\x20\n\
\x20fn run() {}\n\
\\ No newline at end of file\n"
        }
        "crates/reno-kernel/src/lease.rs" => {
            "diff --git a/crates/reno-kernel/src/lease.rs b/crates/reno-kernel/src/lease.rs\n\
index 4455667..8899aab 100644\n\
--- a/crates/reno-kernel/src/lease.rs\n\
+++ b/crates/reno-kernel/src/lease.rs\n\
@@ -10,5 +10,8 @@ impl Lease {\n\
\x20    pub fn renew(&mut self, now: u64) {\n\
-        self.expires = now + 30;\n\
-        self.renewals += 1;\n\
+        self.expires = now + self.ttl;\n\
+        self.renewals = self.renewals.saturating_add(1);\n\
+        if self.renewals > MAX_RENEWALS {\n\
+            self.expire();\n\
+        }\n\
\x20    }\n\
\x20\n\
@@ -42,4 +45,5 @@ impl Lease {\n\
\x20    pub fn expire(&mut self) {\n\
\x20        self.expires = 0;\n\
+        self.state = State::Expired;\n\
\x20    }\n\
\x20}\n"
        }
        "crates/reno-kernel/src/lease_tests.rs" => {
            "diff --git a/crates/reno-kernel/src/lease_tests.rs b/crates/reno-kernel/src/lease_tests.rs\n\
new file mode 100644\n\
index 0000000..c0ffee1\n\
--- /dev/null\n\
+++ b/crates/reno-kernel/src/lease_tests.rs\n\
@@ -0,0 +1,5 @@\n\
+#[test]\n\
+fn a_lease_expires_after_too_many_renewals() {\n\
+    let mut lease = Lease::new(30);\n\
+    (0..=MAX_RENEWALS).for_each(|t| lease.renew(t));\n\
+    assert!(lease.is_expired());\n"
        }
        "docs/old-notes.md" => {
            "diff --git a/docs/old-notes.md b/docs/old-notes.md\n\
deleted file mode 100644\n\
index 7777777..0000000\n\
--- a/docs/old-notes.md\n\
+++ /dev/null\n\
@@ -1,4 +0,0 @@\n\
-# Old notes\n\
-\n\
-Superseded by the lease design.\n\
-Delete when the scheduler lands.\n"
        }
        _ => return None,
    };
    Some(text.to_owned())
}
