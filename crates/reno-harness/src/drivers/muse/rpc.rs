//! The pipe to `muse serve`: JSON-RPC 2.0, one object per line each way.
//!
//! Both directions are bounded (ARCH-11). Reading stdout never stops while the run lives (a
//! reader that stops can stall the host). Every outbound request passes through [`Client::send`],
//! the one place a frame is written, which refuses a method that is not on the allowlist of
//! `wire::muse::frames`: Reno never writes a harness's configuration.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::HarnessError;
use crate::wire::muse::frames::{self, Inbound};

/// The host's own inbound frame limit (10 MiB).
const MAX_FRAME: usize = 10 * 1024 * 1024;
/// Frames waiting to be written to Muse. A run sends a handful; a host that stops reading stdin
/// fills this and the next send fails instead of growing without bound.
const OUT_CAPACITY: usize = 64;
/// Frames read from Muse and not yet consumed.
const IN_CAPACITY: usize = 512;

/// A UUIDv7: the 48-bit Unix-millisecond timestamp, version 7, then random bits. The form MSP
/// requires for every `commandId` and session id.
pub(super) fn command_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    let mut bytes = *uuid::Uuid::new_v4().as_bytes();
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    uuid::Uuid::from_bytes(bytes).to_string()
}

/// The sending half. Cloneable: the run and every open question's answer share it.
#[derive(Clone, Debug)]
pub(super) struct Client {
    writer: mpsc::Sender<Value>,
    next_id: Arc<AtomicU64>,
}

impl Client {
    fn send(&self, frame: Value) -> Result<(), HarnessError> {
        // The guard: whatever else a caller builds, only these methods leave.
        if let Some(method) = frame["method"].as_str() {
            let allowed = if frame.get("id").is_some() {
                frames::may_send(method)
            } else {
                frames::may_notify(method)
            };
            if !allowed {
                let why = if frames::WRITES_CONFIG.contains(&method) {
                    "it would write Muse's own configuration or credentials"
                } else {
                    "Reno does not send it"
                };
                return Err(HarnessError::Protocol(format!(
                    "refused to send Muse `{method}`: {why}"
                )));
            }
        }
        self.writer.try_send(frame).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => {
                HarnessError::Protocol("Muse is not reading its stdin".into())
            }
            mpsc::error::TrySendError::Closed(_) => {
                HarnessError::Protocol("Muse stdin closed".into())
            }
        })
    }

    /// Send `method`; a command gets a fresh `commandId` unless `params` already carry one (a
    /// turn's id is chosen before it is sent). Returns the id the response will carry.
    pub(super) fn request(&self, method: &str, mut params: Value) -> Result<u64, HarnessError> {
        if !frames::is_query(method) && params.get("commandId").is_none() {
            params["commandId"] = json!(command_id());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.send(frames::request(id, method, params))?;
        Ok(id)
    }

    /// Send the notification `method`.
    pub(super) fn notify(&self, method: &str) -> Result<(), HarnessError> {
        self.send(frames::notification(method))
    }

    /// Answer a request of the host's.
    pub(super) fn answer_host(&self, id: &Value, method: &str) -> Result<(), HarnessError> {
        let frame = if crate::wire::muse::known::ANSWERED_REQUESTS.contains(&method) {
            frames::reply(id, json!({}))
        } else {
            frames::reply_not_found(id, method)
        };
        self.send(frame)
    }
}

