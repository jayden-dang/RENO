//! Production-settings survey: real multi-tool turns must settle exactly once with no orphaned
//! output. Silent gaps are measured for diagnostics; silence is never proof that an ACP prompt
//! has completed.
//!
//! SURVEY_RUNS=3 cargo test -p reno-harness --test real_quiet_survey -- --ignored --nocapture
//! Uninstalled/unauthenticated agents are skipped. This is the only test that runs the real agent
//! CLIs, and only when asked to.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::StreamExt;
use reno_harness::AcpExecutor;
use reno_harness::exec::{Decision, ExecControls, Executor};
use reno_proto::{AccessMode, ExecEvent, ExecSpec, RunId, SandboxLevel, StopReason};

const POST_DONE_WINDOW: Duration = Duration::from_secs(20);

struct ProbeOutcome {
    dones: Vec<(StopReason, Option<String>)>,
    after_first_done: usize,
    finished: bool,
    max_gap: Duration,
    started_err: Option<String>,
}

async fn probe_once(executor: AcpExecutor) -> ProbeOutcome {
    let (controls, handle) = ExecControls::new(8);
    let spec = ExecSpec {
        run_id: RunId::from_suffix("01HZX9").unwrap(),
        prompt: "Use your shell tool to run `echo probe-one`. After you see its output, \
                 run `echo probe-two` as a second separate command. After that, reply \
                 with exactly the word PROBE-DONE."
            .into(),
        cwd: std::env::temp_dir(),
        model: None,
        effort: None,
        access: AccessMode::FullAccess,
        sandbox: SandboxLevel::Unrestricted,
        resume: None,
        mcp: Vec::new(),
        env: Default::default(),
        output_schema: None,
        instructions: None,
    };
    let mut stream = match executor.start(spec, controls).await {
        Ok(stream) => stream,
        Err(error) => {
            return ProbeOutcome {
                dones: Vec::new(),
                after_first_done: 0,
                finished: false,
                max_gap: Duration::ZERO,
                started_err: Some(error.to_string()),
            };
        }
    };
    let started = std::time::Instant::now();
    let mut events: Vec<(Duration, ExecEvent)> = Vec::new();
    let mut first_done_at: Option<std::time::Instant> = None;
    loop {
        let budget = match first_done_at {
            Some(at) => POST_DONE_WINDOW.saturating_sub(at.elapsed()),
            None => Duration::from_secs(120),
        };
        match tokio::time::timeout(budget, stream.next()).await {
            Ok(Some(event)) => {
                match &event {
                    ExecEvent::Done { .. } if first_done_at.is_none() => {
                        first_done_at = Some(std::time::Instant::now());
                    }
                    ExecEvent::NeedsApproval { request_id, .. } => {
                        let _ = handle.decide(request_id.clone(), Decision::Allow).await;
                    }
                    _ => {}
                }
                events.push((started.elapsed(), event));
            }
            Ok(None) | Err(_) => break,
        }
        if events.len() > 600 {
            break;
        }
    }
    let first_done_idx = events
        .iter()
        .position(|(_, e)| matches!(e, ExecEvent::Done { .. }));
    // Max silent gap while the turn is live: consecutive-event gaps from the first event through
    // the first Done (post-Done observation excluded).
    let live_end = first_done_idx.unwrap_or(events.len().saturating_sub(1));
    let max_gap = events[..=live_end.min(events.len().saturating_sub(1))]
        .windows(2)
        .map(|w| w[1].0 - w[0].0)
        .max()
        .unwrap_or(Duration::ZERO);
    let after_first_done = first_done_idx.map_or(0, |i| {
        events[i + 1..]
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e,
                    ExecEvent::Text { .. } | ExecEvent::Tool { .. } | ExecEvent::ToolResult { .. }
                )
            })
            .count()
    });
    let text: String = events
        .iter()
        .filter_map(|(_, e)| match e {
            ExecEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    ProbeOutcome {
        dones: events
            .iter()
            .filter_map(|(_, e)| match e {
                ExecEvent::Done {
                    stop, diagnostic, ..
                } => Some((*stop, diagnostic.clone())),
                _ => None,
            })
            .collect(),
        after_first_done,
        finished: text.contains("PROBE-DONE"),
        max_gap,
        started_err: None,
    }
}

fn is_auth_or_missing(o: &ProbeOutcome) -> bool {
    let msg = o
        .started_err
        .clone()
        .or_else(|| o.dones.iter().find_map(|(_, e)| e.clone()))
        .unwrap_or_default()
        .to_lowercase();
    msg.contains("auth")
        || msg.contains("login")
        || msg.contains("signed in")
        || msg.contains("not installed")
        || msg.contains("not found")
        || msg.contains("no such file")
}

#[tokio::test]
#[ignore = "runs every installed+authenticated agent CLI; costs a few small prompts each"]
async fn real_all_agents_quiet_survey() {
    let runs: usize = std::env::var("SURVEY_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    type Make = fn() -> AcpExecutor;
    let agents: Vec<(&str, Make)> = vec![
        ("devin", AcpExecutor::devin),
        ("grok", AcpExecutor::grok),
        ("hermes", AcpExecutor::hermes),
    ];
    let mut failures: Vec<String> = Vec::new();
    for (name, ctor) in agents {
        for i in 1..=runs {
            let o = probe_once(ctor()).await;
            if is_auth_or_missing(&o) {
                println!("[{name}] SKIP: not installed / not authenticated");
                break;
            }
            let clean = o.dones.len() == 1
                && o.dones[0].0 == StopReason::Completed
                && o.after_first_done == 0
                && o.finished;
            println!(
                "[{name}] run {i}/{runs}: dones={:?} after_first_done={} finished={} \
                 max_live_gap={:.3}s -> {}",
                o.dones
                    .iter()
                    .map(|(s, _)| format!("{s:?}"))
                    .collect::<Vec<_>>(),
                o.after_first_done,
                o.finished,
                o.max_gap.as_secs_f64(),
                if clean { "CLEAN" } else { "VIOLATION" }
            );
            if !clean {
                failures.push(format!(
                    "[{name}] run {i}: dones={:?} after={} finished={}",
                    o.dones, o.after_first_done, o.finished
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
