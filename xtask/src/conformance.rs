//! The pure half of `cargo xtask conformance-live`: the report the live suite writes, the
//! comparison with the previous night's, the version-bump hints and the Markdown. Nothing here
//! touches the machine, so all of it is unit-tested; [`crate::live`] is the I/O shell.
//!
//! # What counts as a regression (the exit code)
//!
//! Against the previous dated report, and always for the report itself:
//!
//! - a case that **fails**, new or not;
//! - a case that is **newly waived**: it passed (or was skipped) before and is waived now, so a
//!   claim lost its proof;
//! - a case that **no longer runs**: it passed before and is skipped now;
//! - a **new unrecognised-frame kind**: a kind the previous report did not have (with no previous
//!   report there is no baseline, so the counters are only noted);
//! - a CLI that was **usable before and is not now** (not installed, signed out).
//!
//! Everything else is a note: a CLI version change, a count that rose for a known kind, a
//! waiver that was already there, a CLI that was not usable before either.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// One night's report: what `tests/conformance_live.rs` writes, plus the date.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Report {
    #[serde(default)]
    pub(crate) schema: u32,
    /// `YYYY-MM-DD` (UTC), set by the job.
    #[serde(default)]
    pub(crate) date: String,
    #[serde(default)]
    pub(crate) generated_at_ms: u64,
    #[serde(default)]
    pub(crate) targets: Vec<TargetRecord>,
}

/// One CLI's results, or the reason it could not be run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TargetRecord {
    pub(crate) target: String,
    /// Why the CLI could not be run (not installed, not signed in).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) unavailable: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) cli_version: Option<String>,
    #[serde(default)]
    pub(crate) cli_min: Option<String>,
    /// `cli_verified.verified` of the driver's descriptor.
    #[serde(default)]
    pub(crate) cli_verified: Option<String>,
    #[serde(default)]
    pub(crate) version_gate: Option<String>,
    #[serde(default)]
    pub(crate) estimated_runs: Option<u64>,
    #[serde(default)]
    pub(crate) cases: Vec<CaseRecord>,
    #[serde(default)]
    pub(crate) unknown_frames: BTreeMap<String, u64>,
}

/// One case's result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CaseRecord {
    pub(crate) case: String,
    /// `pass`, `skip`, `waived` or `fail`.
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) reason: Option<String>,
    #[serde(default)]
    pub(crate) took_ms: u64,
}

impl TargetRecord {
    fn case(&self, name: &str) -> Option<&CaseRecord> {
        self.cases.iter().find(|c| c.case == name)
    }

    fn has_failures(&self) -> bool {
        self.cases.iter().any(|c| c.status == "fail")
    }
}

/// The one-line change that raises a driver's `cli_verified.verified`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bump {
    pub(crate) target: String,
    pub(crate) file: &'static str,
    pub(crate) from: String,
    pub(crate) to: String,
}

impl Bump {
    /// The change, as the line it replaces and the line that replaces it.
    pub(crate) fn line(&self) -> String {
        format!(
            "{}: pub const VERIFIED_CLI_VERSION: &str = \"{}\";  ->  \"{}\";",
            self.file, self.from, self.to
        )
    }
}

/// What the comparison found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Comparison {
    /// Each makes the job exit non-zero.
    pub(crate) regressions: Vec<String>,
    /// Worth reading, not a failure.
    pub(crate) notes: Vec<String>,
    /// Drivers whose verified version can move up.
    pub(crate) bumps: Vec<Bump>,
}

/// The driver source that holds `VERIFIED_CLI_VERSION` for a live target.
fn driver_file(target: &str) -> Option<&'static str> {
    match target {
        "claude-code" => Some("crates/reno-harness/src/drivers/claude/mod.rs"),
        "codex" => Some("crates/reno-harness/src/drivers/codex/mod.rs"),
        _ => None,
    }
}

/// `1.2.3`, `v1.2.3` or `1.2.3-beta` as numbers; what follows the patch number is ignored.
pub(crate) fn version_triplet(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches('v');
    let mut parts = text.split('.');
    let number = |part: Option<&str>| -> Option<u64> {
        let digits: String = part?.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    };
    let major = number(parts.next())?;
    let minor = number(parts.next())?;
    let patch = number(parts.next())?;
    Some((major, minor, patch))
}