/// The pipe: a [`Client`] to write with and the frames Muse sent.
#[derive(Debug)]
pub(super) struct Transport {
    pub(super) client: Client,
    /// Frames in stdout order. `Err` once, when stdout closes or breaks.
    pub(super) incoming: mpsc::Receiver<Result<Inbound, HarnessError>>,
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
    /// Close Muse's stdin: its request for an orderly shutdown. Frames not yet written are lost.
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
                // fill_buf bounds allocation before a noisy peer sends LF.
                let chunk = match reader.fill_buf().await {
                    Ok([]) => {
                        let _ = tx
                            .send(Err(HarnessError::Protocol("Muse stdout closed".into())))
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
                        target: "reno_harness::muse",
                        "Muse frame over {} MiB dropped",
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
                    // A stray non-UTF-8 byte (raw tool output echoed into a frame) costs that
                    // character, never the run.
                    let text = String::from_utf8_lossy(&bytes);
                    match serde_json::from_str::<Value>(&text) {
                        Ok(frame) => match frames::decode(&frame) {
                            Ok(inbound) => {
                                if tx.send(Ok(inbound)).await.is_err() {
                                    break;
                                }
                            }
                            Err(why) => {
                                let _ = tx.send(Err(HarnessError::Protocol(why))).await;
                                break;
                            }
                        },
                        // Stray output is not a reason to end the run.
                        Err(_) => tracing::warn!(
                            target: "reno_harness::muse",
                            "unparseable Muse line skipped"
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
    use super::*;

    #[test]
    fn command_ids_are_uuid_v7() {
        let id = uuid::Uuid::parse_str(&command_id()).unwrap();
        assert_eq!(id.get_version_num(), 7);
        assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
        assert_ne!(command_id(), command_id());
    }

    #[tokio::test]
    async fn commands_carry_a_command_id_and_queries_do_not() {
        let (host, peer) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(host);
        let (peer_read, mut peer_write) = tokio::io::split(peer);
        let mut transport = Transport::new(write, read);
        let query = transport.client.request("model/list", json!({})).unwrap();
        let command = transport
            .client
            .request("turn/start", json!({"commandId": "turn-1"}))
            .unwrap();
        transport
            .client
            .request("turn/interrupt", json!({}))
            .unwrap();
        transport.client.notify("initialized").unwrap();
        let mut lines = BufReader::new(peer_read).lines();
        let mut next = async || -> Value {
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap()
        };
        let first = next().await;
        assert_eq!(first["id"], query);
        assert!(first["params"].get("commandId").is_none());
        let second = next().await;
        assert_eq!(second["id"], command);
        assert_eq!(second["params"]["commandId"], "turn-1");
        let third = next().await;
        assert!(third["params"]["commandId"].is_string());
        let fourth = next().await;
        assert_eq!(fourth["method"], "initialized");
        assert!(fourth.get("id").is_none());

        for frame in [
            json!({"jsonrpc":"2.0","id":query,"result":{"models":[]}}),
            json!({"jsonrpc":"2.0","id":command,"error":{"code":-32020,"message":"gone",
                "data":{"kind":"sessionNotFound"}}}),
            json!({"jsonrpc":"2.0","id":7,"method":"approval/request","params":{"approvalId":"a"}}),
            json!({"jsonrpc":"2.0","method":"item/delta","params":{"delta":"n"}}),
        ] {
            peer_write
                .write_all(format!("{frame}\r\n").as_bytes())
                .await
                .unwrap();
        }
        assert!(matches!(
            transport.incoming.recv().await.unwrap().unwrap(),
            Inbound::Response { id, result: Ok(_) } if id == query
        ));
        assert!(matches!(
            transport.incoming.recv().await.unwrap().unwrap(),
            Inbound::Response { result: Err(f), .. } if f.kind == "sessionNotFound"
        ));
        assert!(matches!(
            transport.incoming.recv().await.unwrap().unwrap(),
            Inbound::Request { method, .. } if method == "approval/request"
        ));
        assert!(matches!(
            transport.incoming.recv().await.unwrap().unwrap(),
            Inbound::Notification { method, .. } if method == "item/delta"
        ));
    }

    #[tokio::test]
    async fn a_method_off_the_allowlist_is_refused_at_the_writer_and_never_written() {
        let (host, peer) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(host);
        let (peer_read, _peer_write) = tokio::io::split(peer);
        let transport = Transport::new(write, read);
        for method in crate::wire::muse::frames::WRITES_CONFIG
            .iter()
            .chain(&["not/a/method", "host/anything"])
        {
            let refused = transport.client.request(method, json!({})).unwrap_err();
            assert!(refused.to_string().contains(method), "{refused}");
        }
        assert!(transport.client.notify("settings/changed").is_err());
        // The next frame on the wire is the first one that was allowed.
        transport.client.request("model/list", json!({})).unwrap();
        let mut lines = BufReader::new(peer_read).lines();
        let first: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first["method"], "model/list");
    }

    #[tokio::test]
    async fn an_oversized_frame_costs_that_frame_and_stray_output_costs_nothing() {
        let (host, peer) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(host);
        let (_peer_read, mut peer_write) = tokio::io::split(peer);
        let mut transport = Transport::bounded(write, read, 64);
        let big = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"x\",\"params\":\"{}\"}}\n",
            "a".repeat(200)
        );
        peer_write.write_all(big.as_bytes()).await.unwrap();
        peer_write.write_all(b"not json at all\n").await.unwrap();
        peer_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"turn/started\"}\n")
            .await
            .unwrap();
        assert!(matches!(
            transport.incoming.recv().await.unwrap().unwrap(),
            Inbound::Notification { method, .. } if method == "turn/started"
        ));
    }
}
