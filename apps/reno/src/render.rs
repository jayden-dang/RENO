//! What the CLI prints: a transcript as plain text or as JSON lines, and one-line summaries.
//!
//! Human output is short and plain: assistant text as it arrives, then one line for each tool
//! call, result, question and message. JSON output is the protocol's own types, one object per
//! line for streams.

use std::collections::HashSet;
use std::io::{self, Write};

use reno_proto::defs::format_instant;
use reno_proto::{
    Approval, ApprovalId, ApprovalOutcome, ApprovalRecord, ApprovalStatus, ApprovalSubject,
    CleanItem, CleanOutcome, DaemonStatusInfo, ExecEvent, ExecutorInfo, FiringOutcome,
    InstallStatus, ModelSource, NotInstalledReason, ResolvedBy, RoutingDecision, RunEventItem,
    RunStatus, RunSummary, SignedIn, StopReason, TriggerFireResult, TriggerInfo, TriggerStatus,
    VersionGate, WorkspaceInfo,
};
use serde::Serialize;

/// How long a tool call's input may be before it is cut.
const INPUT_WIDTH: usize = 100;

/// The wire name of an enum that serialises as a plain string (`succeeded`, `completed`).
pub(crate) fn wire_name<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        _ => "unknown".to_owned(),
    }
}

/// How a run ended, as the stream's `Done` told it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finished {
    pub(crate) stop: StopReason,
    pub(crate) diagnostic: Option<String>,
}

/// Writes a run's events to `W`.
#[derive(Debug)]
pub(crate) struct Transcript<W: Write> {
    out: W,
    json: bool,
    /// The last thing written was text with no newline after it.
    mid_line: bool,
}

impl<W: Write> Transcript<W> {
    pub(crate) fn new(out: W, json: bool) -> Self {
        Self {
            out,
            json,
            mid_line: false,
        }
    }

    /// Writes one event. Returns how the run ended when this was its `Done`.
    pub(crate) fn event(&mut self, item: &RunEventItem) -> io::Result<Option<Finished>> {
        let finished = match &item.event {
            ExecEvent::Done {
                stop, diagnostic, ..
            } => Some(Finished {
                stop: *stop,
                diagnostic: diagnostic.clone(),
            }),
            _ => None,
        };
        if self.json {
            serde_json::to_writer(&mut self.out, item).map_err(io::Error::other)?;
            writeln!(self.out)?;
        } else {
            self.human(&item.event)?;
        }
        self.out.flush()?;
        Ok(finished)
    }

    fn human(&mut self, event: &ExecEvent) -> io::Result<()> {
        if let ExecEvent::Text { text } = event {
            write!(self.out, "{text}")?;
            self.mid_line = !text.ends_with('\n');
            return Ok(());
        }
        // Not part of the plain transcript.
        if matches!(
            event,
            ExecEvent::Started { .. } | ExecEvent::Reasoning { .. } | ExecEvent::Usage { .. }
        ) {
            return Ok(());
        }
        self.end_line()?;
        match event {
            ExecEvent::Tool { call, .. } => {
                let input = call.input.to_string();
                if input == "null" || input == "{}" {
                    writeln!(self.out, "> {}", call.name)
                } else {
                    writeln!(self.out, "> {} {}", call.name, shorten(&input, INPUT_WIDTH))
                }
            }
            ExecEvent::ToolResult { ok, summary, .. } => {
                let outcome = if *ok { "ok" } else { "failed" };
                match summary {
                    Some(summary) => writeln!(self.out, "  {outcome}: {summary}"),
                    None => writeln!(self.out, "  {outcome}"),
                }
            }
            ExecEvent::NeedsApproval { subject, .. } => {
                let what = subject.command.as_deref().unwrap_or("");
                writeln!(self.out, "? approval needed: {} {what}", subject.tool)
            }
            ExecEvent::ApprovalResolved {
                decision,
                by,
                message,
                ..
            } => writeln!(
                self.out,
                "  {}",
                resolved_text(*decision, by, message.as_deref())
            ),
            ExecEvent::NeedsInput { questions, .. } => {
                for question in questions {
                    writeln!(self.out, "? the run asks: {}", question.prompt)?;
                }
                writeln!(self.out, "  (the CLI cannot answer questions yet)")
            }
            ExecEvent::UserMessage { text } => writeln!(self.out, "you: {text}"),
            ExecEvent::AgentReport {
                status,
                summary,
                details,
            } => {
                writeln!(self.out, "report ({}): {summary}", wire_name(status))?;
                match details {
                    Some(details) => writeln!(self.out, "  {}", shorten(details, INPUT_WIDTH)),
                    None => Ok(()),
                }
            }
            ExecEvent::Done {
                stop,
                diagnostic,
                reason,
                ..
            } => {
                writeln!(self.out, "done ({})", wire_name(stop))?;
                // The typed reason; a daemon that predates it only has the diagnostic's tag.
                let budget = reason.as_ref().map_or_else(
                    || {
                        diagnostic
                            .as_deref()
                            .and_then(reno_proto::BudgetStop::from_diagnostic)
                            .map(|stop| stop.to_string())
                    },
                    |reason| Some(reason.to_string()),
                );
                match budget {
                    Some(why) => writeln!(self.out, "  {why}"),
                    None => Ok(()),
                }
            }
            ExecEvent::Resumed { from_attempt, .. } => writeln!(
                self.out,
                "resumed: attempt {} continues the session",
                from_attempt.saturating_add(1)
            ),
            // Text and the quiet kinds returned above; a kind added later prints nothing.
            _ => Ok(()),
        }
    }

    fn end_line(&mut self) -> io::Result<()> {
        if self.mid_line {
            writeln!(self.out)?;
            self.mid_line = false;
        }
        Ok(())
    }