/// Compares `now` with `prev` (the latest earlier report, when there is one).
pub(crate) fn compare(prev: Option<&Report>, now: &Report) -> Comparison {
    let mut out = Comparison::default();
    for target in &now.targets {
        let name = &target.target;
        let before = prev.and_then(|p| p.targets.iter().find(|t| &t.target == name));
        if let Some(why) = &target.unavailable {
            match before {
                Some(b) if b.unavailable.is_none() => out.regressions.push(format!(
                    "{name}: usable in the previous run, not now ({why})"
                )),
                _ => out.notes.push(format!("{name}: not run ({why})")),
            }
            continue;
        }
        compare_cases(&mut out, target, before);
        compare_frames(&mut out, target, before, prev.is_some());
        if let Some(b) = before
            && b.unavailable.is_none()
            && b.cli_version != target.cli_version
        {
            out.notes.push(format!(
                "{name}: CLI version {} -> {}",
                b.cli_version.as_deref().unwrap_or("unknown"),
                target.cli_version.as_deref().unwrap_or("unknown")
            ));
        }
        if target.version_gate.as_deref() == Some("too_old") {
            out.notes.push(format!(
                "{name}: the installed CLI {} is older than the supported minimum {}",
                target.cli_version.as_deref().unwrap_or("unknown"),
                target.cli_min.as_deref().unwrap_or("unknown")
            ));
        }
        if let Some(bump) = bump_for(target) {
            out.bumps.push(bump);
        }
    }
    out
}

fn compare_cases(out: &mut Comparison, target: &TargetRecord, before: Option<&TargetRecord>) {
    let name = &target.target;
    let before = before.filter(|b| b.unavailable.is_none());
    for case in &target.cases {
        let was = before.and_then(|b| b.case(&case.case));
        let reason = case.reason.as_deref().unwrap_or("no reason given");
        match (case.status.as_str(), was.map(|w| w.status.as_str())) {
            ("fail", Some("fail")) => out
                .regressions
                .push(format!("{name}/{}: still failing: {reason}", case.case)),
            ("fail", _) => out
                .regressions
                .push(format!("{name}/{}: fails: {reason}", case.case)),
            ("waived", Some(old)) if old != "waived" => out.regressions.push(format!(
                "{name}/{}: newly waived (was {old}): {reason}",
                case.case
            )),
            ("skip", Some("pass")) => out.regressions.push(format!(
                "{name}/{}: no longer runs (was pass): {reason}",
                case.case
            )),
            ("waived", None) if before.is_some() => out.notes.push(format!(
                "{name}/{}: a case the previous run did not have is waived: {reason}",
                case.case
            )),
            _ => {}
        }
    }
}

fn compare_frames(
    out: &mut Comparison,
    target: &TargetRecord,
    before: Option<&TargetRecord>,
    have_baseline: bool,
) {
    let name = &target.target;
    let before = before.filter(|b| b.unavailable.is_none());
    for (kind, count) in &target.unknown_frames {
        match before.map(|b| b.unknown_frames.get(kind)) {
            Some(Some(old)) if count > old => out.notes.push(format!(
                "{name}: unrecognised frame {kind} rose from {old} to {count}"
            )),
            Some(Some(_)) => {}
            Some(None) => out.regressions.push(format!(
                "{name}: new unrecognised frame kind {kind} ({count}): the CLI moved, see docs/KEEPING-UP.md"
            )),
            None if have_baseline => out.regressions.push(format!(
                "{name}: new unrecognised frame kind {kind} ({count}): the CLI moved, see docs/KEEPING-UP.md"
            )),
            None => out.notes.push(format!(
                "{name}: unrecognised frame {kind} ({count}); no earlier report to compare with"
            )),
        }
    }
    if let Some(b) = before {
        for kind in b.unknown_frames.keys() {
            if !target.unknown_frames.contains_key(kind) {
                out.notes
                    .push(format!("{name}: unrecognised frame {kind} did not occur"));
            }
        }
    }
}

/// The bump a green run on a newer CLI calls for, if any.
fn bump_for(target: &TargetRecord) -> Option<Bump> {
    let file = driver_file(&target.target)?;
    let version = target.cli_version.as_deref()?;
    let verified = target.cli_verified.as_deref()?;
    if target.has_failures() || target.cases.is_empty() {
        return None;
    }
    (version_triplet(version)? > version_triplet(verified)?).then(|| Bump {
        target: target.target.clone(),
        file,
        from: verified.to_owned(),
        to: version.trim().trim_start_matches('v').to_owned(),
    })
}

