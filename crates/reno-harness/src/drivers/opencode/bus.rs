//! The event bus: `/global/event` (1.x) or `/api/event` (2.x), tailed into the run.
//!
//! The server is our own child on loopback, so a dropped stream with a live process is transient:
//! it is reconnected after a short pause, and past a budget the run learns the bus is gone (frames
//! missed mean the transcript can no longer be trusted). The bus has no replay, so a frame sent
//! while it was down is lost: the run re-lists pending requests and re-reads the session's status
//! when [`BusMsg::Connected`] comes again.
//!
//! A bus silent past `silence` (the server sends a heartbeat every 10 s on 1.x, every 15 s on 2.x)
//! is presumed wedged and reconnected, but only after the server is asked whether it is alive at
//! all: a stopped or hung process still accepts loopback connections (the kernel completes the
//! handshake), so each reconnect would otherwise wait out another silence, and the reconnect
//! budget runs out after half an hour of "working". Any answer proves it alive; none within `probe`
//! ends the run. A server that answers its health route but sends no frame for `wedged` is
//! presumed wedged as well.

use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::server::Api;
use crate::wire::opencode::sse::SseDecoder;
use crate::wire::opencode::v2;
use crate::wire::opencode::version::Protocol;

/// A stream that frames is a live server; this many attempts in a row that do not, and the bus is
/// given up.
const RECONNECT_DELAY: Duration = Duration::from_millis(250);
const RECONNECT_ATTEMPTS: u32 = 40;

/// What the bus hands the run.
#[derive(Debug)]
pub(super) enum BusMsg {
    /// A stream delivered its first frame. The first one gates the first prompt (the bus has no
    /// replay: a turn that fails fast can run busy, error, idle before a late subscription
    /// exists); later ones mean a gap that may have swallowed frames.
    Connected,
    /// A frame, in the 1.x vocabulary.
    Frame(Value),
    /// The stream is gone past the reconnect budget.
    Disconnected,
    /// The server is up but no longer serving. Carries the run's error.
    Unresponsive(String),
}

/// Bus liveness bounds; tests shrink them.
#[derive(Clone, Copy, Debug)]
pub(super) struct BusTimeouts {
    /// Longest the bus may stay silent before it is presumed wedged and reconnected.
    pub(super) silence: Duration,
    /// Longest the server may take to answer a health check on a silent bus.
    pub(super) probe: Duration,
    /// Longest the bus may go without one frame while the server still answers health checks.
    pub(super) wedged: Duration,
}

impl Default for BusTimeouts {
    fn default() -> Self {
        Self {
            silence: Duration::from_secs(45),
            probe: Duration::from_secs(5),
            wedged: Duration::from_secs(150),
        }
    }
}

/// Tail the bus into `tx` until it is closed, given up or the run is over.
pub(super) async fn run(api: Api, tx: mpsc::Sender<BusMsg>, timeouts: BusTimeouts) {
    let BusTimeouts {
        silence,
        probe,
        wedged,
    } = timeouts;
    // The server just answered its startup and session calls.
    let mut last_frame = Instant::now();
    // Attempts in a row that ended in silence, and that failed to frame at all.
    let mut silences: u32 = 0;
    let mut failures: u32 = 0;
    let mut normalizer = v2::Normalizer::default();
    loop {
        if tx.is_closed() {
            return;
        }
        let attempt = Instant::now();
        // A server that accepts the connection but never answers it must not park the bus.
        let connected = match api.bus_request() {
            Ok(request) => tokio::time::timeout(silence, request.send()).await,
            Err(_) => return,
        };
        if let Ok(Ok(response)) = connected
            && response.status().is_success()
        {
            let framed = stream(
                &api,
                &tx,
                response,
                silence,
                &mut last_frame,
                &mut normalizer,
            )
            .await;
            if tx.is_closed() {
                return;
            }
            // Only a stream that delivered a frame proves the server healthy: one accepted and
            // closed (or wedged) before its first frame keeps counting toward the budget.
            if framed {
                failures = 0;
            }
        }
        let quiet = last_frame.elapsed();
        if quiet < silence {
            silences = 0;
        } else {
            // A stream that framed, then fell silent, is the first silence.
            silences = if last_frame >= attempt {
                1
            } else {
                silences + 1
            };
            let unresponsive = if api.alive(probe).await {
                // Two silences waited out, so a clock that jumped across a machine sleep never
                // trips it.
                (silences >= 2 && quiet >= wedged).then(|| {
                    format!(
                        "opencode server stopped responding: no event for {}s",
                        quiet.as_secs()
                    )
                })
            } else {
                Some(format!(
                    "opencode server stopped responding: no event for {}s, and no answer to a \
                     health check within {}s",
                    quiet.as_secs(),
                    probe.as_secs_f32()
                ))
            };
            if let Some(message) = unresponsive {
                let _ = tx.send(BusMsg::Unresponsive(message)).await;
                return;
            }
        }
        failures += 1;
        if failures > RECONNECT_ATTEMPTS {
            let _ = tx.send(BusMsg::Disconnected).await;
            return;
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// Forward one stream's events; whether it delivered a frame. Stamps `last_frame` with each
/// frame, heartbeats included, and abandons a stream silent for `silence` so the caller
/// reconnects.
async fn stream(
    api: &Api,
    tx: &mpsc::Sender<BusMsg>,
    response: reqwest::Response,
    silence: Duration,
    last_frame: &mut Instant,
    normalizer: &mut v2::Normalizer,
) -> bool {
    let mut body = response.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut announced = false;
    loop {
        let bytes = match tokio::time::timeout(silence, body.next()).await {
            Ok(Some(Ok(bytes))) => bytes,
            Ok(Some(Err(_)) | None) => return announced,
            Err(_) => {
                tracing::warn!(
                    target: "reno_harness::opencode",
                    "event bus silent for {}s; reconnecting",
                    silence.as_secs()
                );
                return announced;
            }
        };
        let frames = decoder.frames;
        let events = decoder.feed(&bytes);
        // The first frame of a stream is the server's `server.connected`, sent to every new
        // subscriber: it proves a connection, not a server that is working, so it does not count
        // as sign of life (or a wedged server would renew itself with each reconnect).
        if decoder.frames > frames.max(1) {
            *last_frame = Instant::now();
        }
        if !announced && decoder.frames > 0 {
            announced = true;
            if tx.send(BusMsg::Connected).await.is_err() {
                return announced;
            }
        }
        for data in events {
            let Ok(event) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            let frames = if api.protocol == Protocol::V2 {
                let frames = normalizer.normalize(&event);
                if normalizer.leaking() {
                    let _ = tx.send(BusMsg::Disconnected).await;
                    return announced;
                }
                frames
            } else {
                vec![event]
            };
            for frame in frames {
                if tx.send(BusMsg::Frame(frame)).await.is_err() {
                    return announced;
                }
            }
        }
    }
}
