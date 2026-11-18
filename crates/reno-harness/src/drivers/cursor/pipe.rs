//! The pipe to the shim: JSONL out on its stdin, JSONL in from its stdout.
//!
//! Both directions are bounded (ARCH-11). Reading stdout never stops while the run lives: a
//! reader that stops can stall a child that honours backpressure.

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::HarnessError;
use crate::wire::pi::framing::{MAX_FRAME, parse_frame};

/// Frames waiting to be written to the shim. A run sends a handful; a shim that stops reading
/// stdin fills this and the next send fails instead of growing without bound.
const OUT_CAPACITY: usize = 64;
/// Frames read from the shim and not yet consumed.
const IN_CAPACITY: usize = 256;

/// The sending half.
#[derive(Clone, Debug)]
pub(super) struct Client {
    writer: mpsc::Sender<Value>,
}

impl Client {
    /// Wait for room to queue one frame, so a burst of steering is held back, not dropped: the
    /// caller takes the next message only once it holds the permit.
    pub(super) async fn reserve(&self) -> Result<mpsc::Permit<'_, Value>, HarnessError> {
        self.writer
            .reserve()
            .await
            .map_err(|_| HarnessError::Protocol("the Cursor shim's stdin closed".into()))
    }

    /// Queue `frame`, waiting up to `within` for room: for the one frame that must not be lost
    /// to a full queue (the interrupt).
    pub(super) async fn send_within(
        &self,
        frame: Value,
        within: std::time::Duration,
    ) -> Result<(), HarnessError> {
        match tokio::time::timeout(within, self.writer.send(frame)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(HarnessError::Protocol(
                "the Cursor shim's stdin closed".into(),
            )),
            Err(_) => Err(HarnessError::Protocol(
                "the Cursor shim is not reading its stdin".into(),
            )),
        }
    }

    /// Queue `frame` for the shim.
    pub(super) fn send(&self, frame: Value) -> Result<(), HarnessError> {
        self.writer.try_send(frame).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => {
                HarnessError::Protocol("the Cursor shim is not reading its stdin".into())
            }
            mpsc::error::TrySendError::Closed(_) => {
                HarnessError::Protocol("the Cursor shim's stdin closed".into())
            }
        })
    }
}

/// The pipe: a [`Client`] to write with and the frames the shim sent.
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
    /// Close the shim's stdin: the request for an orderly shutdown (it cancels and closes the
    /// SDK on EOF). Frames not yet written are lost.
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
                // fill_buf bounds allocation before a noisy peer sends its newline.
                let chunk = match reader.fill_buf().await {
                    Ok([]) => {
                        let _ = tx
                            .send(Err(HarnessError::Protocol(
                                "the Cursor shim's stdout closed".into(),
                            )))
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
                        target: "reno_harness::cursor",
                        "a Cursor shim frame over {} MiB was dropped",
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
                            target: "reno_harness::cursor",
                            "an unparseable Cursor shim line was skipped"
                        ),
                    }
                }
                bytes.clear();
            }
        });
        Self {
            client: Client { writer: writer_tx },
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
    async fn frames_keep_their_order_across_fragmented_utf8_and_a_closed_pipe_is_an_error() {
        let (host, peer) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(host);
        let (peer_read, mut peer_write) = tokio::io::split(peer);
        let mut transport = Transport::new(write, read);
        transport.client.send(json!({"op":"interrupt"})).unwrap();
        let mut lines = BufReader::new(peer_read).lines();
        assert_eq!(
            serde_json::from_str::<Value>(&lines.next_line().await.unwrap().unwrap()).unwrap(),
            json!({"op":"interrupt"})
        );
        let frames = [
            json!({"ev":"text","text":"n\u{303} \u{2028} ok"}),
            json!({"ev":"turn","status":"finished"}),
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
        // JSON.stringify writes a lone surrogate as an escape serde_json refuses.
        peer.write_all(br#"{"ev":"text","text":"cut \ud83d here"}"#)
            .await
            .unwrap();
        peer.write_all(b"\n").await.unwrap();
        peer.write_all(format!(r#"{{"ev":"huge","text":"{}"#, "x".repeat(100)).as_bytes())
            .await
            .unwrap();
        peer.write_all(br#""}"#).await.unwrap();
        peer.write_all(b"\n{\"ev\":\"turn\"}\n").await.unwrap();
        assert_eq!(
            transport.incoming.recv().await.unwrap().unwrap(),
            json!({"ev":"text","text":"cut \u{fffd} here"})
        );
        assert_eq!(
            transport.incoming.recv().await.unwrap().unwrap(),
            json!({"ev":"turn"})
        );
    }

    #[tokio::test]
    async fn a_shim_that_stops_reading_fills_the_queue_and_the_next_send_fails() {
        let (host, _peer) = tokio::io::duplex(8);
        let (read, write) = tokio::io::split(host);
        let transport = Transport::new(write, read);
        let big = json!({"op":"steer","prompt":"x".repeat(4096)});
        let sent = (0..OUT_CAPACITY + 8).find(|_| transport.client.send(big.clone()).is_err());
        assert!(sent.is_some(), "the queue is bounded");
    }
}
