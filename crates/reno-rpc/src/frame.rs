//! JSON-RPC 2.0 framing: ndjson lines, and the private wire structs (ARCH-4).
//!
//! Batches are not supported. A line holding a JSON array is answered with
//! [`ErrorCode::InvalidRequest`] and a null id; the connection stays open.

use futures::SinkExt;
use reno_proto::{ErrorCode, ErrorObject, Seq, SubGapParams, SubscriptionId};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedWrite, LinesCodec};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// Longest accepted line: 8 MiB.
pub(crate) const MAX_LINE: usize = 8 * 1024 * 1024;
/// Capacity of every connection's outbound frame channel (both sides).
pub(crate) const OUT_CAP: usize = 64;

pub(crate) const SUB_ITEM: &str = "sub.item";
pub(crate) const SUB_DONE: &str = "sub.done";
pub(crate) const SUB_CANCEL: &str = "sub.cancel";
pub(crate) const SUB_GAP: &str = "sub.gap";

/// A line codec with the frame limit applied.
pub(crate) fn codec() -> LinesCodec {
    LinesCodec::new_with_max_length(MAX_LINE)
}

/// One decoded incoming frame.
#[derive(Debug)]
pub(crate) enum Inbound {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        outcome: Result<Value, ErrorObject>,
    },
}

/// A frame that could not be decoded: the id to answer with (null if unknown) and the error.
/// Boxed so the `Ok` path stays small.
pub(crate) type BadFrame = Box<(Value, ErrorObject)>;

/// Decodes one line.
pub(crate) fn parse_inbound(line: &str) -> Result<Inbound, BadFrame> {
    let value: Value = serde_json::from_str(line).map_err(|e| {
        Box::new((
            Value::Null,
            ErrorObject::new(ErrorCode::ParseError, format!("parse error: {e}")),
        ))
    })?;
    let mut obj = match value {
        Value::Object(obj) => obj,
        other => {
            let msg = if other.is_array() {
                "batch requests are not supported"
            } else {
                "a frame must be a JSON object"
            };
            return Err(Box::new((
                Value::Null,
                ErrorObject::new(ErrorCode::InvalidRequest, msg),
            )));
        }
    };
    let id = obj.remove("id");
    let reply_id = id.clone().unwrap_or(Value::Null);
    let invalid = |msg: &str| -> BadFrame {
        Box::new((
            reply_id.clone(),
            ErrorObject::new(ErrorCode::InvalidRequest, msg),
        ))
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(invalid("`jsonrpc` must be \"2.0\""));
    }
    if let Some(method) = obj.remove("method") {
        let Value::String(method) = method else {
            return Err(invalid("`method` must be a string"));
        };
        let params = match obj.remove("params") {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(p @ (Value::Object(_) | Value::Array(_))) => p,
            Some(_) => return Err(invalid("`params` must be an object or array")),
        };
        return Ok(match id {
            Some(id) => Inbound::Request { id, method, params },
            None => Inbound::Notification { method, params },
        });
    }
    let Some(id) = id else {
        return Err(invalid("frame has neither `method` nor `id`"));
    };
    if let Some(error) = obj.remove("error") {
        let error = serde_json::from_value::<ErrorObject>(error)
            .map_err(|_| invalid("malformed `error` object"))?;
        return Ok(Inbound::Response {
            id,
            outcome: Err(error),
        });
    }
    match obj.remove("result") {
        Some(result) => Ok(Inbound::Response {
            id,
            outcome: Ok(result),
        }),
        None => Err(invalid("response has neither `result` nor `error`")),
    }
}

#[derive(Serialize)]
struct RequestOut<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: &'a Value,
}

#[derive(Serialize)]
struct NotificationOut<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    params: &'a Value,
}

#[derive(Serialize)]
struct ResultOut<'a> {
    jsonrpc: &'static str,
    id: &'a Value,
    result: &'a Value,
}

#[derive(Serialize)]
struct ErrorOut<'a> {
    jsonrpc: &'static str,
    id: &'a Value,
    error: &'a ErrorObject,
}

/// Params of `sub.item`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubItemParams {
    pub(crate) subscription: SubscriptionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) seq: Option<Seq>,
    pub(crate) item: Value,
}

/// Params of `sub.done` and `sub.cancel`; result of a subscribing request.
#[derive(Serialize, Deserialize)]
pub(crate) struct SubRef {
    pub(crate) subscription: SubscriptionId,
}

fn to_line<T: Serialize>(frame: &T) -> String {
    // Serialising these structs cannot fail: every key is a string and `Value` is valid JSON.
    serde_json::to_string(frame).unwrap_or_else(|e| {
        tracing::error!(error = %e, "frame serialisation failed");
        String::new()
    })
}

pub(crate) fn to_value<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

pub(crate) fn request_line(id: u64, method: &str, params: &Value) -> String {
    to_line(&RequestOut {
        jsonrpc: "2.0",
        id,
        method,
        params,
    })
}

pub(crate) fn notification_line(method: &str, params: &Value) -> String {
    to_line(&NotificationOut {
        jsonrpc: "2.0",
        method,
        params,
    })
}

pub(crate) fn result_line(id: &Value, result: &Value) -> String {
    to_line(&ResultOut {
        jsonrpc: "2.0",
        id,
        result,
    })
}

pub(crate) fn error_line(id: &Value, error: &ErrorObject) -> String {
    to_line(&ErrorOut {
        jsonrpc: "2.0",
        id,
        error,
    })
}

pub(crate) fn sub_item_line(sub: &SubscriptionId, seq: Option<Seq>, item: Value) -> String {
    let params = SubItemParams {
        subscription: sub.clone(),
        seq,
        item,
    };
    notification_line(SUB_ITEM, &to_value(&params))
}

pub(crate) fn sub_done_line(sub: &SubscriptionId) -> String {
    let params = SubRef {
        subscription: sub.clone(),
    };
    notification_line(SUB_DONE, &to_value(&params))
}

pub(crate) fn sub_gap_line(sub: &SubscriptionId, resume_from: Seq) -> String {
    let params = SubGapParams {
        subscription: sub.clone(),
        resume_from,
    };
    notification_line(SUB_GAP, &to_value(&params))
}

/// Spawns the task that owns the write half: it writes each queued line and flushes. It ends
/// when every sender is dropped (after flushing what is queued), on an I/O error, or at once
/// when `stop` fires.
pub(crate) fn spawn_writer<W>(
    tracker: &TaskTracker,
    write: W,
    mut rx: mpsc::Receiver<String>,
    stop: CancellationToken,
) -> JoinHandle<()>
where
    W: AsyncWrite + Send + Unpin + 'static,
{
    tracker.spawn(async move {
        let mut sink = FramedWrite::new(write, codec());
        loop {
            let line = tokio::select! {
                biased;
                () = stop.cancelled() => break,
                line = rx.recv() => match line {
                    Some(line) => line,
                    None => break,
                },
            };
            if line.is_empty() {
                continue;
            }
            let sent = tokio::select! {
                () = stop.cancelled() => break,
                sent = sink.send(line) => sent,
            };
            if let Err(e) = sent {
                tracing::debug!(error = %e, "write failed; closing writer");
                break;
            }
        }
        // Dropping `rx` makes pending and future sends fail, which tells producers to stop.
        // Closing shuts the write half down, so the peer reads EOF.
        drop(rx);
        let _ = SinkExt::<String>::close(&mut sink).await;
    })
}