    /// What to do about one held request (human output only): the command that answers it, or
    /// who already did.
    pub(crate) fn approval_hint(&mut self, hint: &ApprovalHint) -> io::Result<()> {
        if self.json {
            return Ok(());
        }
        match hint {
            ApprovalHint::Pending(id) => {
                writeln!(
                    self.out,
                    "  reno approve {id}   reno deny {id}   reno deny {id} --message \"…\""
                )?;
            }
            ApprovalHint::Decided(text) => writeln!(self.out, "  already decided: {text}")?,
            ApprovalHint::Unknown => {}
        }
        self.out.flush()
    }
}

/// What the hint after a `NeedsApproval` event says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApprovalHint {
    /// Still held: the id that answers this request.
    Pending(ApprovalId),
    /// Already answered, and by whom.
    Decided(String),
    /// The daemon has no record of it (yet); say nothing rather than point at another request.
    Unknown,
}

/// The hint for the request `request_id` with `subject`.
///
/// A decided request is found in `history` by its request id and says who decided. A held one is
/// not listed with its request id, so it is the oldest pending approval of the same subject that
/// an earlier request of this transcript has not claimed (`claimed` records the ids handed out).
/// Requests are recorded in the order they are raised, so this pairs each with its own id.
pub(crate) fn approval_hint_for(
    request_id: &str,
    subject: &ApprovalSubject,
    pending: &[Approval],
    history: &[ApprovalRecord],
    claimed: &mut HashSet<ApprovalId>,
) -> ApprovalHint {
    if let Some(record) = history.iter().find(|r| r.request_id == request_id) {
        let outcome = match record.status {
            ApprovalStatus::Approved => ApprovalOutcome::Approved,
            _ => ApprovalOutcome::Denied,
        };
        return match &record.decided_by {
            Some(by) => {
                ApprovalHint::Decided(resolved_text(outcome, by, record.message.as_deref()))
            }
            None => ApprovalHint::Decided(wire_name(&record.status)),
        };
    }
    match pending
        .iter()
        .find(|a| &a.subject == subject && !claimed.contains(&a.id))
    {
        Some(approval) => {
            claimed.insert(approval.id.clone());
            ApprovalHint::Pending(approval.id.clone())
        }
        None => ApprovalHint::Unknown,
    }
}

/// `text` cut to `width` characters, with an ellipsis when it was longer.
fn shorten(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(width).collect();
    cut.push('…');
    cut
}

/// A routing decision as lines for `run show`: who was chosen and by which rule, then the
/// executors that were passed over with their reasons, then the fallback walk.
///
/// Executors the task or its bot leaves out are one line per reason (`6 others not listed by bot
/// keeper`) unless `verbose`, which names each of them.
pub(crate) fn routing_lines(decision: &RoutingDecision, verbose: bool) -> Vec<String> {
    let summary = if verbose {
        decision.explain_verbose()
    } else {
        decision.explain()
    };
    let mut lines = vec![format!("routing: {summary}")];
    if !decision.ranked.is_empty() {
        let ranked: Vec<&str> = decision.ranked.iter().map(|id| id.as_str()).collect();
        lines.push(format!("  ranked: {}", ranked.join(", ")));
    }
    for rejection in &decision.rejected {
        let left_out = matches!(rejection.reason, reno_proto::RejectReason::Excluded { .. });
        if verbose || !left_out {
            lines.push(format!(
                "  rejected: {}: {}",
                rejection.executor, rejection.reason
            ));
        }
    }
    if !verbose {
        lines.extend(
            decision
                .left_out_lines()
                .into_iter()
                .map(|line| format!("  {line}")),
        );
    }
    for step in &decision.fallbacks {
        let to = step.to.as_ref().map_or("nothing left", |id| id.as_str());
        lines.push(format!(
            "  fallback: {} -> {to}: {}",
            step.from, step.reason
        ));
    }
    for warning in &decision.warnings {
        lines.push(format!("  warning: {warning}"));
    }
    lines
}

/// The model a run asks for (a task's, else its bot's) or reports, for `run show`; `None` when
/// the executor's default applies and it has not started.
pub(crate) fn model_line(run: &RunSummary) -> Option<String> {
    run.model.as_ref().map(|model| format!("model: {model}"))
}

/// One run on one line: `id  status  executor  title`.
pub(crate) fn run_line(run: &RunSummary) -> String {
    format!(
        "{}  {:<16}  {:<12}  {}",
        run.run_id,
        status_name(run.status),
        run.executor.as_str(),
        run.title
    )
}

/// What a run's stop and resume state add to its one line, as extra lines: why it stopped, and
/// why it was not resumed.
pub(crate) fn stop_lines(run: &RunSummary) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(stop) = &run.stop {
        lines.push(format!("stopped: {stop}"));
    }
    if let Some(why) = &run.not_resumed {
        lines.push(format!(
            "not resumed: {why}; `reno run resume {}` continues it",
            run.run_id
        ));
    }
    lines
}

pub(crate) fn status_name(status: RunStatus) -> String {
    wire_name(&status)
}

/// Who or what decided, as a short phrase: `you`, `rule cargo is fine`, `its time limit`.
pub(crate) fn by_text(by: &ResolvedBy) -> String {
    match by {
        ResolvedBy::Human => "you".to_owned(),
        ResolvedBy::Rule { name } => format!("rule {name}"),
        ResolvedBy::Expired => "the time limit".to_owned(),
        ResolvedBy::RepeatDenial => "the repeat-denial rule".to_owned(),
    }
}

