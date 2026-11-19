//! Settings › Accounts (brief: docs/design/2026-11-19-settings-pages-ui-brief.md): each harness's
//! account and its tightest usage meter, and the finding as the page's title. Show only:
//! accounts are switched in the harness, and Executors signs them in.

use reno_proto::{ExecutorInfo, SignedIn};

/// One meter of a plan.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Meter {
    /// "Session", "Week".
    pub(crate) label: String,
    /// 0.0 to 1.0.
    pub(crate) used: f32,
    /// "resets in 2h".
    pub(crate) resets: Option<String>,
}

impl Meter {
    /// From 80% the bar is a warning.
    pub(crate) fn near_limit(&self) -> bool {
        self.used >= 0.8
    }

    /// "Session 54% · resets in 2h".
    pub(crate) fn words(&self) -> String {
        let pct = (self.used * 100.0).round();
        match &self.resets {
            Some(r) => format!("{} {pct}% \u{b7} {r}", self.label),
            None => format!("{} {pct}%", self.label),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Row {
    pub(crate) executor: String,
    pub(crate) name: String,
    /// The email, "Signed in", or "Signed out".
    pub(crate) who: String,
    pub(crate) signed_out: bool,
    pub(crate) plan: Option<String>,
    /// The tightest meter; none when signed out or not reported.
    pub(crate) meter: Option<Meter>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct View {
    /// The finding: "Claude Code is 54% into its session".
    pub(crate) title: String,
    pub(crate) detail: String,
    /// The tightest meter is near its limit.
    pub(crate) warn: bool,
    pub(crate) rows: Vec<Row>,
}

/// The page for `executors` at `now_ms`. An executor is listed when it has an account to speak
/// of: signed in with an email or meters, or signed out. Accounts with meters come first, then
/// the rest signed in, then the signed out; a signed-out account shows no meter.
pub(crate) fn view(executors: &[ExecutorInfo], now_ms: u64) -> View {
    let mut rows: Vec<Row> = executors.iter().filter_map(|e| row(e, now_ms)).collect();
    rows.sort_by_key(|r| (r.signed_out, r.meter.is_none()));

    let tightest = rows
        .iter()
        .filter_map(|r| r.meter.as_ref().map(|m| (r, m)))
        .max_by(|a, b| a.1.used.total_cmp(&b.1.used));
    let (title, mut detail, warn) = match tightest {
        Some((r, m)) => (
            format!(
                "{} is {}% into its {}",
                r.name,
                (m.used * 100.0).round(),
                m.label.to_lowercase()
            ),
            m.resets
                .as_ref()
                .map_or_else(String::new, |when| format!("It {when}. ")),
            m.near_limit(),
        ),
        None => ("No usage to show".to_owned(), String::new(), false),
    };
    let out: Vec<&str> = rows
        .iter()
        .filter(|r| r.signed_out)
        .map(|r| r.name.as_str())
        .collect();
    detail.push_str(&match out.as_slice() {
        [] => String::new(),
        [one] => format!("{one} is signed out; sign in from Executors."),
        [a, b] => format!("{a} and {b} are signed out; sign in from Executors."),
        more => format!(
            "{} accounts are signed out; sign in from Executors.",
            more.len()
        ),
    });
    View {
        title,
        detail: detail.trim_end().to_owned(),
        warn,
        rows,
    }
}

fn row(e: &ExecutorInfo, now_ms: u64) -> Option<Row> {
    let health = e.health.as_ref()?;
    let executor = e.descriptor.id.to_string();
    let (who, signed_out) = match &health.signed_in {
        SignedIn::Yes { account } => (account.clone(), false),
        SignedIn::No { .. } => (None, true),
        SignedIn::Unknown => (None, false),
    };
    let usage = health.plan_usage.as_ref();
    let meter = usage.filter(|_| !signed_out).and_then(|u| {
        u.windows
            .iter()
            .max_by(|a, b| a.used_fraction().total_cmp(&b.used_fraction()))
            .map(|w| Meter {
                label: w.label.clone(),
                used: w.used_fraction(),
                resets: w.resets_at_ms.map(|at| {
                    format!(
                        "resets in {}",
                        crate::summary::age_label(at.saturating_sub(now_ms))
                    )
                }),
            })
    });
    if !signed_out && who.is_none() && meter.is_none() {
        return None;
    }
    Some(Row {
        name: crate::summary::executor_name(&executor),
        executor,
        who: match (&who, signed_out) {
            (_, true) => "Signed out".to_owned(),
            (Some(email), false) => email.clone(),
            (None, false) => "Signed in".to_owned(),
        },
        signed_out,
        plan: usage.and_then(|u| u.plan_label.clone()),
        meter,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_proto::{
        ExecutorHealth, InstallStatus, ModelCatalog, PlanUsage, UsageWindow, VersionGate,
    };

    use super::*;
    use crate::vm::executors::tests_support::info;

    const NOW: u64 = 1_000 * 3_600_000;
    const HOUR: u64 = 3_600_000;

    fn health(signed_in: SignedIn, usage: Option<PlanUsage>) -> ExecutorHealth {
        ExecutorHealth {
            installed: InstallStatus::Installed {
                path: "/usr/local/bin/x".into(),
                version: None,
            },
            version_gate: VersionGate::Ok,
            signed_in,
            models: ModelCatalog::curated(vec![]),
            checked_at_ms: NOW,
            plan_usage: usage,
        }
    }

    fn usage(plan: Option<&str>, windows: Vec<UsageWindow>) -> Option<PlanUsage> {
        Some(PlanUsage {
            plan_label: plan.map(str::to_owned),
            windows,
            fetched_at_ms: NOW,
        })
    }

    fn world() -> Vec<ExecutorInfo> {
        vec![
            info(
                "acp.grok",
                Some(health(
                    SignedIn::Yes {
                        account: Some("ada@example.com".into()),
                    },
                    None,
                )),
            ),
            info(
                "codex",
                Some(health(
                    SignedIn::No {
                        hint: "run `codex login`".into(),
                    },
                    usage(
                        Some("ChatGPT Plus"),
                        vec![UsageWindow::new("Session", 0.12, None)],
                    ),
                )),
            ),
            info(
                "claude-code",
                Some(health(
                    SignedIn::Yes {
                        account: Some("ada@example.com".into()),
                    },
                    usage(
                        None,
                        vec![
                            UsageWindow::new("Session", 0.54, Some(NOW + 2 * HOUR)),
                            UsageWindow::new("Week", 0.21, Some(NOW + 70 * HOUR)),
                        ],
                    ),
                )),
            ),
            info(
                "cursor",
                Some(health(
                    SignedIn::No {
                        hint: "set CURSOR_API_KEY".into(),
                    },
                    None,
                )),
            ),
            info("pi", Some(health(SignedIn::Unknown, None))),
            info("muse", None),
        ]
    }

    #[test]
    fn the_title_is_the_tightest_meter_in_words() {
        let v = view(&world(), NOW);
        assert_eq!(v.title, "Claude Code is 54% into its session");
        assert_eq!(
            v.detail,
            "It resets in 2h. Codex and Cursor are signed out; sign in from Executors."
        );
        assert!(!v.warn);
    }

    #[test]
    fn meters_first_then_signed_in_then_signed_out_and_no_meter_when_signed_out() {
        let v = view(&world(), NOW);
        let names: Vec<_> = v.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Claude Code", "Grok", "Codex", "Cursor"]);
        let claude = &v.rows[0];
        assert_eq!(claude.who, "ada@example.com");
        assert_eq!(
            claude.meter.as_ref().unwrap().words(),
            "Session 54% \u{b7} resets in 2h"
        );
        let codex = &v.rows[2];
        assert!(codex.signed_out && codex.meter.is_none());
        assert_eq!(codex.who, "Signed out");
        assert_eq!(codex.plan.as_deref(), Some("ChatGPT Plus"));
    }

    #[test]
    fn a_meter_at_eighty_percent_warns() {
        let near = vec![info(
            "claude-code",
            Some(health(
                SignedIn::Yes { account: None },
                usage(None, vec![UsageWindow::new("Week", 0.86, None)]),
            )),
        )];
        let v = view(&near, NOW);
        assert_eq!(v.title, "Claude Code is 86% into its week");
        assert!(v.warn);
        assert_eq!(v.rows[0].who, "Signed in");
        assert!(v.rows[0].meter.as_ref().unwrap().near_limit());
    }

    #[test]
    fn with_no_usage_the_title_says_so() {
        let none = vec![info(
            "cursor",
            Some(health(SignedIn::No { hint: "x".into() }, None)),
        )];
        let v = view(&none, NOW);
        assert_eq!(v.title, "No usage to show");
        assert_eq!(v.detail, "Cursor is signed out; sign in from Executors.");
        assert_eq!(v.rows.len(), 1);
    }
}
