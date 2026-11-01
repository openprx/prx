//! Bidirectional JSON-RPC correlation.
//!
//! Outgoing request ids are `prx-N` so they do not collide with agent-host ids.
//! `reply.*` frames stay in the replay buffer until the host acks them.
//! Inbound `idempotency_key` values are remembered for ten minutes.

use super::protocol::{
    self, Envelope, ProtocolError, RpcErrorBody, RpcId, encode_bridge_call, is_reply_method, parse_envelope,
};
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const DEDUPE_LIMIT: usize = 256;
const REPLAY_LIMIT: usize = 128;
const DEDUPE_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, thiserror::Error)]
pub enum RpcFailure {
    #[error("bridge connection lost")]
    ConnectionLost,
    #[error("bridge request timed out")]
    Timeout,
    #[error("bridge peer returned {code}: {message}")]
    Remote { code: i64, message: String },
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

struct Pending {
    method: String,
    body: Vec<u8>,
    waiter: Option<oneshot::Sender<Result<Value, RpcFailure>>>,
}

struct DedupeEntry {
    key: String,
    at: Instant,
    result: Value,
}

/// Correlates in-flight requests and remembers idempotent inbound results.
pub struct RpcHub {
    next_id: AtomicU64,
    writer: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    pending: Mutex<HashMap<String, Pending>>,
    replay: Mutex<VecDeque<String>>,
    dedupe: Mutex<VecDeque<DedupeEntry>>,
}

impl RpcHub {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            writer: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            replay: Mutex::new(VecDeque::new()),
            dedupe: Mutex::new(VecDeque::new()),
        }
    }

    pub fn set_writer(&self, writer: mpsc::Sender<Vec<u8>>) {
        *self.writer.lock() = Some(writer);
    }

    /// Drop the writer. Non-reply waiters fail. Unacked `reply.*` stay queued.
    pub fn clear_writer(&self) {
        *self.writer.lock() = None;
        let mut pending = self.pending.lock();
        let mut replay = self.replay.lock();
        pending.retain(|id, entry| {
            if is_reply_method(&entry.method) {
                if let Some(waiter) = entry.waiter.take() {
                    let _ = waiter.send(Err(RpcFailure::ConnectionLost));
                }
                if !replay.iter().any(|queued| queued == id) {
                    push_replay(&mut replay, id.clone());
                }
                true
            } else {
                if let Some(waiter) = entry.waiter.take() {
                    let _ = waiter.send(Err(RpcFailure::ConnectionLost));
                }
                false
            }
        });
    }

    /// Send a request and wait for its response.
    ///
    /// # Errors
    ///
    /// Returns [`RpcFailure`] on timeout, disconnect, or a remote error.
    /// Unacked `reply.*` remain replayable after a timeout or disconnect.
    pub async fn request(&self, method: &str, params: &Value, timeout: Duration) -> Result<Value, RpcFailure> {
        let number = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = RpcId::String(format!("prx-{number}"));
        let key = id.to_string();
        let params = reply_params_with_key(method, params, &key);
        let body = encode_bridge_call(&id, method, &params)?;
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock();
            pending.insert(
                key.clone(),
                Pending {
                    method: method.to_string(),
                    body: body.clone(),
                    waiter: Some(tx),
                },
            );
            if is_reply_method(method) {
                push_replay(&mut self.replay.lock(), key.clone());
            }
        }
        if let Err(error) = self.emit(body).await {
            self.fail_emit(&key, method);
            return Err(error);
        }
        tokio::select! {
            biased;
            received = rx => received.unwrap_or(Err(RpcFailure::ConnectionLost)),
            () = tokio::time::sleep(timeout) => {
                self.detach_waiter(&key, method);
                Err(RpcFailure::Timeout)
            }
        }
    }

    /// Send a response frame. Failures are logged by the caller.
    ///
    /// # Errors
    ///
    /// Returns [`RpcFailure`] when the frame cannot be written.
    pub async fn respond(&self, id: &RpcId, result: &Value) -> Result<(), RpcFailure> {
        let body = protocol::encode_result(id, result)?;
        self.emit(body).await
    }

    /// Send an error response.
    ///
    /// # Errors
    ///
    /// Returns [`RpcFailure`] when the frame cannot be written.
    pub async fn respond_error(&self, id: &RpcId, error: &RpcErrorBody) -> Result<(), RpcFailure> {
        let body = protocol::encode_error(id, error)?;
        self.emit(body).await
    }

    /// Complete an outgoing request when its response arrives.
    pub fn complete(&self, id: &RpcId, body: Result<Value, RpcErrorBody>) {
        let key = id.to_string();
        let entry = self.pending.lock().remove(&key);
        self.replay.lock().retain(|queued| queued != &key);
        let Some(entry) = entry else {
            return;
        };
        let result = match body {
            Ok(value) => Ok(value),
            Err(error) => Err(RpcFailure::Remote {
                code: error.code,
                message: error.message,
            }),
        };
        if let Some(waiter) = entry.waiter {
            let _ = waiter.send(result);
        }
    }

    /// Resend unacknowledged `reply.*` frames after a new hello.
    pub async fn flush_replay(&self) {
        let bodies: Vec<Vec<u8>> = {
            let pending = self.pending.lock();
            let replay = self.replay.lock();
            replay
                .iter()
                .filter_map(|id| pending.get(id).map(|entry| entry.body.clone()))
                .collect()
        };
        for body in bodies {
            if self.emit(body).await.is_err() {
                break;
            }
        }
    }

    #[must_use]
    pub fn cached(&self, key: &str) -> Option<Value> {
        let mut queue = self.dedupe.lock();
        let now = Instant::now();
        queue.retain(|entry| now.saturating_duration_since(entry.at) < DEDUPE_TTL);
        queue
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.result.clone())
    }

    pub fn remember(&self, key: String, result: Value) {
        let mut queue = self.dedupe.lock();
        queue.push_back(DedupeEntry {
            key,
            at: Instant::now(),
            result,
        });
        while queue.len() > DEDUPE_LIMIT {
            queue.pop_front();
        }
    }

    /// Classify a raw frame as a peer response or a peer request.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError`] when the frame is not JSON-RPC 2.0.
    pub fn parse(&self, bytes: &[u8]) -> Result<Envelope, ProtocolError> {
        parse_envelope(bytes)
    }

    /// Send a parse or transport error whose request id is null.
    pub async fn emit_null_error(&self, code: i64, message: &str) {
        let body = protocol::encode_null_error(code, message);
        if !body.is_empty() {
            let _ = self.emit(body).await;
        }
    }

    async fn emit(&self, body: Vec<u8>) -> Result<(), RpcFailure> {
        let writer = self.writer.lock().clone();
        let Some(writer) = writer else {
            return Err(RpcFailure::ConnectionLost);
        };
        writer.send(body).await.map_err(|_| RpcFailure::ConnectionLost)
    }

    fn fail_emit(&self, key: &str, method: &str) {
        if is_reply_method(method) {
            if let Some(entry) = self.pending.lock().get_mut(key) {
                if let Some(waiter) = entry.waiter.take() {
                    let _ = waiter.send(Err(RpcFailure::ConnectionLost));
                }
            }
            return;
        }
        let removed = self.pending.lock().remove(key);
        if let Some(entry) = removed {
            if let Some(waiter) = entry.waiter {
                let _ = waiter.send(Err(RpcFailure::ConnectionLost));
            }
        }
    }

    fn detach_waiter(&self, key: &str, method: &str) {
        let mut pending = self.pending.lock();
        if is_reply_method(method) {
            if let Some(entry) = pending.get_mut(key) {
                entry.waiter = None;
            }
            return;
        }
        pending.remove(key);
    }
}

fn reply_params_with_key(method: &str, params: &Value, key: &str) -> Value {
    if !is_reply_method(method) {
        return params.clone();
    }
    let mut owned = params.clone();
    if let Some(object) = owned.as_object_mut() {
        object
            .entry("idempotency_key")
            .or_insert_with(|| Value::String(key.to_string()));
    }
    owned
}

fn push_replay(replay: &mut VecDeque<String>, id: String) {
    if replay.iter().any(|queued| queued == &id) {
        return;
    }
    replay.push_back(id);
    while replay.len() > REPLAY_LIMIT {
        replay.pop_front();
    }
}