/// The transcript line of a resolved approval: `approved by rule cargo is fine`.
pub(crate) fn resolved_text(
    decision: ApprovalOutcome,
    by: &ResolvedBy,
    message: Option<&str>,
) -> String {
    let outcome = match decision {
        ApprovalOutcome::Approved => "approved",
        ApprovalOutcome::Denied => "denied",
    };
    let line = match by {
        ResolvedBy::Expired => format!("{outcome}: nobody answered in time"),
        ResolvedBy::RepeatDenial => format!("{outcome}: it was denied earlier in this run"),
        by => format!("{outcome} by {}", by_text(by)),
    };
    match message {
        Some(message) => format!("{line} ({message})"),
        None => line,
    }
}

/// One decided approval on one line: `id  run  status  by  tool command`.
pub(crate) fn history_line(record: &ApprovalRecord) -> String {
    let subject = &record.subject;
    let what = subject.command.as_deref().unwrap_or("");
    let by = record
        .decided_by
        .as_ref()
        .map_or_else(|| "nobody yet".to_owned(), by_text);
    let line = format!(
        "{}  run {}  {:<8}  by {by}  {} {what}",
        record.id,
        record.run_id,
        wire_name(&record.status),
        subject.tool
    );
    match &record.message {
        Some(message) => format!("{line} ({message})"),
        None => line,
    }
}

/// One pending approval on one line, in the columns of [`history_line`]: nobody has decided it
/// yet. (An `Approval` does not say which rule asked, so only a decided one can name a rule.)
pub(crate) fn approval_line(approval: &Approval) -> String {
    let subject = &approval.subject;
    let what = subject.command.as_deref().unwrap_or("");
    format!(
        "{}  run {}  {:<8}  by nobody yet  {} {what}",
        approval.id, approval.run_id, "pending", subject.tool
    )
}

/// One workspace on one line: run, status, branch, what it holds and where it is.
pub(crate) fn workspace_line(workspace: &WorkspaceInfo) -> String {
    let mut line = format!(
        "{} {:<9} {}",
        workspace.run_id,
        wire_name(&workspace.status),
        workspace.branch
    );
    if let Some(changes) = workspace.changes {
        line.push_str(&format!("  {} commit(s)", changes.commits));
        if changes.dirty {
            line.push_str(", uncommitted changes");
        }
    }
    line.push_str(&format!("  {}", workspace.path.display()));
    line
}

/// Every field of a workspace, one per line.
pub(crate) fn workspace_detail(workspace: &WorkspaceInfo) -> Vec<String> {
    let at = |ms: u64| format_instant(i64::try_from(ms).unwrap_or(i64::MAX), None);
    let mut lines = vec![
        format!("run        {}", workspace.run_id),
        format!("status     {}", wire_name(&workspace.status)),
        format!("repository {}", workspace.repo.display()),
        format!("worktree   {}", workspace.path.display()),
        format!("branch     {}", workspace.branch),
        format!("base       {}", workspace.base_sha),
    ];
    if !workspace.subdir.is_empty() {
        lines.push(format!("subdir     {}", workspace.subdir));
    }
    if let Some(changes) = workspace.changes {
        lines.push(format!(
            "holds      {} commit(s){}",
            changes.commits,
            if changes.dirty {
                ", uncommitted changes"
            } else {
                ""
            }
        ));
    }
    lines.push(format!("created    {}", at(workspace.created_at_ms)));
    lines.push(format!("updated    {}", at(workspace.updated_at_ms)));
    lines
}

/// What `reno workspace clean` says about one workspace.
pub(crate) fn clean_line(item: &CleanItem) -> String {
    match (item.outcome, &item.reason) {
        (CleanOutcome::Removed, _) => format!("{} removed", item.run_id),
        (CleanOutcome::Kept, Some(why)) => format!("{} kept: {why}", item.run_id),
        (CleanOutcome::Kept, None) => format!("{} kept", item.run_id),
    }
}

/// The unrecognised frames, a line per executor and an indented line per kind.
pub(crate) fn frames_lines(executors: &[reno_proto::ExecutorFrameCounts]) -> Vec<String> {
    if executors.is_empty() {
        return vec!["no executors".to_owned()];
    }
    let mut lines = Vec::new();
    for executor in executors {
        let version = executor.cli_version.as_deref().unwrap_or("version unknown");
        if executor.frames.is_empty() {
            lines.push(format!(
                "{:<16} {version}: no unrecognised frames",
                executor.id.as_str()
            ));
            continue;
        }
        lines.push(format!(
            "{:<16} {version}: {} unrecognised frame kind(s)",
            executor.id.as_str(),
            executor.frames.len()
        ));
        for frame in &executor.frames {
            lines.push(format!(
                "  {:<40} {:>6}  first seen {}",
                frame.kind,
                frame.count,
                format_instant(i64::try_from(frame.first_seen_ms).unwrap_or(0), None),
            ));
        }
    }
    lines
}

