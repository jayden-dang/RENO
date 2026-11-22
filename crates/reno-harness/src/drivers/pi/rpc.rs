//! The pipe to `pi --mode rpc`: JSONL out on stdin, JSONL in from stdout.
//!
//! Pi's RPC is not JSON-RPC 2.0. Replies and session events share one ordered stdout channel,
//! which is what the driver relies on to end a run without a timer: a reply to a command that was
//! sent after an event arrives after it.
//!
//! Both directions are bounded (ARCH-11). Reading stdout never stops while the run lives (Pi
//! honours backpressure, and a reader that stops can stall it).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::HarnessError;
use crate::wire::pi::frames::{string, writes_pi_settings};
use crate::wire::pi::framing::{MAX_FRAME, parse_frame};

/// Frames waiting to be written to Pi. A run sends a handful; a Pi that stops reading stdin
/// fills this and the next send fails instead of growing without bound.
const OUT_CAPACITY: usize = 64;
/// Frames read from Pi and not yet consumed.
const IN_CAPACITY: usize = 256;

/// Pi's RPC command that runs a shell command for the host, outside the agent's tool loop.
const HOST_BASH: &str = "bash";

/// The sending half. Cloneable: the run and every open dialog's answer share it.
#[derive(Clone, Debug)]
pub(super) struct Client {
    writer: mpsc::Sender<Value>,
    next_id: Arc<AtomicU64>,
    /// Pi's host `bash` command runs a command without the `tool_call` hook the gate sits on.
    refuse_host_bash: bool,
}

impl Client {
    /// This client, and every clone of it made afterwards, refuses Pi's host RPC `bash`.
    pub(super) fn refusing_host_bash(mut self) -> Client {
        self.refuse_host_bash = true;
        self
    }

    /// Queue `frame` for Pi.
    ///
    /// A command that would make Pi write its own settings (`wire::pi::frames::writes_pi_settings`)
    /// is refused here, at the one place every frame passes: Reno never writes a harness's
    /// configuration.
    pub(super) fn send(&self, frame: Value) -> Result<(), HarnessError> {
        let kind = string(&frame, "type");
        if writes_pi_settings(kind) {
            return Err(HarnessError::Protocol(format!(
                "refused to send Pi `{kind}`: it would write Pi's own settings"
            )));
        }
        if self.refuse_host_bash && kind == HOST_BASH {
            return Err(HarnessError::Protocol(format!(
                "refused to send Pi `{kind}`: it runs a command past Reno's gate"
            )));
        }
        self.writer.try_send(frame).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => {
                HarnessError::Protocol("Pi is not reading its stdin".into())
            }
            mpsc::error::TrySendError::Closed(_) => {
                HarnessError::Protocol("Pi stdin closed".into())
            }
        })
    }

    /// Queue `frame` with a fresh `id`, and return it: the reply carries it back.
    pub(super) fn request(&self, mut frame: Value) -> Result<String, HarnessError> {
        let id = format!("reno-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        frame["id"] = Value::String(id.clone());
        self.send(frame)?;
        Ok(id)
    }
}

/// The pipe: a [`Client`] to write with and the frames Pi sent.
#[derive(Debug)]
pub(super) struct Transport {
    pub(super) client: Client,
    /// Frames in stdout order. `Err` once, when stdout closes or breaks.
    pub(super) incoming: mpsc::Receiver<Result<Value, HarnessError>>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

impl Transport {
    /// Close Pi's stdin: its request for an orderly shutdown. Frames not yet written are lost.
    pub(super) fn close_input(&self) {
        self.writer.abort();
    }

    pub(super) fn new(
        input: impl AsyncWrite + Unpin + Send + 'static,
        output: impl AsyncRead + Unpin + Send + 'static,
    ) -> Self {
        Self::bounded(input, output, MAX_FRAME)
    }

