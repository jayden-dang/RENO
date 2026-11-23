//! A Pi gate call as the ladder's [`ApprovalSubject`].
//!
//! Sans-IO (ARCH-6): the gate's `call` line in, the subject out. The class is
//! [`normalize::action_class`], so a call is classed as the `Tool` event for it is.

use std::path::{Path, PathBuf};

use reno_proto::{ActionClass, ApprovalSubject};
use serde_json::{Value, json};

use super::normalize::action_class;

/// The subject of the gate call `call_id` to `tool`, made by `parent` when another tool made it.
/// `None` when the call cannot be read: a shell tool with no command, or a command, or the path of
/// an edit or read, that is there but is not text. The caller denies such a call, since what it
/// would run is not known.
///
/// A command is `input.command`, which a shell tool always has. Edits and reads name `input.path`;
/// `grep`, `find` and `ls` may omit it (meaning the working directory), which leaves no path. A
/// path is spelled as Pi will open it (see [`opened_paths`]). Pi expands `~` with its child's
/// `os.homedir()`, which is the `HOME` of the child's own environment, so the caller passes that
/// as `home`; without it `~/Library/Application Support/Reno` would read as a path inside the
/// working directory. A `find` pattern and a `grep` glob are not paths and stay only in `raw`,
/// which is the call as the gate sent it.
pub(crate) fn subject(
    tool: &str,
    call_id: &str,
    parent: Option<&str>,
    input: &Value,
    home: &Path,
) -> Option<ApprovalSubject> {
    let action = action_class(tool);
    let command = match action {
        ActionClass::Command => Some(text(input, "command")??.to_owned()),
        _ => None,
    };
    let paths = match action {
        ActionClass::Edit | ActionClass::Read => text(input, "path")?
            .map(|path| opened_paths(path, home))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    Some(ApprovalSubject {
        action,
        tool: tool.to_owned(),
        command,
        paths,
        raw: json!({
            "toolName": tool,
            "toolCallId": call_id,
            "parentToolCallId": parent,
            "input": input,
        }),
    })
}

/// The text argument `key`: `Some(None)` when it is absent, `None` when it is there and is not
/// text.
fn text<'a>(input: &'a Value, key: &str) -> Option<Option<&'a str>> {
    match input.get(key) {
        None => Some(None),
        Some(value) => value.as_str().map(Some),
    }
}

/// The paths the protection should judge for a tool's `path` argument, spelled as Pi will open
/// it, in Pi's order: Unicode spaces become plain spaces; one leading `@` is dropped; a leading `~`
/// or `~/` is expanded against `home`; then a `file://` URL is read as node's `fileURLToPath`
/// reads it. A URL that call would refuse (another host, an encoded `/`) still yields its decoded
/// path and, besides, the text as written, so a protected component in either form is seen.
fn opened_paths(path: &str, home: &Path) -> Vec<PathBuf> {
    let spaced: String = path
        .chars()
        .map(|c| if is_unicode_space(c) { ' ' } else { c })
        .collect();
    let path = spaced.strip_prefix('@').unwrap_or(&spaced);
    if path == "~" {
        return vec![home.to_path_buf()];
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return vec![home.join(rest)];
    }
    file_url_paths(path).unwrap_or_else(|| vec![PathBuf::from(path)])
}

