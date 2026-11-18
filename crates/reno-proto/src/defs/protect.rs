//! Built-in protected paths: what no run may change, whatever the policy says.
//!
//! A bot that can edit `.claude/settings.json`, `.codex/config.toml`, `.pi/extensions`, `.mcp.json`, a git hook or
//! Reno's own policy and definitions can grant itself permissions or plant a hook that runs in a
//! later session, outside any approval. So [`violation`] is checked by [`super::evaluate`]
//! **before the policy's rules**: its denial is final, an `allow` rule in `policy.toml` cannot
//! loosen it, and neither can a standing rule (the kernel judges a policy denial first).
//!
//! # What is protected
//!
//! A path with a component `.git`, `.claude`, `.codex`, `.pi` or `.reno` anywhere in it, a file named
//! `.mcp.json`, and (outside the run's worktree) Reno's data directory, `Application Support/Reno`.
//! Components compare ASCII case-insensitively, as macOS does. A path inside the worktree is
//! judged by its worktree-relative part, so a worktree that itself lives under a `.claude` or
//! `.reno` directory is not locked. `.gitignore` and `.github` are not protected.
//!
//! # What is refused
//!
//! - An `edit` action (the file-edit tools, a Codex file change) with any such path. Reading is
//!   never refused here.
//! - A command whose simple command (read through wrappers, see `normalize`) is a file mutator
//!   aimed at such a path: `rm`, `mv`, `tee`, `touch`, `chmod`, `ln`, `truncate`, `patch`,
//!   `sed -i`, `perl -i` (any operand); `cp`, `install`, `rsync` (the destination); `dd of=`;
//!   or any output redirection (`> path`, `>> path`) to one.
//! - **Not refused:** `git commit`, `git checkout`, `git config` and every other git command.
//!   Git writes `.git` itself, and the policy's allow and deny rules are how a person governs
//!   it. The same goes for any program that is not a file mutator named above (a script or an
//!   interpreter can write anywhere; the harness's sandbox is the control for that).
//!
//! # Opting out
//!
//! There is no policy switch, on purpose: the policy file is itself protected, so a switch would
//! be one more thing a run could flip. A person who wants a bot to edit one of these files does
//! it themselves, or runs the harness without Reno's approvals.

use std::path::{Path, PathBuf};

use super::eval::locate_text;
use super::normalize::{basename, deny_forms, tokenize};
use crate::exec::{ActionClass, ApprovalSubject, ToolCall};

/// The name the denial carries in place of a policy rule.
pub const PROTECTED_RULE: &str = "built-in protected paths";

const PROTECTED_DIRS: &[&str] = &[".git", ".claude", ".codex", ".pi", ".reno"];
const PROTECTED_FILE: &str = ".mcp.json";

/// Why `subject` is refused by the built-in protection, or `None`. `cwd` must be lexical.
pub(super) fn violation(cwd: &Path, subject: &ApprovalSubject) -> Option<String> {
    let hit = hit(cwd, subject)?;
    Some(format!(
        "denied: `{hit}` is protected (git internals, agent and Reno configuration cannot be \
         changed by a run)"
    ))
}

/// The protected path `subject` would change, as it was written.
fn hit(cwd: &Path, subject: &ApprovalSubject) -> Option<String> {
    match subject.action {
        ActionClass::Edit => subject
            .paths
            .iter()
            .find(|path| protected_path(cwd, path))
            .map(|path| path.display().to_string()),
        ActionClass::Command => subject
            .command
            .as_deref()
            .and_then(|line| command_target(cwd, line)),
        _ => None,
    }
}

/// Whether `path`, relative to a run's worktree root and written with `/`, is protected. The
/// after-the-fact checks (a worktree diff) use it, where no approval could refuse the write.
pub fn is_protected_relative(path: &str) -> bool {
    protected_text(true, path)
}

/// The protected path a finished tool call changed, as the call named it, or `None`. Read from
/// the call's own input (a file tool's path, a file change's paths, a command line), so it is a
/// hint for a harness that cannot be asked: it sees only what the call says, and a script that
/// writes by other means shows nothing. `cwd` must be lexical.
pub fn protected_path_of_call(cwd: &Path, call: &ToolCall) -> Option<String> {
    let input = &call.input;
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut take = |value: &serde_json::Value| {
        if let Some(path) = value.as_str().filter(|path| !path.is_empty()) {
            paths.push(PathBuf::from(path));
        }
    };
    for key in ["file_path", "notebook_path", "path", "filePath"] {
        if let Some(value) = input.get(key) {
            take(value);
        }
    }
    for key in ["changes", "locations"] {
        for entry in input
            .get(key)
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(value) = entry.get("path") {
                take(value);
            }
        }
    }
    let command = input
        .get("command")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    hit(
        cwd,
        &ApprovalSubject {
            action: call.class,
            tool: call.name.clone(),
            command,
            paths,
            raw: serde_json::Value::Null,
        },
    )
}

fn protected_path(cwd: &Path, path: &Path) -> bool {
    let (inside, text) = locate_text(cwd, path);
    protected_text(inside, &text)
}