    fn bounded(
        input: impl AsyncWrite + Unpin + Send + 'static,
        output: impl AsyncRead + Unpin + Send + 'static,
        max_frame: usize,
    ) -> Self {
        let (writer_tx, mut writer_rx) = mpsc::channel::<Value>(OUT_CAPACITY);
        let (tx, incoming) = mpsc::channel(IN_CAPACITY);
        let errors = tx.clone();
        let writer = tokio::spawn(async move {
            let mut input = input;
            while let Some(frame) = writer_rx.recv().await {
                let Ok(mut bytes) = serde_json::to_vec(&frame) else {
                    continue;
                };
                bytes.push(b'\n');
                let result = async {
                    input.write_all(&bytes).await?;
                    input.flush().await
                }
                .await;
                if let Err(e) = result {
                    let _ = errors.send(Err(e.into())).await;
                    break;
                }
            }
        });
        let reader = tokio::spawn(async move {
            let mut reader = BufReader::new(output);
            let mut bytes = Vec::new();
            // Inside an oversized frame: discard through its newline.
            let mut skipping = false;
            loop {
                // fill_buf bounds allocation before a hostile or noisy peer sends LF.
                let chunk = match reader.fill_buf().await {
                    Ok([]) => {
                        let _ = tx
                            .send(Err(HarnessError::Protocol("Pi stdout closed".into())))
                            .await;
                        break;
                    }
                    Ok(chunk) => chunk,
                    Err(e) => {
                        let _ = tx.send(Err(e.into())).await;
                        break;
                    }
                };
                let count = chunk
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(chunk.len(), |i| i + 1);
                let complete = chunk[count - 1] == b'\n';
                if skipping {
                    reader.consume(count);
                    skipping = !complete;
                    continue;
                }
                // One oversized frame costs that frame, not the whole run.
                if bytes.len() + count > max_frame {
                    tracing::warn!(
                        target: "reno_harness::pi",
                        "Pi RPC frame over {} MiB dropped",
                        max_frame / (1024 * 1024)
                    );
                    bytes.clear();
                    reader.consume(count);
                    skipping = !complete;
                    continue;
                }
                bytes.extend_from_slice(&chunk[..count]);
                reader.consume(count);
                if !complete {
                    continue;
                }
                if !bytes.iter().all(u8::is_ascii_whitespace) {
                    match parse_frame(&bytes) {
                        Some(frame) => {
                            if tx.send(Ok(frame)).await.is_err() {
                                break;
                            }
                        }
                        // Stray output (a dependency's console.log) is not a reason to end the
                        // run.
                        None => tracing::warn!(
                            target: "reno_harness::pi",
                            "unparseable Pi RPC line skipped"
                        ),
                    }
                }
                bytes.clear();
            }
        });
        Self {
            client: Client {
                writer: writer_tx,
                next_id: Arc::new(AtomicU64::new(1)),
                refuse_host_bash: false,
            },
            incoming,
            reader,
            writer,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn fragmented_utf8_and_responses_preserve_wire_order() {
        let (host, peer) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(host);
        let (peer_read, mut peer_write) = tokio::io::split(peer);
        let mut transport = Transport::new(write, read);
        let id = transport
            .client
            .request(json!({"type":"get_state"}))
            .unwrap();
        let mut lines = BufReader::new(peer_read).lines();
        let request: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(request["id"], id);
        assert!(request.get("jsonrpc").is_none());
        let frames = [
            json!({"type":"message_update", "text":"ñ\u{2028}ok"}),
            json!({"type":"agent_settled"}),
            json!({"type":"response","id":id,"success":true}),
        ];
        for frame in &frames {
            for byte in format!("{frame}\r\n").bytes() {
                peer_write.write_all(&[byte]).await.unwrap();
            }
        }
        for expected in frames {
            assert_eq!(transport.incoming.recv().await.unwrap().unwrap(), expected);
        }
        peer_write.shutdown().await.unwrap();
        drop(peer_write);
        assert!(transport.incoming.recv().await.unwrap().is_err());
    }

    #[tokio::test]
    async fn noise_lone_surrogates_and_oversized_frames_do_not_end_the_stream() {
        let (host, mut peer) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(host);
        let mut transport = Transport::bounded(write, read, 64);
        peer.write_all(b"not json\n").await.unwrap();
        peer.write_all(br#"{"type":"message_update","text":"cut \ud83d here"}"#)
            .await
            .unwrap();
        peer.write_all(b"\n").await.unwrap();
        // Over the 64-byte bound, arriving in pieces before its newline.
        peer.write_all(format!(r#"{{"type":"huge","text":"{}"#, "x".repeat(100)).as_bytes())
            .await
            .unwrap();
        peer.write_all(br#""}"#).await.unwrap();
        peer.write_all(b"\n{\"type\":\"agent_settled\"}\n")
            .await
            .unwrap();
        let first = transport.incoming.recv().await.unwrap().unwrap();
        assert_eq!(
            first,
            json!({"type":"message_update","text":"cut \u{fffd} here"})
        );
        let next = transport.incoming.recv().await.unwrap().unwrap();
        assert_eq!(next, json!({"type":"agent_settled"}));
    }

    #[tokio::test]
    async fn a_command_that_writes_pis_settings_is_refused_before_the_wire() {
        let (host, _peer) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(host);
        let transport = Transport::new(write, read);
        for kind in ["set_steering_mode", "set_auto_compaction", "cycle_model"] {
            let error = transport
                .client
                .request(json!({ "type": kind, "mode": "all" }))
                .unwrap_err();
            assert!(
                error.to_string().contains("write Pi's own settings"),
                "{kind}"
            );
        }
        assert!(
            transport
                .client
                .request(json!({"type":"get_state"}))
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_client_that_refuses_host_bash_refuses_it_and_still_the_settings_writers() {
        let (host, mut peer) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(host);
        let transport = Transport::new(write, read);
        let owner = transport.client.clone();
        let reno = transport.client.clone().refusing_host_bash();
        let bash = json!({"type":"bash","command":"echo hi"});
        let error = reno.send(bash.clone()).unwrap_err();
        assert!(error.to_string().contains("`bash`"), "{error}");
        assert!(owner.send(bash).is_ok(), "an unflagged client is unchanged");
        for client in [&owner, &reno] {
            assert!(
                client
                    .request(json!({"type":"set_auto_compaction","enabled":false}))
                    .unwrap_err()
                    .to_string()
                    .contains("write Pi's own settings")
            );
        }
        assert!(reno.request(json!({"type":"get_state"})).is_ok());
        let mut lines = BufReader::new(&mut peer).lines();
        let first: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first["type"], "bash");
        let second: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(second["type"], "get_state");
    }
}