/// The Markdown report.
pub(crate) fn markdown(now: &Report, prev_date: Option<&str>, cmp: &Comparison) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Live conformance, {}\n", now.date);
    let _ = writeln!(
        out,
        "Compared with: {}.\n",
        prev_date.unwrap_or("no earlier report")
    );
    if cmp.regressions.is_empty() {
        out.push_str("**No regressions.**\n\n");
    } else {
        let _ = writeln!(out, "**{} regression(s):**\n", cmp.regressions.len());
        for line in &cmp.regressions {
            let _ = writeln!(out, "- {line}");
        }
        out.push('\n');
    }
    if !cmp.notes.is_empty() {
        out.push_str("Notes:\n\n");
        for line in &cmp.notes {
            let _ = writeln!(out, "- {line}");
        }
        out.push('\n');
    }
    if !cmp.bumps.is_empty() {
        out.push_str(
            "A green run on a newer CLI than the one the driver was verified on. If the schema \
             check and the rest of the checklist (docs/KEEPING-UP.md) are done, make this change \
             by hand:\n\n",
        );
        for bump in &cmp.bumps {
            let _ = writeln!(out, "    {}", bump.line());
        }
        out.push('\n');
    }
    for target in &now.targets {
        let _ = writeln!(out, "## {}\n", target.target);
        if let Some(why) = &target.unavailable {
            let _ = writeln!(out, "Not run: {why}\n");
            continue;
        }
        let _ = writeln!(
            out,
            "CLI {} (verified up to {}, gate {}), model {}, about {} model runs.\n",
            target.cli_version.as_deref().unwrap_or("unknown"),
            target.cli_verified.as_deref().unwrap_or("unknown"),
            target.version_gate.as_deref().unwrap_or("unknown"),
            target.model.as_deref().unwrap_or("unknown"),
            target.estimated_runs.unwrap_or(0)
        );
        out.push_str("| case | status | took |\n|---|---|---|\n");
        for case in &target.cases {
            let _ = writeln!(
                out,
                "| {} | {} | {:.1} s |",
                case.case,
                case.status,
                case.took_ms as f64 / 1000.0
            );
        }
        out.push('\n');
        let not_pass: Vec<&CaseRecord> = target
            .cases
            .iter()
            .filter(|c| c.status != "pass" && c.reason.is_some())
            .collect();
        if !not_pass.is_empty() {
            out.push_str("Not a pass:\n\n");
            for case in not_pass {
                let _ = writeln!(
                    out,
                    "- {} ({}): {}",
                    case.case,
                    case.status,
                    case.reason.as_deref().unwrap_or_default()
                );
            }
            out.push('\n');
        }
        if target.unknown_frames.is_empty() {
            out.push_str("Unrecognised frames: none.\n\n");
        } else {
            out.push_str("Unrecognised frames:\n\n");
            for (kind, count) in &target.unknown_frames {
                let _ = writeln!(out, "- {kind}: {count}");
            }
            out.push('\n');
        }
    }
    out
}

/// The UTC date of `ms` since the Unix epoch, as `YYYY-MM-DD`.
pub(crate) fn civil_date(ms: u64) -> String {
    let days = i64::try_from(ms / 86_400_000).unwrap_or(0);
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

/// Whether `name` is the file name of a dated report (`YYYY-MM-DD.json`), and its date.
pub(crate) fn report_date(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".json")?;
    let bytes = stem.as_bytes();
    let shaped = bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        });
    shaped.then_some(stem)
}

