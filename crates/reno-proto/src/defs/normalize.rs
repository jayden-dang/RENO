//! The command normalizer: what a command line really runs, read before every command match.
//!
//! A harness reports a command as it was typed or generated, and that is often a wrapper:
//! Codex says `/usr/bin/zsh -lc 'git push'`, Claude writes `sudo -n rm -rf x`, `env A=1 git push`
//! or `timeout 5 git reset --hard`. A glob over the whole line never sees the program inside, so
//! a deny for `git reset --hard*` would miss it and an allow for `echo *` would not cover a
//! wrapped `echo hi`. This module reads through the wrappers. It answers two different questions,
//! and they are deliberately not the same. It is pure: no file, no clock (ARCH-1).
//!
//! # The allow view ([`allow_form`])
//!
//! One simple command, or `None`. Used for `allow` rules and for the command a kept standing
//! rule stores. It only removes wrappers that cannot change *which* program runs or with what
//! privilege, and it **never matches more than the line was already allowed to**: a line it
//! cannot read with confidence yields `None`, and the caller then judges the raw line exactly as
//! before.
//!
//! - `sh|bash|zsh|dash|ksh|ash [-l] [-i] -c '<script>'` (also `-lc`, `-ic`) is replaced by the
//!   script, recursively, with quotes decoded. Extra operands (`$0`, `$1`) or any other option
//!   make it `None`. The script must itself be one simple command (or a pure output filter
//!   pipeline), so `zsh -lc 'echo hi; rm x'` is not allowed by `echo *`.
//! - `timeout [opts] DURATION`, `nohup`, `time`, `nice [-n N | -N]`, `stdbuf -o L`, and a bare
//!   `command` or `exec` are removed.
//! - An absolute program path in a system directory (`/bin`, `/usr/bin`, `/usr/local/bin`,
//!   `/sbin`, `/usr/sbin`, `/opt/homebrew/bin`) is reduced to the program name. Any other path
//!   (`./git`, `/tmp/x/git`) is left alone: it is not the program a rule author meant.
//! - **Not removed:** `env`, `VAR=val`, `sudo`, `xargs`, `find -exec`. They change the
//!   environment, the privilege or what runs, so `cargo *` does not allow `sudo cargo test`.
//! - A line that is not confidently one command is `None`: an unbalanced quote, a backtick,
//!   `$(`, a here-doc or any redirect, a chain, a newline, and an inner `eval`/`source`/`.`
//!   found by unwrapping.
//!
//! # The deny view ([`deny_forms`])
//!
//! Every simple command a line may run, as several strings. Used for `deny` and `ask` rules,
//! which match when **any** of them matches, so over-reading is safe. It peels everything the
//! allow view does and also `env`, `VAR=val`, `sudo`/`doas`, `xargs`, `find -exec/-execdir/-ok`,
//! `builtin`, `setsid`, `ionice`, `eval`, and any program path (`/bin/rm` becomes `rm`), and it
//! reads quotes leniently: an unbalanced quote never stops it. Each peel level is a form, so a
//! rule for `git push*` matches `sudo timeout 5 /usr/bin/git push`.

use super::eval::chains_or_redirects;
use super::pipeline::pure_pipeline_head;

/// How deep wrappers may nest before the reading stops.
const MAX_DEPTH: u8 = 8;
/// The most forms the deny view collects.
const MAX_FORMS: usize = 512;
/// Directories whose programs the allow view reads by name.
const SYSTEM_DIRS: &[&str] = &[
    "/bin",
    "/usr/bin",
    "/usr/local/bin",
    "/sbin",
    "/usr/sbin",
    "/opt/homebrew/bin",
];
/// The shells whose `-c` script is read.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "ash"];

// ---- words -----------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(super) struct Word {
    /// The word with its quotes and escapes decoded.
    pub(super) text: String,
    /// Byte range of the word as written.
    start: usize,
    end: usize,
}