/// One executor on one line: id, whether it is installed (and at which version), the version
/// gate, whether it is signed in, and the models with where the list came from.
pub(crate) fn executor_line(info: &ExecutorInfo) -> String {
    let id = info.descriptor.id.as_str();
    let Some(health) = &info.health else {
        return format!("{id:<16} not probed yet");
    };
    let mut line = format!("{id:<16} ");
    match &health.installed {
        InstallStatus::Installed { version, .. } => {
            line.push_str("installed ");
            line.push_str(version.as_deref().unwrap_or("(version unknown)"));
        }
        InstallStatus::NotInstalled { why } => {
            line.push_str(&match why {
                NotInstalledReason::NotFound { hint } => format!("not installed: {hint}"),
                NotInstalledReason::NeedsInstall { what } => {
                    format!("needs install: {what}")
                }
                NotInstalledReason::Unusable { detail } => format!("unusable: {detail}"),
                NotInstalledReason::ProbeFailed { detail } => {
                    format!("probe failed: {detail}")
                }
            });
            return line;
        }
    }
    match &health.version_gate {
        VersionGate::Ok | VersionGate::Unchecked => {}
        VersionGate::TooOld { min } => line.push_str(&format!(" TOO OLD (needs {min})")),
        VersionGate::Untested { verified } => {
            line.push_str(&format!(" untested (verified {verified})"));
        }
    }
    match &health.signed_in {
        SignedIn::Yes { account: Some(who) } => line.push_str(&format!("  signed in as {who}")),
        SignedIn::Yes { account: None } => line.push_str("  signed in"),
        SignedIn::No { hint } => line.push_str(&format!("  NOT SIGNED IN: {hint}")),
        SignedIn::Unknown => line.push_str("  sign-in unknown"),
    }
    let source = match health.models.source {
        ModelSource::Curated => "curated",
        ModelSource::Live => "live",
    };
    line.push_str(&format!(
        "  {} models ({source})",
        health.models.models.len()
    ));
    line
}

/// One trigger on one line: name, kind, status, then what is known of its timing.
pub(crate) fn trigger_line(trigger: &TriggerInfo) -> String {
    let at = |ms: u64| format_instant(i64::try_from(ms).unwrap_or(i64::MAX), None);
    let mut line = format!(
        "{:<16} {:<7} {:<10} bot {}",
        trigger.name,
        trigger.kind,
        status_text_of(trigger.status),
        trigger.bot
    );
    if let Some(next) = trigger.next_due_at_ms {
        line.push_str(&format!("  next {}", at(next)));
    }
    if let Some(last) = trigger.last_fired_at_ms {
        line.push_str(&format!("  last {}", at(last)));
    }
    if trigger.failures > 0 {
        line.push_str(&format!("  failures {}", trigger.failures));
    }
    if let Some(until) = trigger.backoff_until_ms {
        line.push_str(&format!("  backed off until {}", at(until)));
    }
    if let Some(error) = &trigger.error {
        line.push_str(&format!("  error: {error}"));
    }
    line
}

fn status_text_of(status: TriggerStatus) -> &'static str {
    match status {
        TriggerStatus::Scheduled => "scheduled",
        TriggerStatus::Watching => "watching",
        TriggerStatus::Manual => "manual",
        TriggerStatus::Done => "done",
        TriggerStatus::BackedOff => "backed off",
        TriggerStatus::Disabled => "disabled",
        TriggerStatus::Failed => "failed",
    }
}

/// What `reno trigger fire` says.
pub(crate) fn fired_line(name: &str, fired: &TriggerFireResult) -> String {
    let run = fired.run_id.as_ref().map(ToString::to_string);
    match (fired.outcome, run) {
        (FiringOutcome::Fired, Some(run)) => format!("{name} fired: run {run}"),
        (FiringOutcome::Coalesced, Some(run)) => {
            format!("{name} is still running {run}; this occurrence was merged into it")
        }
        (FiringOutcome::Duplicate, _) => {
            format!("{name} had already fired for this occurrence; nothing was done")
        }
        (FiringOutcome::BackedOff, _) => format!("{name} is backed off"),
        (outcome, None) => format!("{name}: {outcome:?}"),
    }
}

/// One standing rule on one line: id, effect, scope, what it matches, and its end.
/// `now_ms` is the time to read the expiry against.
pub(crate) fn rule_line(rule: &reno_proto::StandingRule, now_ms: u64) -> String {
    use reno_proto::{CommandPattern, StandingScope};
    let scope = match &rule.scope {
        StandingScope::Run { run } => format!("run {run}"),
        StandingScope::Bot { bot } => format!("bot {bot}"),
        StandingScope::Always => "always".to_owned(),
    };
    let matcher = &rule.matcher;
    let mut what = format!("{} {}", wire_name(&matcher.action), matcher.tool);
    match &matcher.command {
        Some(CommandPattern::Exact { command }) => what.push_str(&format!(" `{command}`")),
        Some(CommandPattern::Glob { pattern }) => what.push_str(&format!(" matching `{pattern}`")),
        None => {}
    }
    if !matcher.paths.is_empty() {
        what.push_str(&format!(" on {}", matcher.paths.join(", ")));
    }
    let end = match (rule.revoked_at_ms, rule.expires_at_ms) {
        (Some(_), _) => "  revoked".to_owned(),
        (None, Some(at)) if at <= now_ms => "  expired".to_owned(),
        (None, Some(at)) => format!("  expires in {}", duration((at - now_ms) / 1000)),
        (None, None) => String::new(),
    };
    format!(
        "{}  {:<5}  {scope}  {what}{end}",
        rule.id,
        wire_name(&rule.effect)
    )
}

/// `73` as `1m 13s`.
pub(crate) fn duration(secs: u64) -> String {
    let (hours, minutes, seconds) = (secs / 3600, secs / 60 % 60, secs % 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m {seconds:02}s"),
        _ => format!("{hours}h {minutes:02}m"),
    }
}

/// What `reno pause` says.
pub(crate) fn pause_line(bot: Option<&str>, result: &reno_proto::BudgetPauseResult) -> String {
    let what = bot.map_or_else(|| "everything".to_owned(), |name| format!("bot {name}"));
    let mut line = if result.changed {
        format!("paused {what}: nothing new will start; triggers wait for `reno resume`")
    } else {
        format!("{what} was already paused")
    };
    if !result.stopped.is_empty() {
        let runs: Vec<_> = result.stopped.iter().map(ToString::to_string).collect();
        line.push_str(&format!(
            "\nstopping {} live run(s): {}",
            runs.len(),
            runs.join(", ")
        ));
    }
    line
}

