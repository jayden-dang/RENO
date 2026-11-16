//! Pure-pipeline normalisation for `allow` command rules.
//!
//! A coding agent habitually pipes a build or test command through a text filter to keep the
//! output short (`cargo test 2>&1 | tail -40`). The filter changes nothing on disk, so the line
//! is exactly as safe as the command it filters. [`pure_pipeline_head`] recognises such a line
//! and returns the command being filtered, which an `allow` rule then matches in place of the
//! whole line. Everything it does not recognise is left to the ordinary compound-line guard,
//! which refuses it.
//!
//! This is the one place the policy reads a pipe as anything but a chain. See "Commands" in
//! [`super::evaluate`] for the rule as users see it.
//!
//! # The recognised shape
//!
//! `X [2>&1 | 2>/dev/null] | F1 | F2 ...`, where:
//!
//! - `X` is everything before the first unquoted `|`. It is returned with the one trailing
//!   stderr redirection removed, and it must itself pass the ordinary compound-line guard, so
//!   `X` cannot chain, substitute or redirect. An environment prefix
//!   (`FOO=1 cmd`) is **not** stripped: `RUSTC=sh cargo build` and `LD_PRELOAD=x cmd` change
//!   what a command does, so such a line is matched as written, and a glob such as `cargo *`
//!   does not match it.
//! - each `Fi` is a single command from [`FILTERS`], every option of which is on that filter's
//!   allow-list (below). An option, or an operand count, that is not listed rejects the line.
//!   No filter takes a file operand, so none reads or writes a file by name.
//! - the only redirection anywhere is the trailing `2>&1` or `2>/dev/null` on `X`.
//!
//! The line must be free of backslashes (outside single quotes in a filter), backticks, control
//! characters and unbalanced quotes, and a filter of any unquoted `; & < > ( ) { } $ * ? [ ~ ! #`
//! and of `$` inside double quotes. `||`, `|&`, `&&`, `;` and the rest therefore leave the line
//! compound.
//!
//! # The filters
//!
//! | filter | options accepted | operands |
//! |---|---|---|
//! | `head`, `tail` | `-n N`, `-c N`, `-N`, `-q`, `--lines`, `--bytes`; `N` is `[+-]digits` | none |
//! | `grep` | `-E -F -G -P -i -v -n -w -x -c -o -s -h -H -I -a -q -l -L -b -r`, `-A -B -C -m N`, `-N`, `-e PATTERN`, and the long forms of the same | the pattern, unless `-e` is given |
//! | `rg` | `-i -S -s -v -n -w -x -c -o -H -I -F -l -L -q`, `-A -B -C -m N`, `-N`, `-e PATTERN`, long forms | the pattern, unless `-e` |
//! | `wc` | `-l -w -c -m -L` | none |
//! | `sort` | `-b -d -f -g -i -M -h -n -r -R -u -s -V -z -m`, `-k KEY`, `-t SEP` | none |
//! | `uniq` | `-c -d -D -i -u -z`, `-f N`, `-s N`, `-w N` | none |
//! | `cut` | `-b -c -f -d` with a value, `-s`, `-n`, `--complement` | none |
//! | `tr` | `-c -C -d -s` | one or two sets |
//! | `cat` | `-n -b -s -E -T -v -A -e -t -u` | none |
//! | `sed` | `-n` (required), `-E`, `-r`, `--quiet`, `--silent` | one script, see below |
//!
//! Left out on purpose: `awk` (it can run commands and write files), `tee`, `xargs`, `sh`,
//! `bash`, `env`, `find`, `-f`/`--file`, `--output`, `-o` of `sort`, `--compress-program`,
//! `rg --pre`, `rg -z`, `tail -f`, `sed -i`, `sed -f`, `sed -e`, and any file operand.
//!
//! A `sed` script is a `;`-separated list of `[address[,address]][!]command`, with command one
//! of `p`, `d`, `q`, `=`, or `s` with the delimiter `/` or `|` and flags from `g p i I m M` and
//! digits. Addresses are a line number, `$`, `first~step`, a `/regex/` (with `I`/`M`), and for
//! the second address also `+N` or `~N`. Anything else, `w`, `W`, `e`, `r`, `R`, `x`, `{` and
//! `s///w`, `s///e` included, rejects the line. A bracket expression holding the delimiter
//! rejects it too, since `sed` implementations disagree about where such a regex ends.