/// The latest date among `names` that is strictly before `today`.
pub(crate) fn previous_date<'a>(
    names: impl IntoIterator<Item = &'a str>,
    today: &str,
) -> Option<String> {
    names
        .into_iter()
        .filter_map(report_date)
        .filter(|date| *date < today)
        .max()
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(name: &str, status: &str, reason: Option<&str>) -> CaseRecord {
        CaseRecord {
            case: name.into(),
            status: status.into(),
            reason: reason.map(str::to_owned),
            took_ms: 1_500,
        }
    }

    fn target(name: &str, version: &str, cases: Vec<CaseRecord>) -> TargetRecord {
        TargetRecord {
            target: name.into(),
            model: Some("m".into()),
            cli_version: Some(version.into()),
            cli_min: Some("1.0.0".into()),
            cli_verified: Some("1.2.3".into()),
            version_gate: Some("ok".into()),
            estimated_runs: Some(12),
            cases,
            ..TargetRecord::default()
        }
    }

    fn report(date: &str, targets: Vec<TargetRecord>) -> Report {
        Report {
            schema: 1,
            date: date.into(),
            generated_at_ms: 0,
            targets,
        }
    }

    fn green() -> Vec<CaseRecord> {
        vec![
            case("probe", "pass", None),
            case("steer", "pass", None),
            case("crash", "waived", Some("not driven")),
        ]
    }

    #[test]
    fn an_identical_green_run_is_no_regression() {
        let before = report("2026-11-16", vec![target("codex", "1.2.3", green())]);
        let now = report("2026-11-17", vec![target("codex", "1.2.3", green())]);
        assert_eq!(compare(Some(&before), &now), Comparison::default());
    }

    #[test]
    fn failures_new_and_old_are_regressions() {
        let mut broken = green();
        broken[1] = case("steer", "fail", Some("no text"));
        let before = report("a", vec![target("codex", "1.2.3", green())]);
        let now = report("b", vec![target("codex", "1.2.3", broken.clone())]);
        let cmp = compare(Some(&before), &now);
        assert_eq!(cmp.regressions, ["codex/steer: fails: no text"]);
        let again = compare(
            Some(&now),
            &report("c", vec![target("codex", "1.2.3", broken)]),
        );
        assert_eq!(again.regressions, ["codex/steer: still failing: no text"]);
        // And a failure with nothing to compare with still fails the job.
        let first = compare(None, &now);
        assert_eq!(first.regressions.len(), 1);
    }

    #[test]
    fn a_new_waiver_or_a_vanished_case_is_a_regression_but_an_old_waiver_is_not() {
        let before = report("a", vec![target("codex", "1.2.3", green())]);
        let mut worse = green();
        worse[0] = case("probe", "waived", Some("now waived"));
        worse[1] = case("steer", "skip", Some("claims no steering"));
        let now = report("b", vec![target("codex", "1.2.3", worse)]);
        let cmp = compare(Some(&before), &now);
        assert_eq!(
            cmp.regressions,
            [
                "codex/probe: newly waived (was pass): now waived",
                "codex/steer: no longer runs (was pass): claims no steering",
            ]
        );
        assert!(
            cmp.notes.is_empty(),
            "the crash waiver was already there: {:?}",
            cmp.notes
        );
    }

    #[test]
    fn unknown_frames_by_name_new_kinds_regress_and_rising_counts_are_notes() {
        let mut was = target("claude-code", "1.2.3", green());
        was.unknown_frames.insert("type:a".into(), 2);
        let mut is = target("claude-code", "1.2.3", green());
        is.unknown_frames.insert("type:a".into(), 5);
        is.unknown_frames.insert("type:b".into(), 1);
        let cmp = compare(
            Some(&report("a", vec![was])),
            &report("b", vec![is.clone()]),
        );
        assert_eq!(cmp.regressions.len(), 1);
        assert!(
            cmp.regressions[0].contains("type:b"),
            "{:?}",
            cmp.regressions
        );
        assert_eq!(
            cmp.notes,
            ["claude-code: unrecognised frame type:a rose from 2 to 5"]
        );
        // Without an earlier report there is no baseline: noted, not failed.
        let first = compare(None, &report("b", vec![is]));
        assert!(first.regressions.is_empty(), "{:?}", first.regressions);
        assert_eq!(first.notes.len(), 2);
    }

    #[test]
    fn a_cli_that_was_usable_and_is_not_regresses_and_one_that_never_was_is_a_note() {
        let before = report("a", vec![target("codex", "1.2.3", green())]);
        let gone = TargetRecord {
            target: "codex".into(),
            unavailable: Some("not signed in: run codex login".into()),
            ..TargetRecord::default()
        };
        let now = report("b", vec![gone.clone()]);
        let cmp = compare(Some(&before), &now);
        assert_eq!(
            cmp.regressions,
            ["codex: usable in the previous run, not now (not signed in: run codex login)"]
        );
        let still = compare(Some(&now), &report("c", vec![gone]));
        assert!(still.regressions.is_empty());
        assert_eq!(still.notes.len(), 1);
    }

    #[test]
    fn a_version_change_is_noted_and_a_green_newer_cli_gets_the_exact_bump() {
        let before = report("a", vec![target("codex", "1.2.3", green())]);
        let now = report("b", vec![target("codex", "1.3.0", green())]);
        let cmp = compare(Some(&before), &now);
        assert!(cmp.regressions.is_empty());
        assert_eq!(cmp.notes, ["codex: CLI version 1.2.3 -> 1.3.0"]);
        assert_eq!(
            cmp.bumps[0].line(),
            "crates/reno-harness/src/drivers/codex/mod.rs: pub const VERIFIED_CLI_VERSION: &str = \"1.2.3\";  ->  \"1.3.0\";"
        );
        // Not on the verified version, not on a CLI that failed, not on an unknown driver.
        assert!(
            compare(None, &report("b", vec![target("codex", "1.2.3", green())]))
                .bumps
                .is_empty()
        );
        let mut failing = green();
        failing[0] = case("probe", "fail", Some("x"));
        assert!(
            compare(None, &report("b", vec![target("codex", "1.3.0", failing)]))
                .bumps
                .is_empty()
        );
        assert!(
            compare(None, &report("b", vec![target("grok", "1.3.0", green())]))
                .bumps
                .is_empty()
        );
        // A v prefix is not carried into the constant.
        let prefixed = compare(
            None,
            &report("b", vec![target("claude-code", "v1.2.4", green())]),
        );
        assert_eq!(prefixed.bumps[0].to, "1.2.4");
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert_eq!(version_triplet("2.1.325"), Some((2, 1, 325)));
        assert_eq!(version_triplet("v0.170.0-beta.1"), Some((0, 170, 0)));
        assert_eq!(version_triplet("2.1"), None);
        assert!(version_triplet("0.170.0") > version_triplet("0.169.9"));
        assert!(version_triplet("1.10.0") > version_triplet("1.9.0"));
    }

    #[test]
    fn dates_come_from_the_epoch_and_the_previous_report_is_the_latest_earlier_one() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(1_700_000_000_000), "2023-11-14");
        assert_eq!(civil_date(1_709_164_800_000), "2024-02-29");
        assert_eq!(civil_date(1_804_000_000_000), "2027-03-02");
        assert_eq!(report_date("2026-11-16.json"), Some("2026-11-16"));
        assert_eq!(report_date("2026-11-16.md"), None);
        assert_eq!(report_date("latest.json"), None);
        let names = [
            "2026-11-14.json",
            "2026-11-16.json",
            "2026-11-17.json",
            "2026-11-16.md",
            "x.json",
        ];
        assert_eq!(
            previous_date(names, "2026-11-17").as_deref(),
            Some("2026-11-16")
        );
        assert_eq!(previous_date(names, "2026-11-14"), None);
    }

    #[test]
    fn the_markdown_leads_with_the_verdict_and_ends_with_the_bump() {
        let before = report("2026-11-16", vec![target("codex", "1.2.3", green())]);
        let mut worse = green();
        worse[1] = case("steer", "fail", Some("no text"));
        let now = report("2026-11-17", vec![target("codex", "1.3.0", worse)]);
        let text = markdown(&now, Some("2026-11-16"), &compare(Some(&before), &now));
        assert!(text.starts_with("# Live conformance, 2026-11-17"), "{text}");
        assert!(text.contains("**1 regression(s):**"), "{text}");
        assert!(text.contains("- codex/steer: fails: no text"), "{text}");
        assert!(text.contains("| steer | fail | 1.5 s |"), "{text}");
        assert!(
            !text.contains("VERIFIED_CLI_VERSION"),
            "a failing run offers no bump: {text}"
        );

        let ok = report("2026-11-17", vec![target("codex", "1.3.0", green())]);
        let text = markdown(&ok, None, &compare(None, &ok));
        assert!(text.contains("**No regressions.**"), "{text}");
        assert!(text.contains("no earlier report"), "{text}");
        assert!(text.contains("->  \"1.3.0\";"), "{text}");
        assert!(text.contains("Unrecognised frames: none."), "{text}");
    }
}
