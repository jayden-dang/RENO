//! What a click on a link in rendered markdown does (TRANSCRIPT-8.1, 8.2, 8.5, 16.6).
//!
//! The renderer takes a [`LinkPolicy`] from its host. The transcript's policy opens a web link
//! only through the app's link opener ([`crate::links::open`], which tests forbid from reaching
//! a real browser), hands a link or an inline-code path that names a worktree file to the host's
//! [`FileLinks`], and leaves everything else (`mailto:`, custom schemes, absolute paths) as
//! plain unlinked text. The Files tab's preview keeps the policy it always had.

use std::rc::Rc;

use gpui::{App, Window};
use reno_markdown::mend::PENDING_LINK_URL;

use crate::links;

/// A file in the run's worktree a link points at, and the line it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileTarget {
    /// Relative to the worktree root, forward slashes, no `.` or `..` parts.
    pub(crate) path: String,
    /// One-based.
    pub(crate) line: Option<u32>,
}

/// What the host does with a worktree file: whether it exists, and opening it. The transcript
/// wires this to the side panel's Files tab (`open_file_at`).
pub(crate) trait FileLinks {
    /// Whether `path` (worktree-relative) names a file the run has. An inline-code path is only
    /// made clickable when this says so.
    fn resolves(&self, path: &str, cx: &App) -> bool;
    /// Opens `target` in the Files tab, at its line.
    fn open(&self, target: FileTarget, window: &mut Window, cx: &mut App);
}

/// How a host wants links handled.
#[derive(Clone)]
pub(crate) enum LinkPolicy {
    /// The Files tab's preview: `http`, `https` and `mailto` links open straight away; nothing
    /// else does. (Its behaviour before the transcript had a policy.)
    Preview,
    /// The transcript: web links through [`links::open`], worktree files through `files` (when
    /// the host has one), nothing else.
    Transcript { files: Option<Rc<dyn FileLinks>> },
}

impl std::fmt::Debug for LinkPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preview => f.write_str("Preview"),
            Self::Transcript { files } => f
                .debug_struct("Transcript")
                .field("files", &files.is_some())
                .finish(),
        }
    }
}

/// What a link does, decided from its destination alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Open a web page through the link opener.
    Web(String),
    /// The Files tab's preview opens it directly (`http`, `https`, `mailto`).
    Direct(String),
    /// Open a worktree file.
    File(FileTarget),
    /// Not a link: the text is drawn plain.
    Plain,
}

impl LinkPolicy {
    /// What a link to `url` does under this policy. A file link counts only when the host has a
    /// [`FileLinks`]; whether the file exists is the host's to say at click time.
    pub(crate) fn action(&self, url: &str) -> Action {
        if url == PENDING_LINK_URL || url.is_empty() {
            return Action::Plain;
        }
        match self {
            Self::Preview => {
                if super::openable(url) {
                    Action::Direct(url.to_owned())
                } else {
                    Action::Plain
                }
            }
            Self::Transcript { files } => {
                let trimmed = url.trim();
                if links::is_web_link(trimmed) {
                    return Action::Web(trimmed.to_owned());
                }
                match (files, file_target(trimmed)) {
                    (Some(_), Some(target)) => Action::File(target),
                    _ => Action::Plain,
                }
            }
        }
    }

    pub(crate) fn files(&self) -> Option<&Rc<dyn FileLinks>> {
        match self {
            Self::Transcript { files } => files.as_ref(),
            Self::Preview => None,
        }
    }

    /// Runs `action` for a click. Web links go through the link opener only; a worktree file
    /// goes to the host when it still resolves.
    pub(crate) fn run(&self, action: &Action, window: &mut Window, cx: &mut App) {
        match action {
            Action::Web(url) => {
                links::open(cx, url);
            }
            Action::Direct(url) => cx.open_url(url),
            Action::File(target) => {
                if let Some(files) = self.files().cloned()
                    && files.resolves(&target.path, cx)
                {
                    files.open(target.clone(), window, cx);
                }
            }
            Action::Plain => {}
        }
    }
}