/// The commands a pipeline may be filtered through.
pub(crate) const FILTERS: &[&str] = &[
    "head", "tail", "grep", "rg", "wc", "sort", "uniq", "cut", "tr", "cat", "sed",
];

/// Long options whose value must be a number.
const NUMERIC_LONG: &[&str] = &[
    "lines",
    "bytes",
    "after-context",
    "before-context",
    "context",
    "max-count",
];

/// The command a pure pipeline filters, with its trailing `2>&1` or `2>/dev/null` removed, or
/// `None` when `line` is not a pure pipeline.
///
/// The returned command has itself passed the compound-line guard (no `; & | ( ) < >`, backtick
/// or `$(`), so it is one simple command. A line with no pipe and no redirection is returned as
/// it is, so the caller need not special-case it.
pub(crate) fn pure_pipeline_head(line: &str) -> Option<&str> {
    let stages = split_stages(line)?;
    let (head, filters) = stages.split_first()?;
    let head = if filters.is_empty() {
        head
    } else {
        head.trim_end()
    };
    let head = strip_stderr_redirect(head);
    if head.trim().is_empty() {
        return None;
    }
    if super::eval::chains_or_redirects(head) {
        return None;
    }
    for stage in filters {
        if !filter_ok(&words(stage)?) {
            return None;
        }
    }
    Some(head)
}

fn strip_stderr_redirect(head: &str) -> &str {
    for redirect in ["2>&1", "2>/dev/null"] {
        if let Some(rest) = head.strip_suffix(redirect)
            && rest.ends_with(char::is_whitespace)
        {
            return rest.trim_end();
        }
    }
    head
}

/// Cuts `line` at its unquoted `|`, checking the quoting on the way. `None` for anything the
/// shell could read differently from this scanner: an open quote, a backslash outside a filter's
/// single quotes, a backtick, a control character, `$` in a filter's double quotes.
fn split_stages(line: &str) -> Option<Vec<&str>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Q {
        None,
        Single,
        Double,
    }
    let mut stages = Vec::new();
    let mut start = 0;
    let mut quote = Q::None;
    for (at, c) in line.char_indices() {
        if c.is_control() || c == '`' {
            return None;
        }
        let in_filter = !stages.is_empty();
        match (quote, c) {
            (Q::None, '\'') => quote = Q::Single,
            (Q::None, '"') => quote = Q::Double,
            (Q::Single, '\'') | (Q::Double, '"') => quote = Q::None,
            (Q::None, '|') => {
                stages.push(&line[start..at]);
                start = at + 1;
            }
            (Q::Single, '\\') if in_filter => {}
            (_, '\\') => return None,
            (Q::Double, '$') if in_filter => return None,
            _ => {}
        }
    }
    if quote != Q::None {
        return None;
    }
    stages.push(&line[start..]);
    Some(stages)
}

/// One word of a filter stage.
struct Word {
    text: String,
    /// Whether any part of it was quoted.
    quoted: bool,
}

fn blank() -> Word {
    Word {
        text: String::new(),
        quoted: false,
    }
}

/// The words of a filter stage. `None` for an unquoted character that is a shell operator, a
/// glob, an expansion or a comment, and for an empty stage.
fn words(stage: &str) -> Option<Vec<Word>> {
    const BAD: &[char] = &[
        ';', '&', '<', '>', '(', ')', '{', '}', '$', '*', '?', '[', '~', '!', '#',
    ];
    let mut out: Vec<Word> = Vec::new();
    let mut current: Option<Word> = None;
    let mut quote: Option<char> = None;
    for c in stage.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.get_or_insert_with(blank).text.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                current.get_or_insert_with(blank).quoted = true;
            }
            None if c.is_whitespace() => out.extend(current.take()),
            None if BAD.contains(&c) => return None,
            None => current.get_or_insert_with(blank).text.push(c),
        }
    }
    out.extend(current);
    (!out.is_empty()).then_some(out)
}

