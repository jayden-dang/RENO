//! The report: case by executor, pass, skip, waived or fail.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use super::CaseId;

/// How one case ended on one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The case ran and everything held.
    Pass,
    /// The case does not apply: the descriptor claims nothing it would prove. Not a pass.
    Skip(String),
    /// The descriptor claims it, and this target cannot exercise it (a live run of something a
    /// tiny prompt cannot provoke). The claim is unverified. Not a pass.
    Waived(String),
    /// The case ran and something did not hold, or a claim has nothing exercising it.
    Fail(String),
}

impl Status {
    /// The one-word label the table prints.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Skip(_) => "skip",
            Self::Waived(_) => "waived",
            Self::Fail(_) => "FAIL",
        }
    }

    /// The reason, for anything that is not a pass.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Pass => None,
            Self::Skip(why) | Self::Waived(why) | Self::Fail(why) => Some(why),
        }
    }
}

/// One case's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseResult {
    /// The case.
    pub case: CaseId,
    /// How it ended.
    pub status: Status,
    /// How long it took.
    pub took: Duration,
}

/// One target's results, in suite order.
#[derive(Debug, Clone)]
pub struct TargetReport {
    /// The target's name.
    pub target: String,
    /// One result per case.
    pub results: Vec<CaseResult>,
    /// The unrecognised-frame counters at the end of the suite, by kind.
    pub unknown_frames: BTreeMap<String, u64>,
}

impl TargetReport {
    /// How `case` ended.
    pub fn status(&self, case: CaseId) -> Option<&Status> {
        self.results
            .iter()
            .find(|r| r.case == case)
            .map(|r| &r.status)
    }

    /// Every failed case with its reason.
    pub fn failures(&self) -> Vec<(CaseId, &str)> {
        self.results
            .iter()
            .filter_map(|r| match &r.status {
                Status::Fail(why) => Some((r.case, why.as_str())),
                _ => None,
            })
            .collect()
    }

    /// Whether no case failed.
    pub fn is_green(&self) -> bool {
        self.failures().is_empty()
    }

    /// The labels, in suite order, as `case=label`: what a test pins.
    pub fn labels(&self) -> Vec<String> {
        self.results
            .iter()
            .map(|r| format!("{}={}", r.case.name(), r.status.label()))
            .collect()
    }
}

/// The report table: a row per case, a column per target, then every reason that is not a pass
/// and the unrecognised-frame counters.
pub fn render_table(reports: &[TargetReport]) -> String {
    let width = CaseId::ALL
        .iter()
        .map(|c| c.name().len())
        .max()
        .unwrap_or(0)
        .max("case".len());
    let columns: Vec<usize> = reports
        .iter()
        .map(|r| r.target.len().max("waived".len()))
        .collect();
    let mut out = String::new();
    let _ = write!(out, "{:<width$}", "case");
    for (report, w) in reports.iter().zip(&columns) {
        let _ = write!(out, "  {:<w$}", report.target);
    }
    out.push('\n');
    for case in CaseId::ALL {
        let _ = write!(out, "{:<width$}", case.name());
        for (report, w) in reports.iter().zip(&columns) {
            let label = report.status(case).map_or("-", Status::label);
            let _ = write!(out, "  {label:<w$}");
        }
        out.push('\n');
    }
    let mut notes = String::new();
    for report in reports {
        for result in &report.results {
            if let Some(why) = result.status.reason() {
                let _ = writeln!(
                    notes,
                    "  {} / {}: {} - {}",
                    report.target,
                    result.case.name(),
                    result.status.label(),
                    why
                );
            }
        }
    }
    if !notes.is_empty() {
        out.push_str("\nnot a pass:\n");
        out.push_str(&notes);
    }
    let mut counters = String::new();
    for report in reports {
        for (kind, count) in &report.unknown_frames {
            let _ = writeln!(counters, "  {} / {kind}: {count}", report.target);
        }
    }
    if !counters.is_empty() {
        out.push_str("\nunrecognised frames:\n");
        out.push_str(&counters);
    }
    out
}
