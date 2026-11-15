//! Measurements for `cargo xtask bench`. Not a test: `cargo test` only compiles it.
//!
//! - `store_bench append <events>`: appends `events` events to a fresh journal in a temp
//!   directory, each followed by a flush, and prints `{"samplesUs": [...]}`, the time of each
//!   append plus flush in microseconds. `flush` pushes the buffer to the file with `write(2)`; it
//!   does not `fsync`, which is what the daemon does too.
//! - `store_bench claim-latency <reno.db>`: for every run in a stopped daemon's database, the
//!   milliseconds between its `run.claimed` event and the `run.transitioned` event that moves it
//!   to `running`, which the dispatcher commits just before it starts the executor. Prints
//!   `{"deltasMs": [...]}`. Event timestamps are whole milliseconds.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::collections::HashMap;
use std::process::ExitCode;
use std::time::Instant;

use reno_proto::{ExecEvent, Seq};
use reno_store::Store;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["append", events] => events
            .parse()
            .map_err(|e| format!("bad event count: {e}"))
            .and_then(append),
        ["claim-latency", db] => claim_latency(db),
        _ => Err("usage: store_bench append <events> | claim-latency <reno.db>".to_owned()),
    };
    match result {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("store_bench: {message}");
            ExitCode::FAILURE
        }
    }
}

fn append(events: usize) -> Result<String, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let journals = reno_store::Journals::new(dir.path());
    let run = reno_proto::RunId::from_suffix("bench").map_err(|e| format!("run id: {e:?}"))?;
    let mut writer = journals.writer(&run).map_err(|e| e.to_string())?;
    let event = ExecEvent::Text {
        text: "the quick brown fox jumps over the lazy dog, again and again. ".to_owned(),
    };
    // Warm the file, the buffer and the serializer; those are not what a steady run pays.
    for _ in 0..200 {
        writer.append(&event).map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
    }
    let mut samples = Vec::with_capacity(events);
    for _ in 0..events {
        let start = Instant::now();
        writer.append(&event).map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
        samples.push(start.elapsed().as_secs_f64() * 1e6);
    }
    serde_json::to_string(&serde_json::json!({ "samplesUs": samples })).map_err(|e| e.to_string())
}

fn claim_latency(db: &str) -> Result<String, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let store = Store::open(db).map_err(|e| e.to_string())?;
        let mut claimed: HashMap<String, i64> = HashMap::new();
        let mut deltas: Vec<i64> = Vec::new();
        let mut after = Seq(0);
        loop {
            let batch = store
                .events_since(after, 5_000)
                .await
                .map_err(|e| e.to_string())?;
            let Some(last) = batch.last() else { break };
            after = last.seq;
            for event in &batch {
                match event.kind.as_str() {
                    "run.claimed" => {
                        claimed.insert(event.subject_id.clone(), event.ts);
                    }
                    "run.transitioned"
                        if event.data.get("to").and_then(|v| v.as_str()) == Some("running") =>
                    {
                        if let Some(at) = claimed.remove(&event.subject_id) {
                            deltas.push(event.ts - at);
                        }
                    }
                    _ => {}
                }
            }
        }
        serde_json::to_string(&serde_json::json!({ "deltasMs": deltas })).map_err(|e| e.to_string())
    })
}
