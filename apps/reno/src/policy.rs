//! `reno policy check`: evaluates one action against `policy.toml` and says what would happen.
//!
//! It reads the file itself and runs the same pure evaluation the daemon runs
//! (`reno_proto::defs::evaluate`), so it needs no daemon: use it before saving an edit, or to find
//! out why a rule did or did not fire. It judges the policy on disk, which is what the daemon
//! loads next; the daemon keeps running on its last good policy while the file does not validate.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, bail};
use reno_proto::ApprovalSubject;
use reno_proto::defs::{
    DefFile, DefKind, EvalContext, POLICY_NAME, PolicyDecision, PolicyDef, ValidationContext,
    evaluate, load_set,
};
use serde::Serialize;

use crate::cli::{PolicyCheck, PolicyCommand};
use crate::defs::read_one;
use crate::env::{Env, print_json, print_line};

pub(crate) fn run(env: &Env, command: PolicyCommand) -> anyhow::Result<ExitCode> {
    match command {
        PolicyCommand::Check(args) => check(env, args),
    }
}

/// What `--json` prints: the decision, and where the policy came from.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Checked<'a> {
    /// The policy file read, or `None` when there is none and the built-in default applied.
    policy: Option<&'a Path>,
    #[serde(flatten)]
    decision: &'a PolicyDecision,
}