pub(super) struct Words {
    pub(super) words: Vec<Word>,
    /// Whether the text read cleanly: balanced quotes, and none of the things a shell treats
    /// specially that this scanner does not follow.
    clean: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Q {
    None,
    Single,
    Double,
}

/// Splits `text` into shell words. Lenient: it never fails, and `clean` says whether to trust it.
pub(super) fn tokenize(text: &str) -> Words {
    let mut words = Vec::new();
    let mut clean = true;
    let mut chars = text.char_indices().peekable();
    let mut current: Option<Word> = None;
    let mut quote = Q::None;
    while let Some((at, c)) = chars.next() {
        if quote == Q::None && c.is_whitespace() {
            if matches!(c, '\n' | '\r') {
                clean = false;
            }
            if let Some(done) = current.take() {
                words.push(Word { end: at, ..done });
            }
            continue;
        }
        let word = current.get_or_insert_with(|| Word {
            text: String::new(),
            start: at,
            end: at,
        });
        word.end = at + c.len_utf8();
        match quote {
            Q::Single => {
                if c == '\'' {
                    quote = Q::None;
                } else {
                    word.text.push(c);
                }
            }
            Q::Double => match c {
                '"' => quote = Q::None,
                '$' | '`' => {
                    clean = false;
                    word.text.push(c);
                }
                '\\' => {
                    clean = false;
                    match chars.next() {
                        Some((i, n)) => {
                            word.end = i + n.len_utf8();
                            if !matches!(n, '"' | '\\' | '$' | '`') {
                                word.text.push('\\');
                            }
                            word.text.push(n);
                        }
                        None => word.text.push('\\'),
                    }
                }
                _ => word.text.push(c),
            },
            Q::None => match c {
                '\'' => quote = Q::Single,
                '"' => quote = Q::Double,
                '\\' => match chars.next() {
                    Some((_, '\n' | '\r')) | None => clean = false,
                    Some((i, n)) => {
                        word.end = i + n.len_utf8();
                        word.text.push(n);
                    }
                },
                '`' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => {
                    clean = false;
                    word.text.push(c);
                }
                '$' => {
                    if chars.peek().map(|&(_, n)| n) == Some('(') {
                        clean = false;
                    }
                    word.text.push(c);
                }
                _ => word.text.push(c),
            },
        }
    }
    if quote != Q::None {
        clean = false;
    }
    words.extend(current);
    Words { words, clean }
}

pub(super) fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// The program name for the allow view: the word itself, or its basename when it sits in a
/// system directory. `None` for any other path.
fn allow_program(word: &str) -> Option<&str> {
    if !word.contains('/') {
        return Some(word);
    }
    let (dir, name) = word.rsplit_once('/')?;
    (!name.is_empty() && SYSTEM_DIRS.contains(&dir)).then_some(name)
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_duration(word: &str) -> bool {
    let digits = word.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(word);
    digits.chars().any(|c| c.is_ascii_digit())
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// `-c`, `-lc`, `-ic`, `-cl`: a short cluster of `l`, `i`, `c` with a `c` in it.
fn is_command_flag(word: &str) -> bool {
    word.strip_prefix('-').is_some_and(|rest| {
        rest.contains('c') && rest.chars().all(|c| matches!(c, 'l' | 'i' | 'c'))
    })
}

// ---- the allow view --------------------------------------------------------------------------

/// What the allow view makes of `line`: the one simple command it runs, with shell wrappers,
/// benign prefixes and system program paths removed. `None` when it cannot say with confidence.
pub(crate) fn allow_form(line: &str) -> Option<String> {
    allow_at(line, 0).map(|(form, _)| form)
}

/// The allow form, only when a wrapper or path was actually removed. What a kept standing rule
/// stores: a line that needed no reading is kept as written.
pub fn unwrapped_command(line: &str) -> Option<String> {
    allow_at(line, 0)
        .filter(|(_, changed)| *changed)
        .map(|(form, _)| form)
}

fn allow_at(line: &str, depth: u8) -> Option<(String, bool)> {
    // Exactness matters to a rule: padding is something the plain reading never forgave.
    if depth > MAX_DEPTH || line != line.trim() {
        return None;
    }
    let head = if chains_or_redirects(line) {
        pure_pipeline_head(line)?
    } else {
        line
    }
    .trim();
    let toks = tokenize(head);
    if !toks.clean || toks.words.is_empty() {
        return None;
    }
    let words = &toks.words;
    let at = |n: usize| words.get(n).map(|w| w.text.as_str());
    let mut i = 0;
    let mut changed = depth > 0;
    while let Some(program) = at(i).and_then(allow_program) {
        match program {
            p if SHELLS.contains(&p) => {
                let mut j = i + 1;
                while matches!(at(j), Some("-l" | "-i")) {
                    j += 1;
                }
                if !at(j).is_some_and(is_command_flag) {
                    break;
                }
                let script = at(j + 1)?;
                if words.len() != j + 2 {
                    return None;
                }
                return allow_at(script, depth + 1).map(|(form, _)| (form, true));
            }
            "timeout" => {
                let mut j = i + 1;
                loop {
                    match at(j) {
                        Some("--foreground" | "--preserve-status" | "-v" | "--verbose") => j += 1,
                        Some("-k" | "-s") => j += 2,
                        _ => break,
                    }
                }
                if !at(j).is_some_and(is_duration) {
                    break;
                }
                i = j + 1;
            }
            "nohup" => i += 1,
            "time" => {
                i += 1;
                if at(i) == Some("-p") {
                    i += 1;
                }
            }
            "command" | "exec" => {
                if at(i + 1).is_none_or(|next| next.starts_with('-')) {
                    break;
                }
                i += 1;
            }
            "nice" => {
                if at(i + 1) == Some("-n") && at(i + 2).is_some_and(|n| n.parse::<i32>().is_ok()) {
                    i += 3;
                } else if at(i + 1).is_some_and(|n| {
                    n.strip_prefix('-')
                        .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
                }) {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "stdbuf" => {
                let mut j = i + 1;
                while let Some(flag) = at(j) {
                    match flag {
                        "-i" | "-o" | "-e" => j += 2,
                        f if f.len() > 2 && ["-i", "-o", "-e"].iter().any(|p| f.starts_with(p)) => {
                            j += 1;
                        }
                        _ => break,
                    }
                }
                if j == i + 1 {
                    break;
                }
                i = j;
            }
            _ => break,
        }
        changed = true;
        if i >= words.len() {
            return None;
        }
    }
    let word = words.get(i)?;
    let program = allow_program(&word.text);
    let form = match program {
        Some(name) if name != word.text => {
            changed = true;
            format!("{name}{}", &head[word.end..])
        }
        _ => head[word.start..].to_owned(),
    };
    if changed && matches!(program, Some("eval" | "source" | ".")) {
        return None;
    }
    Some((form.trim().to_owned(), changed))
}

// ---- the deny view ---------------------------------------------------------------------------

/// Every form of every simple command `line` may run, for a `deny` or `ask` rule to match. The
/// caller also tries the whole line and its crude cuts; this adds what wrappers hide.
pub(crate) fn deny_forms(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    collect(line, 0, &mut out);
    out.sort();
    out.dedup();
    out
}

fn collect(text: &str, depth: u8, out: &mut Vec<String>) {
    if depth > MAX_DEPTH || out.len() > MAX_FORMS {
        return;
    }
    for segment in split_segments(text) {
        peel(segment, depth, out);
    }
}

/// Cuts `text` where another command starts, aware of quotes: single quotes are literal, double
/// quotes still run a backtick or `$(`, and a backslash escapes the next character.
fn split_segments(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote = Q::None;
    let mut prev = '\0';
    let mut escaped = false;
    for (at, c) in text.char_indices() {
        let mut brk = false;
        if escaped {
            escaped = false;
        } else {
            match (quote, c) {
                (Q::Single, '\'') | (Q::Double, '"') => quote = Q::None,
                (Q::Single, _) => {}
                (Q::None, '\'') => quote = Q::Single,
                (_, '"') => quote = Q::Double,
                (_, '\\') => escaped = true,
                (_, '`' | ')') => brk = true,
                (Q::None, '(') => brk = true,
                (Q::Double, '(') => brk = prev == '$',
                (Q::None, ';' | '&' | '|' | '\n' | '\r') => brk = true,
                _ => {}
            }
        }
        prev = c;
        if brk {
            parts.push(&text[start..at]);
            start = at + c.len_utf8();
        }
    }
    parts.push(&text[start..]);
    parts
}

/// Flags of `sudo`/`doas` that take a value.
const SUDO_VALUED: &[&str] = &[
    "-u", "-g", "-h", "-p", "-C", "-D", "-R", "-T", "-U", "-r", "-t", "-a",
];
/// Flags of `xargs` that take a value.
const XARGS_VALUED: &[&str] = &[
    "-n", "-I", "-L", "-P", "-d", "-s", "-E", "-a", "-J", "-R", "-S",
];

/// Records the forms of one simple command, and recurses into whatever it wraps.
fn peel(segment: &str, depth: u8, out: &mut Vec<String>) {
    let toks = tokenize(segment);
    let words = &toks.words;
    let text = |n: usize| words.get(n).map(|w| w.text.as_str());
    let mut i = 0;
    // `xargs` appends operands, so what it runs also has at least one.
    let mut appended = false;
    while i < words.len() {
        let word = &words[i];
        let reduced = basename(&word.text);
        let mut forms = vec![segment[word.start..].trim().to_owned()];
        let mut decoded: Vec<&str> = vec![reduced];
        decoded.extend(words[i + 1..].iter().map(|w| w.text.as_str()));
        forms.push(decoded.join(" "));
        if reduced != word.text {
            forms.push(
                format!("{reduced}{}", &segment[word.end..])
                    .trim()
                    .to_owned(),
            );
        }
        if appended {
            let more: Vec<String> = forms.iter().map(|form| format!("{form} {{}}")).collect();
            forms.extend(more);
        }
        out.extend(forms);
        if is_assignment(&word.text) {
            i += 1;
            continue;
        }
        // Skip options from `j`; the ones in `valued` also swallow the next word.
        let skip_flags = |mut j: usize, valued: &[&str]| {
            while let Some(flag) = text(j) {
                if flag == "--" {
                    j += 1;
                    break;
                }
                if !flag.starts_with('-') || flag == "-" {
                    break;
                }
                j += if valued.contains(&flag) { 2 } else { 1 };
            }
            j
        };
        match reduced {
            "env" => {
                // `env -S 'string'` splits a string into a command.
                if let Some(at) = (i + 1..words.len()).find(|&n| text(n) == Some("-S"))
                    && let Some(script) = text(at + 1)
                {
                    collect(script, depth + 1, out);
                }
                i = skip_flags(i + 1, &["-u", "-C", "-S", "-P"]);
            }
            "sudo" | "doas" | "pkexec" => i = skip_flags(i + 1, SUDO_VALUED),
            "timeout" => {
                i = skip_flags(i + 1, &["-s", "-k", "--signal", "--kill-after"]);
                i += 1; // the duration
            }
            "nice" | "ionice" => i = skip_flags(i + 1, &["-n", "-c", "-p"]),
            "stdbuf" => i = skip_flags(i + 1, &["-i", "-o", "-e"]),
            "exec" => i = skip_flags(i + 1, &["-a"]),
            "nohup" | "time" | "command" | "builtin" | "setsid" | "caffeinate" => {
                i = skip_flags(i + 1, &[]);
            }
            "xargs" => {
                i = skip_flags(i + 1, XARGS_VALUED);
                appended = true;
            }
            "find" => {
                let mut n = i + 1;
                while n < words.len() {
                    if matches!(text(n), Some("-exec" | "-execdir" | "-ok" | "-okdir")) {
                        let from = n + 1;
                        let to = (from..words.len())
                            .find(|&k| matches!(text(k), Some(";" | "+")))
                            .unwrap_or(words.len());
                        if let (Some(first), true) = (words.get(from), from < to) {
                            collect(&segment[first.start..words[to - 1].end], depth + 1, out);
                        }
                        n = to;
                    }
                    n += 1;
                }
                return;
            }
            "eval" => {
                let joined: Vec<&str> = words[i + 1..].iter().map(|w| w.text.as_str()).collect();
                collect(&joined.join(" "), depth + 1, out);
                return;
            }
            p if SHELLS.contains(&p) => {
                let flag = (i + 1..words.len())
                    .take_while(|&n| text(n).is_some_and(|t| t.starts_with('-')))
                    .find(|&n| {
                        text(n).is_some_and(|t| !t.starts_with("--") && t[1..].contains('c'))
                    });
                if let Some(script) = flag.and_then(|n| text(n + 1)) {
                    collect(script, depth + 1, out);
                }
                return;
            }
            _ => return,
        }
    }
}

// ---- helpers shared with the harness ---------------------------------------------------------

/// Joins an argv into one command line a shell reads back as the same words: a part with
/// anything but safe characters is single-quoted. For a harness that reports an array.
pub fn shell_join<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    parts
        .into_iter()
        .map(|part| {
            let safe = !part.is_empty()
                && part.chars().all(|c| {
                    c.is_ascii_alphanumeric()
                        || matches!(c, '/' | '.' | '-' | '_' | '=' | ':' | ',' | '@' | '+' | '%')
                });
            if safe {
                part.to_owned()
            } else {
                format!("'{}'", part.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(line: &str) -> Option<String> {
        allow_form(line)
    }

    #[test]
    fn the_allow_view_unwraps_shells_wrappers_and_system_paths() {
        let cases: &[(&str, Option<&str>)] = &[
            ("echo hi", Some("echo hi")),
            ("/usr/bin/zsh -lc 'echo hi'", Some("echo hi")),
            ("/bin/bash -c \"echo hi there\"", Some("echo hi there")),
            ("bash -c \"sh -c 'git status'\"", Some("git status")),
            ("zsh -l -c 'git status'", Some("git status")),
            ("timeout 5 git status", Some("git status")),
            ("timeout -k 2 5 git status", Some("git status")),
            ("nohup nice -n 5 git status", Some("git status")),
            ("time git status", Some("git status")),
            ("/bin/rm x", Some("rm x")),
            ("/usr/bin/env git status", Some("env git status")),
            ("zsh -lc 'cargo test 2>&1 | tail -5'", Some("cargo test")),
            // Not confidently one command.
            ("zsh -lc 'echo hi; rm x'", None),
            ("zsh -lc 'echo hi' extra", None),
            ("zsh -lc \"echo $HOME\"", None),
            ("zsh -lc 'echo \"hi'", None),
            ("zsh -lc 'eval x'", None),
            ("bash -c 'echo `id`'", None),
            ("bash -c 'cat <<EOF'", None),
            ("bash -c 'echo hi", None),
            ("echo hi; rm x", None),
        ];
        for (line, want) in cases {
            assert_eq!(allow(line).as_deref(), *want, "{line}");
        }
    }

    #[test]
    fn the_allow_view_keeps_what_changes_privilege_or_environment() {
        for line in [
            "sudo git status",
            "env A=1 git status",
            "A=1 git status",
            "xargs git status",
            "./git status",
            "/tmp/evil/git status",
        ] {
            assert_eq!(allow(line).as_deref(), Some(line), "{line}");
        }
    }

    #[test]
    fn unwrapped_command_is_only_for_lines_that_needed_reading() {
        assert_eq!(unwrapped_command("git status"), None);
        assert_eq!(
            unwrapped_command("/usr/bin/zsh -lc 'git status'").as_deref(),
            Some("git status")
        );
        assert_eq!(unwrapped_command("cargo test 2>&1 | tail"), None);
    }

    fn deny_has(line: &str, want: &str) -> bool {
        deny_forms(line).iter().any(|form| form == want)
    }

    #[test]
    fn the_deny_view_finds_the_command_inside_every_wrapper() {
        let cases = [
            (
                "/usr/bin/zsh -lc 'git push origin main'",
                "git push origin main",
            ),
            ("bash -c \"sh -c 'git push'\"", "git push"),
            ("env A=1 B=2 git push", "git push"),
            ("A=1 git push", "git push"),
            ("sudo -n rm -rf /", "rm -rf /"),
            ("sudo -u root rm -rf /", "rm -rf /"),
            ("timeout 5 git reset --hard", "git reset --hard"),
            ("nohup nice -n 3 git push", "git push"),
            ("/bin/rm -rf /", "rm -rf /"),
            ("./rm -rf /", "rm -rf /"),
            ("ls && zsh -lc 'git push'", "git push"),
            ("echo x | xargs -n1 rm -rf", "rm -rf"),
            ("find . -name x -exec rm -rf {} \\;", "rm -rf {}"),
            ("find . -exec git push {} +", "git push {}"),
            ("eval 'git push'", "git push"),
            ("echo \"$(git push)\"", "git push"),
            ("sudo bash -c 'git push; ls'", "git push"),
            ("bash -c 'git push", "git push"),
            ("env -S 'git push'", "git push"),
        ];
        for (line, want) in cases {
            assert!(deny_has(line, want), "{line} -> {:?}", deny_forms(line));
        }
    }

    #[test]
    fn shell_join_round_trips_through_the_allow_view() {
        let line = shell_join(["/usr/bin/zsh", "-lc", "git commit -m 'it works'"]);
        assert_eq!(allow(&line).as_deref(), Some("git commit -m 'it works'"));
    }
}
