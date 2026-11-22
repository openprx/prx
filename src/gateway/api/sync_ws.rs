//! Bounded durable ACK window; no durable broadcast or lossy delivery queue.
use super::{ApiPrincipal, AppState, capabilities::coded_error};
use crate::memory::sync::{JournalPage, SyncStore};
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::{
    Extension,
    extract::{State, ws::WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use futures::SinkExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Arc, time::Duration};

pub(super) async fn ws(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    if !super::owner_authenticated(&state, &headers) {
        return coded_error(StatusCode::UNAUTHORIZED, "auth_failed", "a paired bearer is required");
    }
    let Some(store) = state.mem.sync_store() else {
        return coded_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "this backend has no transactional sync store",
        );
    };
    let Ok(upgrade) = upgrade else {
        return coded_error(
            StatusCode::BAD_REQUEST,
            "field_invalid",
            "a WebSocket upgrade is required",
        );
    };
    upgrade
        .max_message_size(4096)
        .max_frame_size(4096)
        .on_upgrade(move |socket| super::sync_ws::serve(socket, store, principal.owner_id))
}

const MAX_FRAME: usize = 1024 * 1024;
const WINDOW_EVENTS: usize = 256;
const WINDOW_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum Client {
    #[serde(rename = "hello")]
    Hello { proto: u32, cursor: Value },
    #[serde(rename = "ack")]
    Ack { seq: i64 },
    #[serde(rename = "pong")]
    Pong,
}
async fn close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        socket.send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        }))),
    )
    .await;
}
async fn finish_peer_close(socket: &mut WebSocket) {
    // Receiving Close queues the matching reply inside Tungstenite. Flush it
    // before dropping the upgraded transport, including before hello.
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.flush()).await;
}

async fn send(socket: &mut WebSocket, value: &Value) -> anyhow::Result<usize> {
    let text = serde_json::to_string(value)?;
    anyhow::ensure!(text.len() <= MAX_FRAME, crate::memory::sync::SyncError::InvalidSource);
    let len = text.len();
    tokio::time::timeout(Duration::from_secs(10), socket.send(Message::Text(text.into()))).await??;
    Ok(len)
}
async fn read(store: &Arc<dyn SyncStore>, owner: &str, after: i64, limit: usize) -> anyhow::Result<JournalPage> {
    let store = Arc::clone(store);
    let owner = owner.to_owned();
    crate::runtime::blocking::spawn_blocking(move || store.journal(&owner, after, limit)).await?
}
fn decode(message: Message) -> Result<Client, u16> {
    match message {
        Message::Text(text) if text.len() <= 4096 => serde_json::from_str(&text).map_err(|_| 1008),
        Message::Text(_) => Err(1009),
        _ => Err(1008),
    }
}

pub(super) async fn serve(mut socket: WebSocket, store: Arc<dyn SyncStore>, owner: String) {
    let connection = store.connect(&owner);
    let shutdown = store.shutdown_signal();
    let result = run(&mut socket, &store, &owner, &connection, &shutdown).await;
    if let Err((code, reason)) = result {
        close(&mut socket, code, reason).await;
    }
    store.disconnect(&owner, &connection);
}