fn check(env: &Env, args: PolicyCheck) -> anyhow::Result<ExitCode> {
    let subject: ApprovalSubject = serde_json::from_str(&args.subject).context(
        "the subject is not a valid approval subject; write for example \
         '{\"action\":\"command\",\"tool\":\"bash\",\"command\":\"cargo test\"}'",
    )?;
    let dir = match args.dir.or_else(reno_client::default_defs_dir) {
        Some(dir) => dir,
        None => bail!("cannot find the definitions directory: set --dir, RENO_DEFS_DIR, or HOME"),
    };
    let cwd = match args.cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir().context("cannot read the current directory")?,
    };
    let cwd = std::path::absolute(&cwd)
        .with_context(|| format!("cannot make {} absolute", cwd.display()))?;

    let (policy, source) = load_policy(&dir)?;
    let decision = evaluate(
        &policy,
        &EvalContext {
            bot: args.bot.as_deref(),
            cwd: &cwd,
        },
        &subject,
    );
    if env.json {
        print_json(&Checked {
            policy: source.as_deref(),
            decision: &decision,
        })?;
    } else {
        if source.is_none() {
            print_line(format_args!(
                "no policy.toml in {}: the built-in default applies",
                dir.display()
            ))?;
        }
        print_line(describe(&decision))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// The policy in `dir` and the file it came from, or the built-in default (ask about everything)
/// when `dir` has none, which is what the daemon uses too. A file that does not validate is an
/// error that lists every problem.
fn load_policy(dir: &Path) -> anyhow::Result<(PolicyDef, Option<PathBuf>)> {
    let path = dir.join(DefKind::Policy.path_for(POLICY_NAME));
    if !std::fs::metadata(&path).is_ok_and(|meta| meta.is_file()) {
        return Ok((PolicyDef::default(), None));
    }
    let mut files: Vec<DefFile> = Vec::new();
    let mut problems = Vec::new();
    read_one(
        &path,
        DefKind::Policy,
        POLICY_NAME,
        &mut files,
        &mut problems,
    );
    let loaded = if problems.is_empty() {
        load_set(&files, &ValidationContext::with_builtin([]))
    } else {
        Err(problems)
    };
    match loaded {
        Ok(set) => {
            let policy = set
                .policy()
                .map(|validated| validated.def().clone())
                .context("policy.toml was read but holds no policy")?;
            Ok((policy, Some(path)))
        }
        Err(errors) => {
            let lines: Vec<String> = errors.iter().map(ToString::to_string).collect();
            bail!("the policy does not validate:\n  {}", lines.join("\n  "))
        }
    }
}

/// `allow (rule: cargo is fine)`, `deny (rule: force push): denied by policy: force push`,
/// `ask (default)`.
fn describe(decision: &PolicyDecision) -> String {
    let (verb, rule) = match decision {
        PolicyDecision::Allow { rule } => ("allow", rule),
        PolicyDecision::Deny { rule, .. } => ("deny", rule),
        PolicyDecision::Ask { rule } => ("ask", rule),
    };
    let by = match rule {
        Some(rule) => format!("rule: {}", rule.name),
        None => "default".to_owned(),
    };
    match decision {
        PolicyDecision::Deny { message, .. } => format!("{verb} ({by}): {message}"),
        _ => format!("{verb} ({by})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(policy: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        if let Some(text) = policy {
            std::fs::write(dir.path().join("policy.toml"), text).expect("write");
        }
        dir
    }

    fn decide(dir: &Path, subject: &str, bot: Option<&str>) -> PolicyDecision {
        let (policy, _) = load_policy(dir).expect("policy");
        let subject: ApprovalSubject = serde_json::from_str(subject).expect("subject");
        evaluate(
            &policy,
            &EvalContext {
                bot,
                cwd: Path::new("/work/tree"),
            },
            &subject,
        )
    }

    const POLICY: &str = r#"
        default = "ask"
        [[rules]]
        note = "cargo is fine"
        effect = "allow"
        match = { action = "command", command = "cargo *", scope = ["keeper"] }
        [[rules]]
        note = "outside the worktree"
        effect = "deny"
        match = { action = "edit", path = "/**" }
    "#;

    #[test]
    fn it_prints_the_decision_and_the_rule() {
        let dir = dir_with(Some(POLICY));
        let cargo = r#"{"action":"command","tool":"bash","command":"cargo test"}"#;
        assert_eq!(
            describe(&decide(dir.path(), cargo, Some("keeper"))),
            "allow (rule: cargo is fine)"
        );
        // Scope matters: the same action by another bot falls to the default.
        assert_eq!(
            describe(&decide(dir.path(), cargo, Some("scout"))),
            "ask (default)"
        );
        let outside = r#"{"action":"edit","tool":"write","paths":["../x"]}"#;
        assert_eq!(
            describe(&decide(dir.path(), outside, None)),
            "deny (rule: outside the worktree): denied by policy: outside the worktree"
        );
    }

    #[test]
    fn without_a_policy_file_the_default_applies_and_the_source_is_none() {
        let dir = dir_with(None);
        let (policy, source) = load_policy(dir.path()).expect("default");
        assert_eq!(policy, PolicyDef::default());
        assert_eq!(source, None);
    }

    #[test]
    fn a_policy_that_does_not_validate_is_an_error_with_every_problem() {
        let dir = dir_with(Some(
            "approval_ttl = \"soon\"\n[[rules]]\neffect = \"allow\"\nmatch = {}\n",
        ));
        let error = load_policy(dir.path()).expect_err("invalid");
        let text = format!("{error:#}");
        assert!(text.contains("approval_ttl"), "{text}");
        assert!(text.contains("would switch policy off"), "{text}");
    }

    #[test]
    fn a_subject_may_leave_out_everything_but_action_and_tool() {
        let subject: ApprovalSubject =
            serde_json::from_str(r#"{"action":"network","tool":"fetch"}"#).expect("subject");
        assert!(subject.command.is_none() && subject.paths.is_empty());
        assert!(serde_json::from_str::<ApprovalSubject>(r#"{"tool":"x"}"#).is_err());
    }

    #[test]
    fn the_shipped_default_policy_is_checkable() {
        let dir = dir_with(Some(reno_proto::defs::defaults::POLICY));
        let force = r#"{"action":"command","tool":"bash","command":"git push --force"}"#;
        assert!(matches!(
            decide(dir.path(), force, None),
            PolicyDecision::Deny { .. }
        ));
    }
}