fn protected_text(inside: bool, text: &str) -> bool {
    let parts: Vec<&str> = text.split('/').collect();
    if parts.iter().any(|part| {
        PROTECTED_DIRS
            .iter()
            .any(|dir| part.eq_ignore_ascii_case(dir))
    }) {
        return true;
    }
    if parts
        .last()
        .is_some_and(|last| last.eq_ignore_ascii_case(PROTECTED_FILE))
    {
        return true;
    }
    !inside
        && parts.windows(2).any(|pair| {
            pair[0].eq_ignore_ascii_case("Application Support")
                && pair[1].eq_ignore_ascii_case("Reno")
        })
}

fn word_protected(cwd: &Path, word: &str) -> bool {
    !word.is_empty() && protected_path(cwd, Path::new(word))
}

/// The protected path a command writes, if any of its simple commands writes one.
fn command_target(cwd: &Path, line: &str) -> Option<String> {
    for form in deny_forms(line) {
        let words = tokenize(&form).words;
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        if let Some(hit) = redirect_target(cwd, &texts) {
            return Some(hit);
        }
        let Some((&program, args)) = texts.split_first() else {
            continue;
        };
        let operands = || args.iter().copied().filter(|a| !a.starts_with('-'));
        let hit = match basename(program) {
            "rm" | "rmdir" | "unlink" | "tee" | "touch" | "chmod" | "chown" | "chgrp"
            | "truncate" | "mv" | "ln" | "patch" | "ed" | "ex" | "shred" | "mkdir" => operands()
                .find(|a| word_protected(cwd, a))
                .map(str::to_owned),
            "cp" | "install" | "rsync" => operands()
                .next_back()
                .filter(|a| word_protected(cwd, a))
                .or_else(|| {
                    args.iter()
                        .filter_map(|a| a.strip_prefix("--target-directory="))
                        .find(|a| word_protected(cwd, a))
                })
                .map(str::to_owned),
            "sed" | "perl" | "awk" | "gawk" => {
                let in_place = args.iter().any(|a| {
                    *a == "--in-place"
                        || a.starts_with("--in-place=")
                        || (a.starts_with('-') && !a.starts_with("--") && a.contains('i'))
                        || *a == "inplace"
                });
                in_place
                    .then(|| operands().find(|a| word_protected(cwd, a)))
                    .flatten()
                    .map(str::to_owned)
            }
            "dd" => args
                .iter()
                .filter_map(|a| a.strip_prefix("of="))
                .find(|a| word_protected(cwd, a))
                .map(str::to_owned),
            _ => None,
        };
        if hit.is_some() {
            return hit;
        }
    }
    None
}

/// The protected target of an output redirection among `texts` (`> p`, `>> p`, `>p`, `2>p`).
fn redirect_target(cwd: &Path, texts: &[&str]) -> Option<String> {
    for (n, text) in texts.iter().enumerate() {
        let Some(at) = text.rfind('>') else { continue };
        let after = text[at + 1..].trim_start_matches(['|', '&']);
        let target = if after.is_empty() {
            texts.get(n + 1).copied().unwrap_or("")
        } else {
            after
        };
        if word_protected(cwd, target) {
            return Some(target.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn call(class: ActionClass, input: serde_json::Value) -> ToolCall {
        ToolCall::new("t", input, class)
    }

    #[test]
    fn relative_paths_are_judged_like_the_approval_step_judges_them() {
        for path in [
            ".claude/settings.json",
            "a/.codex/c.toml",
            // Pi loads extensions, settings and an MCP config from its project directory.
            ".pi/extensions/x.ts",
            ".reno/policy.toml",
            ".mcp.json",
            "x/.MCP.json",
            ".Claude/x",
        ] {
            assert!(is_protected_relative(path), "{path}");
        }
        for path in ["src/lib.rs", ".gitignore", ".github/ci.yml", "claude/x"] {
            assert!(!is_protected_relative(path), "{path}");
        }
    }

    #[test]
    fn a_finished_call_names_the_protected_path_it_wrote() {
        let cwd = Path::new("/w");
        let hit = |class, input| protected_path_of_call(cwd, &call(class, input));
        // Claude, Codex and ACP spell the path three ways.
        assert_eq!(
            hit(
                ActionClass::Edit,
                json!({"file_path": "/w/.claude/settings.json"})
            )
            .as_deref(),
            Some("/w/.claude/settings.json")
        );
        assert_eq!(
            hit(
                ActionClass::Edit,
                json!({"changes": [{"path": "a.rs"}, {"path": ".mcp.json"}]})
            )
            .as_deref(),
            Some(".mcp.json")
        );
        assert_eq!(
            hit(
                ActionClass::Edit,
                json!({"locations": [{"path": ".codex/config.toml"}]})
            )
            .as_deref(),
            Some(".codex/config.toml")
        );
        assert!(
            hit(
                ActionClass::Command,
                json!({"command": "echo x > .claude/settings.json"})
            )
            .is_some()
        );
        // Nothing protected, or only read.
        assert_eq!(
            hit(ActionClass::Edit, json!({"file_path": "/w/src/a.rs"})),
            None
        );
        assert_eq!(
            hit(
                ActionClass::Read,
                json!({"file_path": "/w/.claude/settings.json"})
            ),
            None
        );
        assert_eq!(hit(ActionClass::Edit, json!(null)), None);
    }
}
