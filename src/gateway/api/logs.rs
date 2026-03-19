use super::AppState;
use super::extract_resource_auth_token;
use axum::{
    Json,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use tokio::sync::broadcast;

const MAX_MESSAGES_PER_SECOND: usize = 100;
const MAX_WS_CONNECTIONS: usize = 64;

pub async fn ws_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let provided_token = extract_resource_auth_token(&headers);
    if state.pairing.require_pairing() && !state.pairing.is_authenticated(&provided_token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Unauthorized"})),
        )
            .into_response();
    }

    // Connection-level limit: prevent resource exhaustion from too many open streams
    if state.logs_broadcast_tx.receiver_count() >= MAX_WS_CONNECTIONS {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Too many log stream connections"})),
        )
            .into_response();
    }

    let receiver = state.logs_broadcast_tx.subscribe();
    ws.on_upgrade(move |socket| stream_logs(socket, receiver))
}

async fn stream_logs(mut socket: WebSocket, mut receiver: broadcast::Receiver<String>) {
    let mut sent_in_window = 0usize;
    let mut window_started = tokio::time::Instant::now();

    loop {
        tokio::select! {
            recv_result = receiver.recv() => {
                let line = match recv_result {
                    Ok(line) => line,
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                };

                if window_started.elapsed() >= tokio::time::Duration::from_secs(1) {
                    window_started = tokio::time::Instant::now();
                    sent_in_window = 0;
                }

                if sent_in_window >= MAX_MESSAGES_PER_SECOND {
                    continue;
                }

                if socket.send(Message::Text(line.into())).await.is_err() {
                    break;
                }
                sent_in_window += 1;
            }
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}