/// The characters Pi turns into a plain space first (`/[\u00A0\u2000-\u200A\u202F\u205F\u3000]/`).
fn is_unicode_space(c: char) -> bool {
    matches!(
        c,
        '\u{a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// The WHATWG reading of a `file://` URL that Pi hands to `fileURLToPath`: trailing control
/// characters and spaces go, tab, newline and carriage return are deleted, `\` is a `/`, the query
/// and fragment are dropped, a `localhost` host (any case) is the local one, and the path is
/// percent-decoded. `None` when `path` is not such a URL.
fn file_url_paths(path: &str) -> Option<Vec<PathBuf>> {
    let rest = path.strip_prefix("file://")?;
    let cleaned: String = rest
        .trim_end_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .map(|c| if c == '\\' { '/' } else { c })
        .collect();
    let cleaned = cleaned.split(['?', '#']).next().unwrap_or_default();
    let (host, tail) = cleaned.split_at(cleaned.find('/').unwrap_or(cleaned.len()));
    let tail = if tail.is_empty() { "/" } else { tail };
    let decoded = PathBuf::from(percent_decode(tail));
    let refused = !(host.is_empty() || host.eq_ignore_ascii_case("localhost"))
        || tail.to_ascii_lowercase().contains("%2f");
    Some(if refused {
        vec![decoded, PathBuf::from(path)]
    } else {
        vec![decoded]
    })
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let byte = (bytes[at] == b'%')
            .then(|| bytes.get(at + 1..at + 3))
            .flatten()
            .filter(|pair| pair.iter().all(u8::is_ascii_hexdigit))
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        decoded.push(byte.unwrap_or(bytes[at]));
        at += if byte.is_some() { 3 } else { 1 };
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::Path;

    use super::*;

    /// A call as the driver makes it, with a constructed home.
    fn subject(tool: &str, call_id: &str, parent: Option<&str>, input: &Value) -> ApprovalSubject {
        super::subject(tool, call_id, parent, input, Path::new("/home/u")).unwrap()
    }

    fn paths(subject: &ApprovalSubject) -> Vec<&str> {
        subject.paths.iter().map(|p| p.to_str().unwrap()).collect()
    }

    #[test]
    fn a_shell_tool_is_a_command_with_its_command_line() {
        for tool in ["bash", "powershell"] {
            let subject = subject(tool, "c1", None, &json!({ "command": "git status" }));
            assert_eq!(subject.action, ActionClass::Command, "{tool}");
            assert_eq!(subject.tool, tool);
            assert_eq!(subject.command.as_deref(), Some("git status"));
            assert!(subject.paths.is_empty());
        }
    }

    #[test]
    fn a_reading_tool_is_a_read_of_its_path_and_of_nothing_when_it_names_none() {
        for tool in ["read", "ls", "grep", "find"] {
            let named = subject(tool, "c1", None, &json!({ "path": "src/lib.rs" }));
            assert_eq!(named.action, ActionClass::Read, "{tool}");
            assert_eq!(paths(&named), ["src/lib.rs"], "{tool}");
            assert_eq!(named.command, None);

            let unnamed = subject(tool, "c2", None, &json!({ "pattern": "todo" }));
            assert_eq!(unnamed.action, ActionClass::Read, "{tool}");
            assert!(
                unnamed.paths.is_empty(),
                "{tool}: the working directory is no path"
            );
        }
    }

    #[test]
    fn a_path_is_what_pi_will_open_not_what_the_model_typed() {
        let rows: &[(&str, &[&str])] = &[
            ("@.reno/policy.toml", &[".reno/policy.toml"]),
            ("@.git/hooks/pre-commit", &[".git/hooks/pre-commit"]),
            ("@@x", &["@x"]),
            ("@", &[""]),
            ("src/lib.rs", &["src/lib.rs"]),
            ("/work/.reno/x", &["/work/.reno/x"]),
            ("docs/a@b/c", &["docs/a@b/c"]),
            ("~/.reno/x", &["/home/u/.reno/x"]),
            ("@~/.git/x", &["/home/u/.git/x"]),
            (
                "~/Library/Application Support/Reno/x",
                &["/home/u/Library/Application Support/Reno/x"],
            ),
            ("~", &["/home/u"]),
            ("~/", &["/home/u/"]),
            ("~x/.reno", &["~x/.reno"]),
            ("file:///work/.reno/x", &["/work/.reno/x"]),
            ("file:///work/%2Ereno/x", &["/work/.reno/x"]),
            ("file://localhost/work/%2egit/x", &["/work/.git/x"]),
            ("file://LOCALHOST/work/.git/x", &["/work/.git/x"]),
            ("@file:///work/%2Ereno/x", &["/work/.reno/x"]),
            (
                "file:///work/.reno\\policy.toml",
                &["/work/.reno/policy.toml"],
            ),
            ("file://\\work\\.reno\\x", &["/work/.reno/x"]),
            ("file:///work/.re\tno/x", &["/work/.reno/x"]),
            ("file:///work/.re\nno/x", &["/work/.reno/x"]),
            ("file:///work/.reno\r/x", &["/work/.reno/x"]),
            ("file:///work/.reno?x", &["/work/.reno"]),
            ("file:///work/.git#x", &["/work/.git"]),
            ("file:///work/.reno/x?y/z", &["/work/.reno/x"]),
            (
                "file:///work/a%2F.reno/x",
                &["/work/a/.reno/x", "file:///work/a%2F.reno/x"],
            ),
            ("file:///work/%zz/x", &["/work/%zz/x"]),
            (
                "file://other/work/.reno/x",
                &["/work/.reno/x", "file://other/work/.reno/x"],
            ),
            ("file:///work/.git\u{a0}", &["/work/.git"]),
            ("file:///work/.git\u{3000}", &["/work/.git"]),
            ("file:///work/.git\u{2003}\u{202f}", &["/work/.git"]),
            ("file:///work/.git \t", &["/work/.git"]),
            ("file:///work/.git\u{a0}\t ", &["/work/.git"]),
            ("@file:///work/.reno\u{3000} ", &["/work/.reno"]),
            ("\u{a0}file:///work/.reno/x", &[" file:///work/.reno/x"]),
            ("docs/a\u{a0}b/c", &["docs/a b/c"]),
            ("file:///work/%+f/.reno", &["/work/%+f/.reno"]),
            ("file:///work/%2/.reno", &["/work/%2/.reno"]),
            ("FILE:///work/.reno/x", &["FILE:///work/.reno/x"]),
            ("file:/work/.reno/x", &["file:/work/.reno/x"]),
        ];
        for tool in ["read", "ls", "grep", "find", "edit", "write"] {
            for &(typed, opened) in rows {
                let subject = subject(tool, "c1", None, &json!({ "path": typed }));
                assert_eq!(paths(&subject), opened, "{tool} {typed:?}");
                assert_eq!(
                    subject.raw["input"]["path"], typed,
                    "{tool}: raw is as sent"
                );
            }
        }
    }

    #[test]
    fn a_search_pattern_is_not_a_path() {
        let subject = subject(
            "find",
            "c1",
            None,
            &json!({ "pattern": "*.rs", "glob": "src/**" }),
        );
        assert!(subject.paths.is_empty());
        assert_eq!(subject.raw["input"]["pattern"], "*.rs");
    }

    #[test]
    fn a_writing_tool_is_an_edit_of_its_path() {
        for tool in ["edit", "write"] {
            let subject = subject(
                tool,
                "c1",
                None,
                &json!({ "path": ".reno/gate.mjs", "content": "x" }),
            );
            assert_eq!(subject.action, ActionClass::Edit, "{tool}");
            assert_eq!(paths(&subject), [".reno/gate.mjs"], "{tool}");
            assert_eq!(subject.command, None);
        }
    }

    #[test]
    fn an_mcp_tool_is_an_mcp_tool_call_by_name() {
        let subject = subject("mcp__x__y", "c1", None, &json!({ "q": 1 }));
        assert_eq!(subject.action, ActionClass::McpTool);
        assert_eq!(subject.tool, "mcp__x__y");
        assert!(subject.paths.is_empty());
        assert_eq!(subject.command, None);
    }

    #[test]
    fn an_unknown_tool_is_other_and_keeps_its_name() {
        let subject = subject(
            "extension_tool",
            "c1",
            None,
            &json!({ "path": "a", "command": "b" }),
        );
        assert_eq!(subject.action, ActionClass::Other);
        assert_eq!(subject.tool, "extension_tool");
        assert_eq!(subject.command, None);
        assert!(subject.paths.is_empty());
    }

    #[test]
    fn raw_is_the_call_verbatim() {
        let input = json!({ "command": "ls", "extra": { "n": [1, null] } });
        let top = subject("bash", "c1", None, &input);
        assert_eq!(
            top.raw,
            json!({
                "toolName": "bash",
                "toolCallId": "c1",
                "parentToolCallId": null,
                "input": input,
            })
        );
        let nested = subject("read", "c2", Some("c1"), &json!({ "path": "a" }));
        assert_eq!(nested.raw["parentToolCallId"], "c1");
    }

    #[test]
    fn a_command_or_path_that_is_not_text_is_a_call_that_cannot_be_read() {
        let rows: &[(&str, Value)] = &[
            ("bash", json!({})),
            ("bash", json!({ "pattern": "ls" })),
            ("powershell", json!({})),
            ("bash", json!({ "command": 7 })),
            ("bash", json!({ "command": null })),
            ("bash", json!({ "command": ["rm", "-rf"] })),
            ("bash", json!({ "command": { "line": "ls" } })),
            ("powershell", json!({ "command": true })),
            ("edit", json!({ "path": 7 })),
            ("write", json!({ "path": null, "content": "x" })),
            ("read", json!({ "path": [".reno/policy.toml"] })),
            ("read", json!({ "path": { "p": ".git/config" } })),
            ("ls", json!({ "path": false })),
            ("grep", json!({ "path": 1, "pattern": "x" })),
            ("find", json!({ "path": null, "pattern": "x" })),
        ];
        for (tool, input) in rows {
            assert!(
                super::subject(tool, "c1", None, input, Path::new("/home/u")).is_none(),
                "{tool} {input}"
            );
        }
    }

    #[test]
    fn a_search_with_no_path_and_an_unknown_tool_are_still_read() {
        let no_path = subject("ls", "c2", None, &json!({}));
        assert!(no_path.paths.is_empty());
        let other = subject(
            "extension_tool",
            "c3",
            None,
            &json!({ "path": 1, "command": 2 }),
        );
        assert_eq!(other.action, ActionClass::Other);
    }
}