/// What `reno resume` says.
pub(crate) fn resume_line(bot: Option<&str>, result: &reno_proto::BudgetResumeResult) -> String {
    let what = bot.map_or_else(|| "everything".to_owned(), |name| format!("bot {name}"));
    if result.changed {
        format!("resumed {what}")
    } else {
        format!("{what} was not paused")
    }
}

/// `reno budget`: the state of the kill switch, then one line per bot.
pub(crate) fn budget_lines(status: &reno_proto::BudgetStatusResult) -> Vec<String> {
    let mut lines = vec![format!(
        "{}; tokens counted for {} (local day), cache reads at {}%; runs with no bot may take {}",
        if status.paused {
            "PAUSED: nothing new starts"
        } else {
            "running"
        },
        status.day,
        f64::from(status.cache_read_weight_permille) / 10.0,
        duration(status.default_wall_time_ms / 1000),
    )];
    for bot in &status.bots {
        let ceiling = bot.daily_token_ceiling.map_or_else(
            || "no ceiling".to_owned(),
            |ceiling| format!("of {ceiling}"),
        );
        let cache = if bot.cache_read_tokens_today + bot.cache_write_tokens_today > 0 {
            format!(
                ", cache {} read / {} written",
                bot.cache_read_tokens_today, bot.cache_write_tokens_today
            )
        } else {
            String::new()
        };
        lines.push(format!(
            "{}{}: {}/{} live, {} queued, {} tokens today ({ceiling}{cache}), runs may take {}",
            bot.bot,
            if bot.paused { " (paused)" } else { "" },
            bot.live_runs,
            bot.max_concurrent_runs,
            bot.queued_runs,
            bot.tokens_today,
            duration(bot.max_wall_time_ms / 1000),
        ));
    }
    lines
}