async fn run(
    socket: &mut WebSocket,
    store: &Arc<dyn SyncStore>,
    owner: &str,
    connection: &tokio_util::sync::CancellationToken,
    shutdown: &tokio_util::sync::CancellationToken,
) -> Result<(), (u16, &'static str)> {
    let hello = tokio::select! {
        ()=connection.cancelled()=>return Err((4001,"superseded")),
        ()=shutdown.cancelled()=>return Err((1001,"shutdown")),
        hello=tokio::time::timeout(Duration::from_secs(10),socket.recv())=>hello,
    };
    let message = match hello {
        Ok(Some(Ok(Message::Close(_)))) => {
            finish_peer_close(socket).await;
            return Ok(());
        }
        Ok(Some(Ok(message))) => message,
        Ok(Some(Err(_))) => return Err((1009, "client frame too large")),
        _ => return Err((1008, "hello required")),
    };
    let (proto, cursor) = match decode(message).map_err(|code| (code, "invalid client frame"))? {
        Client::Hello { proto, cursor } => (proto, cursor),
        _ => return Err((1008, "hello required")),
    };
    if proto != 1 {
        return Err((1008, "unsupported protocol"));
    }
    let initial = read(store, owner, 0, 0)
        .await
        .map_err(|_| (1011, "journal read failed"))?;
    let epoch = initial.epoch;
    let mut after = if cursor.is_null() {
        initial.floor
    } else {
        let cursor = cursor.as_str().ok_or((1008, "invalid cursor"))?;
        let (generation, seq) = cursor.split_once(':').ok_or((1008, "invalid cursor"))?;
        let seq = seq.parse::<i64>().map_err(|_| (1008, "invalid cursor"))?;
        if generation != epoch || seq < initial.floor || seq > initial.head {
            send(socket, &json!({"type":"resync_required","epoch":epoch}))
                .await
                .map_err(|_| (1011, "send failed"))?;
            return Err((4410, "resync required"));
        }
        seq
    };
    send(socket, &json!({"type":"welcome","epoch":epoch,"head_seq":initial.head}))
        .await
        .map_err(|_| (1011, "send failed"))?;
    let mut acked = after;
    let mut window = VecDeque::<(i64, usize)>::new();
    let mut bytes = 0usize;
    let notify = store.notify();
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    tick.tick().await;
    let mut missed = 0u8;
    let mut dirty = true;
    let mut pending: Option<Value> = None;
    loop {
        if dirty && window.len() < WINDOW_EVENTS && bytes < WINDOW_BYTES && pending.is_none() {
            let mut page = read(store, owner, after, 1)
                .await
                .map_err(|_| (1011, "journal read failed"))?;
            if page.epoch != epoch || after < page.floor {
                send(socket, &json!({"type":"resync_required","epoch":page.epoch}))
                    .await
                    .map_err(|_| (1011, "send failed"))?;
                return Err((4410, "resync required"));
            }
            pending = page.events.pop();
            dirty = pending.is_some();
        }
        if let Some(event) = pending.as_ref() {
            let size = serde_json::to_vec(event)
                .map_err(|_| (1011, "event encode failed"))?
                .len();
            if window.len() < WINDOW_EVENTS && bytes.checked_add(size).is_some_and(|n| n <= WINDOW_BYTES) {
                let seq = event
                    .get("seq")
                    .and_then(Value::as_i64)
                    .ok_or((1011, "invalid journal sequence"))?;
                send(socket, event).await.map_err(|_| (1011, "send failed"))?;
                after = seq;
                bytes += size;
                window.push_back((seq, size));
                pending = None;
                // Still service ACKs, shutdown and liveness between sends.
                tokio::task::yield_now().await;
            }
        }
        let ready = dirty && pending.is_none() && window.len() < WINDOW_EVENTS && bytes < WINDOW_BYTES;
        tokio::select! {
            biased;
            ()=connection.cancelled()=>return Err((4001,"superseded")),
            ()=shutdown.cancelled()=>return Err((1001,"shutdown")),
            inbound=socket.recv()=>{
                let message=match inbound {
                    Some(Ok(Message::Close(_)))=>{finish_peer_close(socket).await;return Ok(());},
                    None=>return Ok(()),Some(Ok(message))=>message,Some(Err(_))=>return Err((1009,"client frame too large"))
                };
                match decode(message).map_err(|code|(code,"invalid client frame"))? {
                    Client::Hello{..}=>return Err((1008,"second hello")),
                    Client::Pong=>missed=0,
                    Client::Ack{seq}=>{
                        if seq<0||seq>after{return Err((1008,"invalid acknowledgement"));}
                        if seq>acked {acked=seq;while window.front().is_some_and(|(sent,_)|*sent<=seq){if let Some((_,size))=window.pop_front(){bytes=bytes.saturating_sub(size);}}dirty=true;}
                    }
                }
            }
            _=tick.tick()=>{
                if missed>=2{return Err((4408,"pong timeout"));}
                send(socket,&json!({"type":"ping"})).await.map_err(|_|(1011,"send failed"))?;missed+=1;dirty=true;
            }
            ()=notify.notified()=>dirty=true,
            ()=std::future::ready(()),if ready=>{}
        }
    }
}