/// How a filter's options are read.
struct Spec {
    /// Short options with no value.
    bare: &'static str,
    /// Short options that take a value, and which of those take a number.
    valued: &'static str,
    numeric: &'static str,
    /// Long options with no value, and with a value (`--name=V` or `--name V`).
    long_bare: &'static [&'static str],
    long_valued: &'static [&'static str],
    /// Whether `-30` alone is accepted (`head -30`, `grep -5`).
    digits: bool,
    /// The fewest and most operands.
    operands: (usize, usize),
    /// The short option that supplies the pattern, which then leaves no operand.
    pattern: Option<char>,
}

const NONE: Spec = Spec {
    bare: "",
    valued: "",
    numeric: "",
    long_bare: &[],
    long_valued: &[],
    digits: false,
    operands: (0, 0),
    pattern: None,
};

const GREP_LONG: &[&str] = &[
    "ignore-case",
    "invert-match",
    "line-number",
    "word-regexp",
    "line-regexp",
    "count",
    "only-matching",
    "no-filename",
    "with-filename",
    "fixed-strings",
    "files-with-matches",
    "files-without-match",
    "quiet",
];

const CONTEXT_LONG: &[&str] = &[
    "after-context",
    "before-context",
    "context",
    "max-count",
    "regexp",
];

fn spec(name: &str) -> Option<Spec> {
    Some(match name {
        "head" | "tail" => Spec {
            bare: "q",
            valued: "nc",
            numeric: "nc",
            long_bare: &["quiet", "silent"],
            long_valued: &["lines", "bytes"],
            digits: true,
            ..NONE
        },
        "grep" => Spec {
            bare: "EFGPivnwxcoshHIaqlLbr",
            valued: "ABCme",
            numeric: "ABCm",
            long_bare: GREP_LONG,
            long_valued: CONTEXT_LONG,
            digits: true,
            operands: (0, 1),
            pattern: Some('e'),
        },
        "rg" => Spec {
            bare: "iSsvnwxcoHIFlLq",
            valued: "ABCme",
            numeric: "ABCm",
            long_bare: GREP_LONG,
            long_valued: CONTEXT_LONG,
            digits: true,
            operands: (0, 1),
            pattern: Some('e'),
        },
        "wc" => Spec {
            bare: "lwcmL",
            long_bare: &["lines", "words", "bytes", "chars", "max-line-length"],
            ..NONE
        },
        "sort" => Spec {
            bare: "bdfgiMhnrRusVzm",
            valued: "kt",
            long_bare: &[
                "numeric-sort",
                "reverse",
                "unique",
                "ignore-case",
                "stable",
                "human-numeric-sort",
                "version-sort",
                "ignore-leading-blanks",
                "dictionary-order",
                "month-sort",
                "general-numeric-sort",
            ],
            ..NONE
        },
        "uniq" => Spec {
            bare: "cdDiuz",
            valued: "fsw",
            numeric: "fsw",
            long_bare: &["count", "repeated", "unique", "ignore-case"],
            ..NONE
        },
        "cut" => Spec {
            bare: "sn",
            valued: "bcfd",
            long_bare: &["complement", "only-delimited"],
            ..NONE
        },
        "tr" => Spec {
            bare: "cCds",
            operands: (1, 2),
            ..NONE
        },
        "cat" => Spec {
            bare: "nbsETvAeut",
            ..NONE
        },
        "sed" => Spec {
            bare: "nEr",
            long_bare: &["quiet", "silent", "regexp-extended"],
            operands: (1, 1),
            ..NONE
        },
        _ => return None,
    })
}