/// The worktree file a link destination or an inline-code span names: `path`, `path:line`,
/// `path:line:col`, `path#L12` or `path#L12-L20`. `None` for anything that is not a plain
/// relative path inside the worktree: a URL, an absolute path, one that climbs out with `..`, a
/// bare fragment.
pub(crate) fn file_target(text: &str) -> Option<FileTarget> {
    let text = text.trim();
    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    let (rest, fragment) = match text.split_once('#') {
        Some((rest, fragment)) => (rest, Some(fragment)),
        None => (text, None),
    };
    let mut line = None;
    let mut path = rest;
    if let Some(fragment) = fragment {
        line = Some(fragment_line(fragment)?);
    } else if let Some((head, tail)) = split_line(rest) {
        path = head;
        line = Some(tail);
    }
    let path = percent_decode(path)?;
    clean_path(&path).map(|path| FileTarget { path, line })
}

/// `L12` or `L12-L20` or `L12C3`: the first line.
fn fragment_line(fragment: &str) -> Option<u32> {
    let digits = fragment.strip_prefix('L')?;
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    let line: u32 = digits[..end].parse().ok()?;
    (line > 0).then_some(line)
}

/// `path:12` and `path:12:3` into the path and the line.
fn split_line(text: &str) -> Option<(&str, u32)> {
    let (head, tail) = text.rsplit_once(':')?;
    if let Ok(last) = tail.parse::<u32>() {
        // `path:12:3`: the number before is the line, this one the column.
        if let Some((path, line)) = head.rsplit_once(':')
            && let Ok(line) = line.parse::<u32>()
        {
            return (line > 0).then_some((path, line));
        }
        return (last > 0).then_some((head, last));
    }
    None
}