/// The text of `reno daemon status` for a running daemon.
pub(crate) fn status_text(info: &DaemonStatusInfo, socket: &std::path::Path) -> String {
    let executors: Vec<_> = info.executors.iter().map(|e| e.as_str()).collect();
    format!(
        "running: pid {}, renod {}, up {}\nruns: {} active, {} queued\nexecutors: {}\nsocket: {}",
        info.pid,
        info.version,
        duration(info.uptime_secs),
        info.active_runs,
        info.queued_runs,
        if executors.is_empty() {
            "none".to_owned()
        } else {
            executors.join(", ")
        },
        socket.display()
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn budget_lines_show_what_the_ceiling_counted_and_the_cache_behind_it() {
        let status: reno_proto::BudgetStatusResult = serde_json::from_value(serde_json::json!({
            "paused": false, "day": "2026-11-16", "dayEndsAtMs": 1, "defaultWallTimeMs": 14_400_000,
            "cacheReadWeightPermille": 100,
            "bots": [
                {"bot": "keeper", "liveRuns": 1, "maxConcurrentRuns": 2, "queuedRuns": 0,
                 "tokensToday": 530, "cacheReadTokensToday": 5000, "cacheWriteTokensToday": 10,
                 "dailyTokenCeiling": 1000, "maxWallTimeMs": 2_700_000, "paused": false},
                {"bot": "scribe", "liveRuns": 0, "maxConcurrentRuns": 1, "queuedRuns": 0,
                 "tokensToday": 0, "maxWallTimeMs": 60_000, "paused": false}
            ]
        }))
        .unwrap();
        let lines = super::budget_lines(&status);
        assert!(lines[0].contains("cache reads at 10%"), "{}", lines[0]);
        assert!(
            lines[1].contains("530 tokens today (of 1000, cache 5000 read / 10 written)"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].contains("0 tokens today (no ceiling)"),
            "{}",
            lines[2]
        );
    }

    fn executor(health: Option<reno_proto::ExecutorHealth>) -> ExecutorInfo {
        ExecutorInfo {
            descriptor: serde_json::from_value(serde_json::json!({
                "id": "codex", "contractVersion": 1, "adapterVersion": "x",
                "cliVerified": {"min": "0.1.0", "verified": "0.2.0"},
                "caps": {"steering": "none", "resume": false, "accessModes": [],
                         "sandboxLevels": [], "mcpInjection": false, "structuredOutput": false,
                         "deterministicTurnEnd": false, "planMode": false, "models": []}
            }))
            .unwrap(),
            health,
        }
    }

    #[test]
    fn an_executor_line_says_what_the_probe_found() {
        use reno_proto::ModelCatalog;
        assert_eq!(
            executor_line(&executor(None)),
            "codex            not probed yet"
        );
        let mut health = reno_proto::ExecutorHealth {
            installed: InstallStatus::Installed {
                path: "/bin/codex".into(),
                version: Some("0.3.0".into()),
            },
            version_gate: VersionGate::Untested {
                verified: "0.2.0".into(),
            },
            signed_in: SignedIn::No {
                hint: "run `codex login`".into(),
            },
            models: ModelCatalog::live(vec![]),
            checked_at_ms: 1,
        };
        assert_eq!(
            executor_line(&executor(Some(health.clone()))),
            "codex            installed 0.3.0 untested (verified 0.2.0)  NOT SIGNED IN: run `codex login`  0 models (live)"
        );
        health.installed = InstallStatus::NotInstalled {
            why: NotInstalledReason::NeedsInstall {
                what: "the adapter".into(),
            },
        };
        assert_eq!(
            executor_line(&executor(Some(health))),
            "codex            needs install: the adapter"
        );
    }

    #[test]
    fn frames_are_listed_per_executor_with_their_kinds() {
        use reno_proto::{ExecutorFrameCounts, UnknownFrameKind};
        assert_eq!(frames_lines(&[]), ["no executors"]);
        let quiet = ExecutorFrameCounts {
            id: "claude-code".parse().unwrap(),
            cli_version: None,
            frames: vec![],
        };
        let loud = ExecutorFrameCounts {
            id: "codex".parse().unwrap(),
            cli_version: Some("0.170.0".into()),
            frames: vec![UnknownFrameKind {
                kind: "method:thread/new".into(),
                count: 3,
                first_seen_ms: 0,
            }],
        };
        let lines = frames_lines(&[quiet, loud]);
        assert_eq!(
            lines[0],
            "claude-code      version unknown: no unrecognised frames"
        );
        assert_eq!(
            lines[1],
            "codex            0.170.0: 1 unrecognised frame kind(s)"
        );
        assert!(lines[2].starts_with("  method:thread/new"), "{}", lines[2]);
        assert!(lines[2].contains("1970-01-01"), "{}", lines[2]);
    }

    use reno_proto::{ActionClass, Question, Seq, ToolCall};
    use serde_json::json;

    use super::*;

    fn item(seq: u64, event: ExecEvent) -> RunEventItem {
        RunEventItem {
            seq: Seq(seq),
            event,
        }
    }

    fn render(json: bool, events: Vec<ExecEvent>) -> (String, Option<Finished>) {
        let mut out = Vec::new();
        let mut last = None;
        let mut transcript = Transcript::new(&mut out, json);
        for (n, event) in events.into_iter().enumerate() {
            last = transcript
                .event(&item(n as u64 + 1, event))
                .unwrap()
                .or(last);
        }
        (String::from_utf8(out).unwrap(), last)
    }

    fn text(s: &str) -> ExecEvent {
        ExecEvent::Text { text: s.into() }
    }

    fn done(stop: StopReason) -> ExecEvent {
        ExecEvent::Done {
            stop,
            result: None,
            diagnostic: Some("boom".into()),
            reason: None,
        }
    }

    #[test]
    fn text_streams_as_it_arrives_and_lines_start_on_a_fresh_line() {
        let (out, finished) = render(
            false,
            vec![
                ExecEvent::Started {
                    session: reno_proto::SessionRef("s".into()),
                    model: "m".into(),
                },
                text("Hello "),
                text("world"),
                ExecEvent::Tool {
                    id: "1".into(),
                    call: ToolCall::new("grep", json!({"pattern": "x"}), ActionClass::Read),
                },
                ExecEvent::ToolResult {
                    id: "1".into(),
                    ok: false,
                    summary: Some("no match".into()),
                },
                text("bye\n"),
                done(StopReason::Completed),
            ],
        );
        assert_eq!(
            out,
            "Hello world\n> grep {\"pattern\":\"x\"}\n  failed: no match\nbye\ndone (completed)\n"
        );
        assert_eq!(
            finished,
            Some(Finished {
                stop: StopReason::Completed,
                diagnostic: Some("boom".into())
            })
        );
    }

    #[test]
    fn a_resolved_approval_says_who_decided_and_why() {
        let resolved = |decision, by, message: Option<&str>| ExecEvent::ApprovalResolved {
            request_id: "r".into(),
            decision,
            by,
            message: message.map(str::to_owned),
        };
        let (out, _) = render(
            false,
            vec![
                resolved(
                    ApprovalOutcome::Approved,
                    ResolvedBy::Rule {
                        name: "cargo is fine".into(),
                    },
                    None,
                ),
                resolved(
                    ApprovalOutcome::Denied,
                    ResolvedBy::Rule {
                        name: "force push".into(),
                    },
                    Some("denied by policy: force push"),
                ),
                resolved(ApprovalOutcome::Approved, ResolvedBy::Human, None),
                resolved(ApprovalOutcome::Denied, ResolvedBy::Expired, None),
                resolved(ApprovalOutcome::Denied, ResolvedBy::RepeatDenial, None),
            ],
        );
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            [
                "  approved by rule cargo is fine",
                "  denied by rule force push (denied by policy: force push)",
                "  approved by you",
                "  denied: nobody answered in time",
                "  denied: it was denied earlier in this run",
            ]
        );
        // JSON output is the event as it is.
        let (out, _) = render(
            true,
            vec![resolved(ApprovalOutcome::Denied, ResolvedBy::Expired, None)],
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&out).unwrap(),
            json!({"seq": 1, "event": {
                "type": "approval_resolved", "requestId": "r",
                "decision": "denied", "by": {"type": "expired"}
            }})
        );
    }

    fn bot_routing() -> RoutingDecision {
        use reno_proto::{Rejection, RuleMatched};
        let unlisted = |name: &str| Rejection {
            executor: name.parse().unwrap(),
            reason: reno_proto::RejectReason::unlisted_by_bot("keeper"),
        };
        RoutingDecision {
            chosen: Some("claude-code".parse().unwrap()),
            ranked: vec!["claude-code".parse().unwrap(), "codex".parse().unwrap()],
            rejected: vec![
                unlisted("gemini"),
                Rejection {
                    executor: "opencode".parse().unwrap(),
                    reason: reno_proto::RejectReason::SignedOut {
                        hint: "run `opencode auth`".into(),
                    },
                },
                unlisted("acp.a"),
                unlisted("acp.b"),
            ],
            rule_matched: RuleMatched::BotPreference {
                bot: "keeper".into(),
            },
            fallbacks: vec![],
            at_ms: 0,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn a_routing_warning_is_shown_under_the_decision() {
        let mut decision = bot_routing();
        decision.warnings = vec!["model \"x\" is not in claude-code's model list".into()];
        let lines = routing_lines(&decision, false);
        assert_eq!(
            lines.last().map(String::as_str),
            Some("  warning: model \"x\" is not in claude-code's model list")
        );
    }

    #[test]
    fn the_model_a_run_asks_for_is_a_line_of_run_show() {
        let mut run: RunSummary = serde_json::from_value(serde_json::json!({
            "runId": "run_01HZX9", "taskId": "task_01HZX9", "executor": "claude-code",
            "status": "queued", "attempt": 1, "title": "t", "createdAtMs": 1, "updatedAtMs": 1
        }))
        .unwrap();
        assert_eq!(model_line(&run), None);
        run.model = Some("claude-sonnet-4-6".into());
        assert_eq!(
            model_line(&run).as_deref(),
            Some("model: claude-sonnet-4-6")
        );
    }

    #[test]
    fn the_routing_summary_names_the_choice_the_fallback_and_collapses_unlisted_executors() {
        assert_eq!(
            routing_lines(&bot_routing(), false),
            [
                "routing: claude-code (bot keeper prefers it; fallback codex); not opencode: signed out (run `opencode auth`)",
                "  ranked: claude-code, codex",
                "  rejected: opencode: signed out (run `opencode auth`)",
                "  3 others not listed by bot keeper",
            ]
        );
        let verbose = routing_lines(&bot_routing(), true);
        assert_eq!(verbose.len(), 2 + 4, "{verbose:?}");
        assert!(verbose[0].contains("not gemini: excluded (bot keeper does not list it)"));
        assert!(
            verbose
                .contains(&"  rejected: gemini: excluded (bot keeper does not list it)".to_owned())
        );
        // Nothing left out: no collapsed line, and the summary is just the choice.
        let plain = RoutingDecision {
            rejected: vec![],
            ..bot_routing()
        };
        assert_eq!(
            routing_lines(&plain, false),
            [
                "routing: claude-code (bot keeper prefers it; fallback codex)",
                "  ranked: claude-code, codex"
            ]
        );
    }

    fn held(id: &str, command: &str) -> Approval {
        Approval {
            id: id.parse().unwrap(),
            run_id: "run_r1".parse().unwrap(),
            subject: subject(command),
            requested_at_ms: 1,
        }
    }

    fn subject(command: &str) -> ApprovalSubject {
        ApprovalSubject {
            action: ActionClass::Command,
            tool: "bash".into(),
            command: Some(command.into()),
            paths: vec![],
            raw: json!(null),
        }
    }

    fn decided(id: &str, request: &str, command: &str, by: ResolvedBy) -> ApprovalRecord {
        ApprovalRecord {
            id: id.parse().unwrap(),
            run_id: "run_r1".parse().unwrap(),
            request_id: request.into(),
            subject: subject(command),
            subject_digest: None,
            status: ApprovalStatus::Approved,
            decided_by: Some(by),
            created_at_ms: 1,
            decided_at_ms: Some(2),
            expires_at_ms: 9,
            message: None,
        }
    }

    #[test]
    fn each_held_request_gets_its_own_approval_id() {
        let pending = [
            held("appr_a1", "git push"),
            held("appr_a2", "cargo update"),
            held("appr_a3", "cargo update"),
        ];
        let mut claimed = HashSet::new();
        let hint = |request: &str, command: &str, claimed: &mut HashSet<_>| {
            approval_hint_for(request, &subject(command), &pending, &[], claimed)
        };
        let id = |s: &str| ApprovalHint::Pending(s.parse().unwrap());
        assert_eq!(hint("r1", "git push", &mut claimed), id("appr_a1"));
        assert_eq!(hint("r2", "cargo update", &mut claimed), id("appr_a2"));
        assert_eq!(hint("r3", "cargo update", &mut claimed), id("appr_a3"));
        // Nothing left to claim: no hint, never another request's id.
        assert_eq!(
            hint("r4", "cargo update", &mut claimed),
            ApprovalHint::Unknown
        );
    }

    #[test]
    fn a_resolved_request_shows_who_decided_instead_of_commands() {
        let history = [decided(
            "appr_a1",
            "r1",
            "git push",
            ResolvedBy::Rule {
                name: "cargo is fine".into(),
            },
        )];
        let pending = [held("appr_a2", "git push")];
        let mut claimed = HashSet::new();
        let hint = approval_hint_for("r1", &subject("git push"), &pending, &history, &mut claimed);
        assert_eq!(
            hint,
            ApprovalHint::Decided("approved by rule cargo is fine".into())
        );
        assert!(claimed.is_empty());
        let mut out = Vec::new();
        let mut transcript = Transcript::new(&mut out, false);
        transcript.approval_hint(&hint).unwrap();
        transcript
            .approval_hint(&ApprovalHint::Pending("appr_a2".parse().unwrap()))
            .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "  already decided: approved by rule cargo is fine\n  reno approve appr_a2   reno deny appr_a2   reno deny appr_a2 --message \"…\"\n"
        );
    }

    #[test]
    fn a_pending_approval_line_has_the_history_columns_with_nobody_yet_deciding() {
        let approval = Approval {
            id: "appr_a1".parse().unwrap(),
            run_id: "run_r1".parse().unwrap(),
            subject: reno_proto::ApprovalSubject {
                action: ActionClass::Command,
                tool: "bash".into(),
                command: Some("git push".into()),
                paths: vec![],
                raw: json!(null),
            },
            requested_at_ms: 1,
        };
        assert_eq!(
            approval_line(&approval),
            "appr_a1  run run_r1  pending   by nobody yet  bash git push"
        );
    }

    #[test]
    fn a_rule_line_shows_scope_effect_what_it_holds_and_when_it_ends() {
        use reno_proto::{
            CommandPattern, RuleMatcher, StandingEffect, StandingRule, StandingScope,
        };
        let rule = StandingRule {
            id: "rule_a1".parse().unwrap(),
            scope: StandingScope::Bot {
                bot: "keeper".into(),
            },
            effect: StandingEffect::Allow,
            matcher: RuleMatcher {
                action: ActionClass::Command,
                tool: "bash".into(),
                command: Some(CommandPattern::Exact {
                    command: "git push".into(),
                }),
                paths: vec![],
            },
            created_by: "human".into(),
            created_at_ms: 0,
            expires_at_ms: None,
            revoked_at_ms: None,
            source_approval: None,
        };
        assert_eq!(
            rule_line(&rule, 10),
            "rule_a1  allow  bot keeper  command bash `git push`"
        );
        let timed = StandingRule {
            expires_at_ms: Some(3_700_000),
            ..rule.clone()
        };
        assert!(rule_line(&timed, 100_000).ends_with("  expires in 1h 00m"));
        assert!(rule_line(&timed, 3_700_000).ends_with("  expired"));
        let edit = StandingRule {
            effect: StandingEffect::Deny,
            scope: StandingScope::Always,
            matcher: RuleMatcher {
                action: ActionClass::Edit,
                tool: "write".into(),
                command: None,
                paths: vec!["src/lib.rs".into(), "/etc/hosts".into()],
            },
            revoked_at_ms: Some(5),
            ..rule
        };
        assert_eq!(
            rule_line(&edit, 10),
            "rule_a1  deny   always  edit write on src/lib.rs, /etc/hosts  revoked"
        );
    }

    #[test]
    fn a_history_line_names_the_decider_and_the_rule() {
        let record = |status, decided_by| ApprovalRecord {
            id: "appr_a1".parse().unwrap(),
            run_id: "run_r1".parse().unwrap(),
            request_id: "q".into(),
            subject: reno_proto::ApprovalSubject {
                action: ActionClass::Command,
                tool: "bash".into(),
                command: Some("cargo test".into()),
                paths: vec![],
                raw: json!(null),
            },
            subject_digest: None,
            status,
            decided_by,
            created_at_ms: 1,
            decided_at_ms: Some(2),
            expires_at_ms: 3,
            message: None,
        };
        assert_eq!(
            history_line(&record(
                reno_proto::ApprovalStatus::Approved,
                Some(ResolvedBy::Rule {
                    name: "cargo is fine".into()
                })
            )),
            "appr_a1  run run_r1  approved  by rule cargo is fine  bash cargo test"
        );
        assert_eq!(
            history_line(&record(
                reno_proto::ApprovalStatus::Denied,
                Some(ResolvedBy::Human)
            )),
            "appr_a1  run run_r1  denied    by you  bash cargo test"
        );
        let mut denied = record(reno_proto::ApprovalStatus::Denied, Some(ResolvedBy::Human));
        denied.message = Some("use a branch".into());
        assert_eq!(
            history_line(&denied),
            "appr_a1  run run_r1  denied    by you  bash cargo test (use a branch)"
        );
        assert_eq!(
            history_line(&record(
                reno_proto::ApprovalStatus::Expired,
                Some(ResolvedBy::Expired)
            )),
            "appr_a1  run run_r1  expired   by the time limit  bash cargo test"
        );
        assert_eq!(
            history_line(&record(
                reno_proto::ApprovalStatus::Denied,
                Some(ResolvedBy::RepeatDenial)
            )),
            "appr_a1  run run_r1  denied    by the repeat-denial rule  bash cargo test"
        );
    }

    #[test]
    fn questions_and_messages_are_one_line_each() {
        let (out, _) = render(
            false,
            vec![
                ExecEvent::NeedsInput {
                    request_id: "r".into(),
                    questions: vec![Question {
                        id: "q".into(),
                        prompt: "Which branch?".into(),
                        options: vec![],
                        multi_select: false,
                    }],
                },
                ExecEvent::UserMessage {
                    text: "go on".into(),
                },
            ],
        );
        assert!(out.contains("? the run asks: Which branch?\n"));
        assert!(out.ends_with("you: go on\n"));
    }

    #[test]
    fn a_report_prints_its_status_and_summary_and_a_shortened_detail() {
        let (out, _) = render(
            false,
            vec![ExecEvent::AgentReport {
                status: reno_proto::ReportStatus::Done,
                summary: "tests pass".into(),
                details: Some("ran cargo test".into()),
            }],
        );
        assert_eq!(out, "report (done): tests pass\n  ran cargo test\n");
    }

    #[test]
    fn json_is_one_item_per_line() {
        let (out, finished) = render(true, vec![text("hi"), done(StopReason::Interrupted)]);
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(lines[0]).unwrap(),
            json!({"seq": 1, "event": {"type": "text", "text": "hi"}})
        );
        assert_eq!(finished.unwrap().stop, StopReason::Interrupted);
    }

    #[test]
    fn long_tool_input_is_cut() {
        let long = "x".repeat(300);
        let (out, _) = render(
            false,
            vec![ExecEvent::Tool {
                id: "1".into(),
                call: ToolCall::new("t", json!(long), ActionClass::Other),
            }],
        );
        assert!(out.chars().count() < 130, "{out}");
        assert!(out.trim_end().ends_with('…'));
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(5), "5s");
        assert_eq!(duration(73), "1m 13s");
        assert_eq!(duration(3_700), "1h 01m");
    }

    #[test]
    fn status_names_are_the_wire_names() {
        assert_eq!(status_name(RunStatus::WaitingApproval), "waiting_approval");
        assert_eq!(wire_name(&StopReason::MaxTokens), "max_tokens");
    }
}