fn is_number(text: &str) -> bool {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Whether one filter stage, as words, is on the allow-list with every option on its list.
fn filter_ok(words: &[Word]) -> bool {
    let Some((name, args)) = words.split_first() else {
        return false;
    };
    if name.quoted || !FILTERS.contains(&name.text.as_str()) {
        return false;
    }
    let Some(spec) = spec(&name.text) else {
        return false;
    };
    let mut operands: Vec<&str> = Vec::new();
    let mut pattern_given = false;
    let mut quiet = false;
    let mut args = args.iter();
    while let Some(word) = args.next() {
        let text = word.text.as_str();
        if let Some(long) = text.strip_prefix("--") {
            let (key, value) = match long.split_once('=') {
                Some((key, value)) => (key, Some(value)),
                None => (long, None),
            };
            if spec.long_bare.contains(&key) && value.is_none() {
                quiet |= matches!(key, "quiet" | "silent");
            } else if spec.long_valued.contains(&key) {
                let Some(value) = value.or_else(|| args.next().map(|w| w.text.as_str())) else {
                    return false;
                };
                if NUMERIC_LONG.contains(&key) && !is_number(value) {
                    return false;
                }
                pattern_given |= key == "regexp";
            } else {
                return false;
            }
        } else if let Some(short) = text.strip_prefix('-').filter(|s| !s.is_empty()) {
            if spec.digits && short.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            for (at, c) in short.char_indices() {
                if spec.bare.contains(c) {
                    quiet |= c == 'n';
                } else if spec.valued.contains(c) {
                    let rest = &short[at + c.len_utf8()..];
                    let Some(value) = (!rest.is_empty())
                        .then_some(rest)
                        .or_else(|| args.next().map(|w| w.text.as_str()))
                    else {
                        return false;
                    };
                    if spec.numeric.contains(c) && !is_number(value) {
                        return false;
                    }
                    pattern_given |= spec.pattern == Some(c);
                    break;
                } else {
                    return false;
                }
            }
        } else if text == "-" {
            return false;
        } else {
            operands.push(text);
        }
    }
    let (least, most) = spec.operands;
    let most = if pattern_given { 0 } else { most };
    if operands.len() > most || operands.len() < least {
        return false;
    }
    if name.text == "sed" {
        return quiet && operands.first().is_some_and(|script| sed_script_ok(script));
    }
    true
}

// ---- sed scripts ------------------------------------------------------------------------------

/// Whether `script` is made only of the `sed` commands the module doc lists.
fn sed_script_ok(script: &str) -> bool {
    let chars: Vec<char> = script.chars().collect();
    let mut at = 0;
    let mut commands = 0;
    loop {
        skip_blanks(&chars, &mut at);
        match chars.get(at) {
            None => return commands > 0,
            Some(';') => {
                at += 1;
                continue;
            }
            Some(_) => {}
        }
        if !address(&chars, &mut at, true) {
            return false;
        }
        if chars.get(at) == Some(&',') {
            at += 1;
            if !address(&chars, &mut at, false) {
                return false;
            }
        }
        skip_blanks(&chars, &mut at);
        if chars.get(at) == Some(&'!') {
            at += 1;
            skip_blanks(&chars, &mut at);
        }
        let Some(&command) = chars.get(at) else {
            return false;
        };
        at += 1;
        match command {
            'p' | 'd' | '=' => {}
            'q' => {
                number(&chars, &mut at);
            }
            's' => {
                let Some(&delim) = chars.get(at).filter(|d| matches!(d, '/' | '|')) else {
                    return false;
                };
                at += 1;
                if !regex_part(&chars, &mut at, delim) || !regex_part(&chars, &mut at, delim) {
                    return false;
                }
                while chars.get(at).is_some_and(|c| {
                    matches!(c, 'g' | 'p' | 'i' | 'I' | 'm' | 'M') || c.is_ascii_digit()
                }) {
                    at += 1;
                }
            }
            _ => return false,
        }
        commands += 1;
        if commands > 64 {
            return false;
        }
        skip_blanks(&chars, &mut at);
        match chars.get(at) {
            None => return true,
            Some(';') => at += 1,
            Some(_) => return false,
        }
    }
}

fn skip_blanks(chars: &[char], at: &mut usize) {
    while chars.get(*at).is_some_and(|c| *c == ' ' || *c == '\t') {
        *at += 1;
    }
}

fn number(chars: &[char], at: &mut usize) -> bool {
    let from = *at;
    while chars.get(*at).is_some_and(char::is_ascii_digit) {
        *at += 1;
    }
    *at > from
}

/// Reads an address. The first one may be absent (`first`); a malformed one is `false`. The
/// second address also accepts `+N` and `~N`.
fn address(chars: &[char], at: &mut usize, first: bool) -> bool {
    match chars.get(*at) {
        Some('$') => *at += 1,
        Some(c) if c.is_ascii_digit() => {
            number(chars, at);
            if chars.get(*at) == Some(&'~') {
                *at += 1;
                if !number(chars, at) {
                    return false;
                }
            }
        }
        Some('+' | '~') if !first => {
            *at += 1;
            if !number(chars, at) {
                return false;
            }
        }
        Some('/') => {
            *at += 1;
            if !regex_part(chars, at, '/') {
                return false;
            }
            while chars.get(*at).is_some_and(|c| matches!(c, 'I' | 'M')) {
                *at += 1;
            }
        }
        _ => return first,
    }
    true
}

/// Reads up to and past the closing `delim`. A bracket expression containing `delim`, or a
/// backslash before it inside one, is refused: implementations disagree on where it ends.
fn regex_part(chars: &[char], at: &mut usize, delim: char) -> bool {
    let mut in_bracket = false;
    while let Some(&c) = chars.get(*at) {
        *at += 1;
        if in_bracket {
            match c {
                c if c == delim => return false,
                '\\' if chars.get(*at) == Some(&delim) => return false,
                ']' => in_bracket = false,
                _ => {}
            }
            continue;
        }
        match c {
            c if c == delim => return true,
            '\\' => {
                if *at >= chars.len() {
                    return false;
                }
                *at += 1;
            }
            '[' => {
                in_bracket = true;
                if chars.get(*at) == Some(&'^') {
                    *at += 1;
                }
                if chars.get(*at) == Some(&']') {
                    *at += 1;
                }
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::defs::{PolicyEffect, command_holds};

    fn head(line: &str) -> Option<&str> {
        pure_pipeline_head(line)
    }

    #[test]
    fn a_filtered_command_yields_the_command() {
        for (line, x) in [
            ("cargo test", "cargo test"),
            ("cargo test 2>&1", "cargo test"),
            ("cargo test 2>/dev/null", "cargo test"),
            ("cargo test 2>&1 | tail -40", "cargo test"),
            ("cargo update 2>&1 | tail -50", "cargo update"),
            (
                "cargo test --workspace 2>&1 | grep -n -A6 \"panicked\" | head -30",
                "cargo test --workspace",
            ),
            (
                "cargo test | grep -E 'a|b' | sort | uniq -c | wc -l",
                "cargo test",
            ),
            ("cargo test | sed -n '10,20p'", "cargo test"),
            ("cargo test | sed -n '/panicked/,+5p'", "cargo test"),
            ("cargo test | sed -n 's/a/b/gp'", "cargo test"),
            ("cargo test | cut -d: -f1 | tr -d '\\r' | cat", "cargo test"),
            ("cargo test | rg -n -C3 --max-count=4 error", "cargo test"),
            ("cargo test | tail -n +5", "cargo test"),
            ("cargo test | head -5", "cargo test"),
            ("cargo test | grep '^ok$'", "cargo test"),
        ] {
            assert_eq!(head(line), Some(x), "{line:?}");
        }
    }

    #[test]
    fn everything_with_a_side_effect_is_not_a_pure_pipeline() {
        for line in [
            "cargo test | sh",
            "cargo test | bash",
            "cargo test | xargs rm",
            "cargo test | tee x",
            "cargo test | sort -o f",
            "cargo test | sort --output=f",
            "cargo test | sort --compress-program=sh",
            "cargo test | sort f",
            "cargo test | uniq a b",
            "cargo test | uniq in out",
            "cargo test | cat f",
            "cargo test | cat -",
            "cargo test | awk '{print}'",
            "cargo test | grep -f pats",
            "cargo test | grep --file=pats x",
            "cargo test | grep x file",
            "cargo test | rg --pre sh x",
            "cargo test | rg -z x",
            "cargo test | tail -f",
            "cargo test | sed -n 'w out'",
            "cargo test | sed -n '1w out'",
            "cargo test | sed -n 's/a/b/w out'",
            "cargo test | sed -n 's/a/b/e'",
            "cargo test | sed -n '1e id'",
            "cargo test | sed -n 'p;w out'",
            "cargo test | sed -n '{p}'",
            "cargo test | sed -n 's/[/]w x/y/'",
            "cargo test | sed -i p",
            "cargo test | sed p",
            "cargo test | sed -n -f s",
            "cargo test | sed -n -e p",
            "cargo test | env sh",
            "cargo test | FOO=1 tail",
            "cargo test | /usr/bin/tail",
            "cargo test | 'tail'",
            "cargo test | tail > out",
            "cargo test | tail >out",
            "cargo test | tail 2>&1",
            "cargo test > out.txt",
            "cargo test > out.txt | tail",
            "cargo test >> out",
            "cargo test 2>out",
            "cargo test 2> /dev/null",
            "cargo test 2>&1 2>&1",
            "cargo test 2>&1 x | tail",
            "cargo test; rm -rf ~",
            "cargo test && rm x",
            "cargo test || rm x",
            "cargo test | tail || rm x",
            "cargo test | tail; rm x",
            "cargo test | tail && rm x",
            "cargo test | tail &",
            "cargo test |& tail",
            "cargo test | tail |",
            "cargo test || tail",
            "| tail",
            "cargo test | $(id)",
            "cargo test | tail $(id)",
            "cargo test | grep \"$(id)\"",
            "cargo test | grep \"$HOME\"",
            "cargo test | grep `id`",
            "cargo test | tail -n $N",
            "cargo test | tail <(id)",
            "cargo test | grep x <<< y",
            "cargo test | grep x < f",
            "cargo test | tail *",
            "cargo test | tail ~/x",
            "cargo test | tail\nrm x",
            "cargo test | tail \\| sh",
            "cargo test \\| sh",
            "cargo test 'unterminated | tail",
            "cargo test | grep 'unterminated",
            "cargo test | tail -n x",
            "cargo test | tail -- x",
            "cargo test | head -n",
            "cargo test | wc -l f",
            "cargo test | tr",
            "cargo test | tr a b c",
            "cargo test | cut -d",
            "cargo test | sort -T /tmp",
            "cargo test | rg --files",
            "cargo test | rg -r x y",
        ] {
            assert_eq!(head(line), None, "{line:?}");
        }
    }

    /// Filter stages that are on the list.
    fn safe_stage() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("tail -40".to_owned()),
            Just("head -n 5".to_owned()),
            Just("grep -n -A6 \"panicked\"".to_owned()),
            Just("rg -i error".to_owned()),
            Just("wc -l".to_owned()),
            Just("sort -u".to_owned()),
            Just("uniq -c".to_owned()),
            Just("cut -d: -f1".to_owned()),
            Just("tr -d 'x'".to_owned()),
            Just("cat".to_owned()),
            Just("sed -n '1,5p'".to_owned()),
        ]
    }

    /// Stages that write a file, run a program, read a named file, or are not on the list.
    fn unsafe_stage() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("sh".to_owned()),
            Just("bash -c x".to_owned()),
            Just("xargs rm".to_owned()),
            Just("tee f".to_owned()),
            Just("sort -o f".to_owned()),
            Just("sort --output f".to_owned()),
            Just("uniq in out".to_owned()),
            Just("awk '{print}'".to_owned()),
            Just("sed -n 'w f'".to_owned()),
            Just("sed -n 's/a/b/w f'".to_owned()),
            Just("sed -n 's/a/b/e'".to_owned()),
            Just("sed -i p f".to_owned()),
            Just("grep -f f".to_owned()),
            Just("rg --pre sh x".to_owned()),
            Just("cat f".to_owned()),
            Just("tail -f f".to_owned()),
            Just("dd of=f".to_owned()),
            Just("python -c x".to_owned()),
            Just("rm -rf x".to_owned()),
            Just("env sh".to_owned()),
            Just("tail > f".to_owned()),
            Just("tail $(id)".to_owned()),
            Just("tail `id`".to_owned()),
            Just("tail <(id)".to_owned()),
            Just("grep x <<< y".to_owned()),
            Just("tail; rm f".to_owned()),
            Just("tail && rm f".to_owned()),
            Just("tail || rm f".to_owned()),
            Just("tail &".to_owned()),
        ]
    }

    fn allow_glob_holds(line: &str) -> bool {
        command_holds(PolicyEffect::Allow, "cargo *", line)
    }

    proptest! {
        /// A pipeline of listed filters is matched as its first command.
        #[test]
        fn a_pipeline_of_listed_filters_is_allowed_as_its_command(
            stages in proptest::collection::vec(safe_stage(), 0..5),
            redirect in prop_oneof![Just(""), Just(" 2>&1"), Just(" 2>/dev/null")],
        ) {
            let mut line = format!("cargo test --all{redirect}");
            for stage in &stages {
                line.push_str(" | ");
                line.push_str(stage);
            }
            prop_assert!(allow_glob_holds(&line), "{}", line);
        }

        /// No line with a side-effecting stage anywhere in it becomes matchable.
        #[test]
        fn a_side_effecting_stage_anywhere_keeps_the_line_compound(
            before in proptest::collection::vec(safe_stage(), 0..3),
            bad in unsafe_stage(),
            after in proptest::collection::vec(safe_stage(), 0..3),
            redirect in prop_oneof![Just(""), Just(" 2>&1")],
        ) {
            let mut line = format!("cargo test{redirect}");
            for stage in before.iter().chain([&bad]).chain(after.iter()) {
                line.push_str(" | ");
                line.push_str(stage);
            }
            prop_assert!(!allow_glob_holds(&line), "{}", line);
        }

        /// Whatever the operator, redirection or substitution put in the line, it stays compound.
        #[test]
        fn an_operator_redirection_or_substitution_keeps_the_line_compound(
            op in prop_oneof![
                Just(";"), Just("&&"), Just("||"), Just("&"), Just("|&"), Just(">"), Just(">>"),
                Just("<"), Just("<<<"), Just("2>"), Just("&>"), Just(">&2"), Just("$("),
                Just("`"), Just("<("), Just(">("), Just("\n"),
            ],
            rest in "[a-z ./~-]{0,10}",
            place in 0..3usize,
            stage in safe_stage(),
        ) {
            let line = match place {
                0 => format!("cargo test {op}{rest} | {stage}"),
                1 => format!("cargo test | {stage} {op}{rest}"),
                _ => format!("cargo test | {stage} | tail {op}{rest}"),
            };
            prop_assert!(!allow_glob_holds(&line), "{}", line);
        }

        /// Whatever the line is, a command taken out of it is one simple command and every
        /// other stage starts with a listed filter.
        #[test]
        fn a_recognised_line_is_a_simple_command_and_listed_filters(
            line in proptest::collection::vec(
                prop_oneof![
                    Just("cargo"), Just("test"), Just("|"), Just("||"), Just(";"), Just("&"),
                    Just("2>&1"), Just(">"), Just("tail"), Just("sh"), Just("'"), Just("\""),
                    Just("-n"), Just("$("), Just(")"), Just("`"), Just("x"), Just("\\"), Just("\n"),
                ],
                0..14,
            ).prop_map(|words| words.join(" "))
        ) {
            if let Some(command) = pure_pipeline_head(&line) {
                prop_assert!(!crate::defs::eval::chains_or_redirects(command));
                let mut quote = None;
                let mut stages: Vec<String> = vec![String::new()];
                for c in line.chars() {
                    match (quote, c) {
                        (None, '\'' | '"') => quote = Some(c),
                        (Some(q), c) if c == q => quote = None,
                        (None, '|') => stages.push(String::new()),
                        _ => {}
                    }
                    if !(quote.is_none() && c == '|') {
                        stages.last_mut().unwrap().push(c);
                    }
                }
                for stage in &stages[1..] {
                    let name = stage.split_whitespace().next().unwrap_or("");
                    prop_assert!(FILTERS.contains(&name), "{line:?}");
                    prop_assert!(!stage.contains(['`', '\n', ';', '<', '>']) || stage.contains(['\'', '"']), "{line:?}");
                }
            }
        }
    }

    #[test]
    fn an_env_prefix_is_never_stripped() {
        assert_eq!(
            head("RUSTC=sh cargo test | tail"),
            Some("RUSTC=sh cargo test")
        );
        assert_eq!(head("FOO=1 cargo test"), Some("FOO=1 cargo test"));
    }
}