fn percent_decode(text: &str) -> Option<String> {
    if !text.contains('%') {
        return Some(text.to_owned());
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = text.get(at + 1..at + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The worktree-relative form of `path`: `./` parts dropped, `\` refused, nothing absolute,
/// nothing that climbs with `..`, no scheme, no colon at all.
pub(crate) fn clean_path(path: &str) -> Option<String> {
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('~')
        || path.contains(['\\', ':', '\0', '?'])
    {
        return None;
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Whether the text of an inline-code span could be a worktree path worth asking the host
/// about: a path with a slash, or a name with an extension, and no spaces.
pub(crate) fn looks_like_path(text: &str) -> bool {
    let Some(target) = file_target(text) else {
        return false;
    };
    let name = target.path.rsplit('/').next().unwrap_or_default();
    target.path.contains('/')
        || name.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.is_empty()
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && ext.chars().any(|c| c.is_ascii_alphabetic())
        })
}

/// The worktree files a reply's links and inline-code spans name, without repeats: what the
/// host is asked to resolve.
pub(crate) fn paths_in(tree: &reno_markdown::BlockTree) -> Vec<String> {
    use reno_markdown::{Block, InlineRun};
    fn runs(runs: &[InlineRun], out: &mut Vec<String>) {
        for run in runs {
            let target = match run.style.link.as_deref() {
                Some(dest) if !links::is_web_link(dest.trim()) => file_target(dest),
                Some(_) => None,
                None if run.style.code && looks_like_path(&run.text) => file_target(&run.text),
                None => None,
            };
            if let Some(target) = target
                && !out.contains(&target.path)
            {
                out.push(target.path);
            }
        }
    }
    fn block(b: &Block, out: &mut Vec<String>) {
        match b {
            Block::Paragraph { runs: r } | Block::Heading { runs: r, .. } => runs(r, out),
            Block::BlockQuote { children } => children.iter().for_each(|c| block(c, out)),
            Block::List { items, .. } => items.iter().flatten().for_each(|c| block(c, out)),
            Block::Table { header, rows, .. } => {
                header.iter().for_each(|c| runs(c, out));
                rows.iter().flatten().for_each(|c| runs(c, out));
            }
            Block::CodeBlock { .. } | Block::Math { .. } | Block::Rule => {}
        }
    }
    let mut out = Vec::new();
    tree.blocks.iter().for_each(|t| block(&t.block, &mut out));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_names_the_worktree_files_its_links_and_code_spans_point_at_once_each() {
        let tree = reno_markdown::parse_full(
            "See [a](src/a.rs:4) and `src/a.rs:9` and `b.txt`, [web](https://x.org/y.rs), `cargo`.\n\n- `docs/g.png`",
        );
        assert_eq!(paths_in(&tree), ["src/a.rs", "b.txt", "docs/g.png"]);
    }

    fn target(path: &str, line: Option<u32>) -> Option<FileTarget> {
        Some(FileTarget {
            path: path.to_owned(),
            line,
        })
    }

    #[test]
    fn worktree_paths_and_their_lines_parse() {
        assert_eq!(
            file_target("src/supervisor.rs"),
            target("src/supervisor.rs", None)
        );
        assert_eq!(
            file_target("src/supervisor.rs:42"),
            target("src/supervisor.rs", Some(42))
        );
        assert_eq!(
            file_target("src/supervisor.rs:42:7"),
            target("src/supervisor.rs", Some(42))
        );
        assert_eq!(
            file_target("src/supervisor.rs#L42"),
            target("src/supervisor.rs", Some(42))
        );
        assert_eq!(
            file_target("src/a.rs#L42-L50"),
            target("src/a.rs", Some(42))
        );
        assert_eq!(file_target("./src/a.rs"), target("src/a.rs", None));
        assert_eq!(file_target("a%20b.md"), target("a b.md", None));
        assert_eq!(file_target("README.md:3"), target("README.md", Some(3)));
    }

    #[test]
    fn nothing_that_leaves_the_worktree_is_a_file_target() {
        for bad in [
            "",
            "/etc/passwd",
            "~/notes.md",
            "../secret.txt",
            "src/../../secret",
            "a/../../b",
            "https://example.com/a.rs",
            "mailto:a@example.org",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "C:\\temp\\x",
            "src\\a.rs",
            "#heading",
            "a.rs#section",
            "a.rs:0",
            "%2e%2e/secret",
            "src/a.rs?x=1",
        ] {
            assert_eq!(file_target(bad), None, "{bad}");
        }
    }

    #[test]
    fn inline_code_is_only_a_path_when_it_looks_like_one() {
        assert!(looks_like_path("src/supervisor.rs:42"));
        assert!(looks_like_path("Cargo.toml"));
        assert!(looks_like_path("docs/graph"));
        assert!(!looks_like_path("cargo test"));
        assert!(!looks_like_path("x"));
        assert!(
            !looks_like_path("v1.2"),
            "digits after the dot are not a file name here"
        );
        assert!(!looks_like_path("https://example.com/a"));
    }

    #[test]
    fn the_transcript_opens_web_links_files_and_nothing_else() {
        struct Files;
        impl FileLinks for Files {
            fn resolves(&self, _: &str, _: &App) -> bool {
                true
            }
            fn open(&self, _: FileTarget, _: &mut Window, _: &mut App) {}
        }
        let policy = LinkPolicy::Transcript {
            files: Some(Rc::new(Files)),
        };
        assert_eq!(
            policy.action("https://example.com/x"),
            Action::Web("https://example.com/x".to_owned())
        );
        assert_eq!(
            policy.action("http://example.com"),
            Action::Web("http://example.com".to_owned())
        );
        assert_eq!(policy.action("mailto:a@example.org"), Action::Plain);
        assert_eq!(policy.action("custom://x"), Action::Plain);
        assert_eq!(policy.action("file:///etc/passwd"), Action::Plain);
        assert_eq!(policy.action("/etc/passwd"), Action::Plain);
        assert_eq!(policy.action(PENDING_LINK_URL), Action::Plain);
        assert_eq!(
            policy.action("src/supervisor.rs:42"),
            Action::File(FileTarget {
                path: "src/supervisor.rs".to_owned(),
                line: Some(42)
            })
        );
    }

    #[test]
    fn without_a_file_host_a_file_link_is_plain_text() {
        let policy = LinkPolicy::Transcript { files: None };
        assert_eq!(policy.action("src/a.rs"), Action::Plain);
        assert!(matches!(policy.action("https://a.b"), Action::Web(_)));
    }

    #[test]
    fn the_previews_policy_is_the_one_it_always_had() {
        let policy = LinkPolicy::Preview;
        assert!(matches!(
            policy.action("https://example.org/a"),
            Action::Direct(_)
        ));
        assert!(matches!(
            policy.action("HTTP://example.org"),
            Action::Direct(_)
        ));
        assert!(matches!(
            policy.action("mailto:a@example.org"),
            Action::Direct(_)
        ));
        assert_eq!(policy.action("file:///etc/passwd"), Action::Plain);
        assert_eq!(policy.action("src/a.rs"), Action::Plain);
        assert_eq!(policy.action(PENDING_LINK_URL), Action::Plain);
        assert_eq!(policy.action(""), Action::Plain);
    }
}
