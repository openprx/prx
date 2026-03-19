use super::{AppState, extract_resource_auth_token};
use crate::providers::ChatMessage;
use axum::{
    Json,
    body::Body,
    extract::{FromRequest, Multipart, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path as StdPath, PathBuf};
use tokio::fs;

const DEFAULT_PAGE_LIMIT: usize = 50;
const MAX_PAGE_LIMIT: usize = 500;
const MAX_UPLOAD_FILES: usize = 10;
const MAX_UPLOAD_FILE_SIZE_BYTES: usize = 20 * 1024 * 1024;
const UPLOADS_DIR_NAME: &str = "uploads";

#[derive(Serialize)]
pub(super) struct SessionSummary {
    session_id: String,
    sender: String,
    channel: String,
    status: String,
    created_at: String,
    updated_at: String,
    message_count: u64,
    last_message_preview: String,
}

#[derive(Serialize)]
pub(super) struct SessionMessage {
    role: String,
    content: String,
    timestamp: String,
    message_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct SessionsQuery {
    limit: Option<usize>,
    offset: Option<usize>,
    channel: Option<String>,
    status: Option<String>,
    search: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct SessionMessagesQuery {
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Deserialize)]
pub(super) struct SendMessageRequest {
    message: String,
}

#[derive(Deserialize)]
pub(super) struct SessionMediaQuery {
    path: String,
}

#[derive(Serialize)]
pub(super) struct SendMessageResponse {
    status: String,
    reply: String,
}

fn matches_session_filters(
    session: &crate::memory::ConversationSessionSummary,
    status_filter: Option<&str>,
    search_filter: Option<&str>,
) -> bool {
    let status = derive_session_status(&session.last_message_preview, session.message_count);
    if let Some(filter) = status_filter {
        if filter != status {
            return false;
        }
    }

    if let Some(filter) = search_filter {
        let haystack = format!(
            "{}\n{}\n{}\n{}",
            session.session_key, session.sender, session.channel, session.last_message_preview
        )
        .to_ascii_lowercase();
        if !haystack.contains(filter) {
            return false;
        }
    }

    true
}

fn derive_session_status(last_message_preview: &str, message_count: u64) -> &'static str {
    if message_count == 0 {
        "empty"
    } else if last_message_preview.trim().is_empty() {
        "pending"
    } else {
        "active"
    }
}

fn normalize_limit(limit: Option<usize>) -> usize {
    match limit {
        Some(0) | None => DEFAULT_PAGE_LIMIT,
        Some(value) => value.min(MAX_PAGE_LIMIT),
    }
}

fn normalize_offset(offset: Option<usize>) -> usize {
    offset.unwrap_or(0)
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

fn sanitize_upload_filename(raw_name: &str) -> String {
    let file_name = StdPath::new(raw_name)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("upload.bin");

    let mut sanitized = file_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    if sanitized.is_empty() {
        sanitized.push_str("upload.bin");
    }
    if sanitized.len() > 120 {
        sanitized.truncate(120);
    }

    sanitized
}

fn has_extension(file_name: &str, candidates: &[&str]) -> bool {
    let lower = file_name.to_ascii_lowercase();
    candidates
        .iter()
        .any(|candidate| lower.ends_with(candidate.trim_start_matches('*')))
}

fn is_image_upload(content_type: &str, file_name: &str) -> bool {
    content_type.starts_with("image/")
        || has_extension(
            file_name,
            &[
                ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".svg", ".heic", ".heif",
            ],
        )
}

fn is_video_upload(content_type: &str, file_name: &str) -> bool {
    content_type.starts_with("video/")
        || has_extension(
            file_name,
            &[".mp4", ".webm", ".mov", ".m4v", ".avi", ".mkv", ".ogg"],
        )
}

fn media_content_type_for_path(path: &StdPath) -> &'static str {
    let lower = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match lower.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "heic" | "heif" => "image/heic",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mkv" => "video/x-matroska",
        "ogg" => "video/ogg",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain; charset=utf-8",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

async fn parse_json_message(state: &AppState, request: Request) -> Result<String, Response> {
    let Json(payload) = Json::<SendMessageRequest>::from_request(request, state)
        .await
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "Invalid JSON payload"))?;

    let message = payload.message.trim().to_string();
    if message.is_empty() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "message must not be empty",
        ));
    }

    Ok(message)
}

async fn parse_multipart_message(state: &AppState, request: Request) -> Result<String, Response> {
    let mut multipart = Multipart::from_request(request, state)
        .await
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "Invalid multipart payload"))?;

    let uploads_root = {
        let config = state.config.lock();
        config.workspace_dir.join(UPLOADS_DIR_NAME)
    };

    fs::create_dir_all(&uploads_root).await.map_err(|error| {
        tracing::error!("Failed to create uploads directory: {error}");
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to prepare uploads directory",
        )
    })?;

    let mut message = String::new();
    let mut attachments = Vec::new();
    let mut file_count = 0usize;

    while let Some(field) = multipart.next_field().await.map_err(|_| {
        json_error(
            StatusCode::BAD_REQUEST,
            "Failed to parse multipart form fields",
        )
    })? {
        let field_name = field.name().unwrap_or_default().to_string();
        match field_name.as_str() {
            "message" => {
                message = field.text().await.map_err(|_| {
                    json_error(
                        StatusCode::BAD_REQUEST,
                        "Failed to decode message text from multipart payload",
                    )
                })?;
            }
            "files" => {
                file_count += 1;
                if file_count > MAX_UPLOAD_FILES {
                    return Err(json_error(
                        StatusCode::BAD_REQUEST,
                        "Too many files (max 10)",
                    ));
                }

                let original_name = field.file_name().unwrap_or("upload.bin").to_string();
                let safe_name = sanitize_upload_filename(&original_name);
                let content_type = field.content_type().unwrap_or("").to_string();
                let bytes = field.bytes().await.map_err(|_| {
                    json_error(
                        StatusCode::BAD_REQUEST,
                        "Failed to read uploaded file content",
                    )
                })?;

                if bytes.len() > MAX_UPLOAD_FILE_SIZE_BYTES {
                    return Err(json_error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "File too large (max 20MB per file)",
                    ));
                }

                let stored_name = format!(
                    "{}_{}_{}",
                    Utc::now().timestamp_millis(),
                    file_count,
                    safe_name
                );
                let stored_path = uploads_root.join(stored_name);

                fs::write(&stored_path, &bytes).await.map_err(|error| {
                    tracing::error!("Failed to persist uploaded file: {error}");
                    json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to persist uploaded file",
                    )
                })?;

                if is_image_upload(&content_type, &safe_name) {
                    attachments.push(format!("[IMAGE:{}]", stored_path.display()));
                } else if is_video_upload(&content_type, &safe_name) {
                    attachments.push(format!("[VIDEO:{}]", stored_path.display()));
                } else {
                    attachments.push(format!("[FILE:{}]", stored_path.display()));
                }
            }
            _ => {}
        }
    }

    let message = message.trim();
    let combined = if attachments.is_empty() {
        message.to_string()
    } else if message.is_empty() {
        attachments.join("\n")
    } else {
        format!("{message}\n{}", attachments.join("\n"))
    };

    if combined.trim().is_empty() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "message must not be empty",
        ));
    }

    Ok(combined)
}

async fn parse_message_from_request(
    state: &AppState,
    request: Request,
) -> Result<String, Response> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    if content_type.contains("multipart/form-data") {
        parse_multipart_message(state, request).await
    } else {
        parse_json_message(state, request).await
    }
}

pub async fn get_sessions(
    State(state): State<AppState>,
    Query(query): Query<SessionsQuery>,
) -> impl IntoResponse {
    let limit = normalize_limit(query.limit);
    let offset = normalize_offset(query.offset);
    let status_filter = query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    let search_filter = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    let fetch_batch = if limit == 0 {
        DEFAULT_PAGE_LIMIT
    } else {
        limit.min(MAX_PAGE_LIMIT)
    };
    let mut response = Vec::with_capacity(limit);
    let mut source_offset = 0usize;
    let mut filtered_offset = 0usize;

    loop {
        let sessions = match state
            .mem
            .list_conversation_sessions(fetch_batch, source_offset, query.channel.as_deref())
            .await
        {
            Ok(sessions) => sessions,
            Err(error) => {
                tracing::error!("Failed to list conversation sessions from DB: {error}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "Failed to load sessions" })),
                )
                    .into_response();
            }
        };

        if sessions.is_empty() {
            break;
        }

        let batch_len = sessions.len();
        for session in sessions {
            if !matches_session_filters(
                &session,
                status_filter.as_deref(),
                search_filter.as_deref(),
            ) {
                continue;
            }

            if filtered_offset < offset {
                filtered_offset += 1;
                continue;
            }

            let status =
                derive_session_status(&session.last_message_preview, session.message_count);
            response.push(SessionSummary {
                session_id: session.session_key,
                sender: session.sender,
                channel: session.channel,
                status: status.to_string(),
                created_at: session.created_at,
                updated_at: session.updated_at,
                message_count: session.message_count,
                last_message_preview: session.last_message_preview,
            });

            if response.len() >= limit {
                break;
            }
        }

        if response.len() >= limit || batch_len < fetch_batch {
            break;
        }
        source_offset += batch_len;
    }

    Json(response).into_response()
}

pub async fn get_session_messages(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<SessionMessagesQuery>,
) -> impl IntoResponse {
    let exists = match state.mem.get_conversation_session(&session_id).await {
        Ok(session) => session.is_some(),
        Err(error) => {
            tracing::error!("Failed to load session metadata from DB: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to load session" })),
            )
                .into_response();
        }
    };
    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Session not found" })),
        )
            .into_response();
    }

    let limit = normalize_limit(query.limit);
    let offset = normalize_offset(query.offset);
    let turns = match state
        .mem
        .list_conversation_turns(&session_id, limit, offset)
        .await
    {
        Ok(turns) => turns,
        Err(error) => {
            tracing::error!("Failed to load conversation turns from DB: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to load session messages" })),
            )
                .into_response();
        }
    };

    let response: Vec<SessionMessage> = turns
        .into_iter()
        .map(|turn| SessionMessage {
            role: turn.role,
            content: turn.content,
            timestamp: turn.timestamp,
            message_id: turn.message_id,
        })
        .collect();

    Json(response).into_response()
}

pub async fn post_session_message(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    request: Request,
) -> impl IntoResponse {
    let message = match parse_message_from_request(&state, request).await {
        Ok(message) => message,
        Err(error_response) => return error_response,
    };

    let Some(session) = (match state.mem.get_conversation_session(&session_id).await {
        Ok(session) => session,
        Err(error) => {
            tracing::error!("Failed to read session metadata from DB: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to load session" })),
            )
                .into_response();
        }
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Session not found" })),
        )
            .into_response();
    };

    let turns = match state
        .mem
        .list_conversation_turns(&session_id, MAX_PAGE_LIMIT, 0)
        .await
    {
        Ok(turns) => turns,
        Err(error) => {
            tracing::error!("Failed to load conversation history from DB: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to load session history" })),
            )
                .into_response();
        }
    };

    let mut history_for_provider: Vec<ChatMessage> = turns
        .into_iter()
        .map(|turn| ChatMessage {
            role: turn.role,
            content: turn.content,
        })
        .collect();
    history_for_provider.push(ChatMessage::user(message.clone()));

    if let Err(error) = state
        .mem
        .append_conversation_turn(
            &session_id,
            &session.channel,
            &session.sender,
            "user",
            &message,
            None,
            None,
        )
        .await
    {
        tracing::error!("Failed to persist user session turn: {error}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to persist session message" })),
        )
            .into_response();
    }

    let reply = match state
        .provider
        .chat_with_history(&history_for_provider, &state.model, state.temperature)
        .await
    {
        Ok(reply) => reply,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": crate::providers::sanitize_api_error(&error.to_string())
                })),
            )
                .into_response();
        }
    };

    if let Err(error) = state
        .mem
        .append_conversation_turn(
            &session_id,
            &session.channel,
            &session.sender,
            "assistant",
            &reply,
            None,
            None,
        )
        .await
    {
        tracing::error!("Failed to persist assistant session turn: {error}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to persist assistant response" })),
        )
            .into_response();
    }

    (
        StatusCode::OK,
        Json(SendMessageResponse {
            status: "ok".to_string(),
            reply,
        }),
    )
        .into_response()
}

pub async fn get_session_media(
    State(state): State<AppState>,
    Query(query): Query<SessionMediaQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let token = extract_resource_auth_token(&headers);
    if state.pairing.require_pairing() && !state.pairing.is_authenticated(&token) {
        return json_error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }

    let requested_path = query.path.trim();
    if requested_path.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "path is required");
    }

    let uploads_root = {
        let config = state.config.lock();
        config.workspace_dir.join(UPLOADS_DIR_NAME)
    };

    let uploads_root = match fs::canonicalize(&uploads_root).await {
        Ok(path) => path,
        Err(_) => return json_error(StatusCode::NOT_FOUND, "Upload not found"),
    };

    let raw_path = PathBuf::from(requested_path);
    if raw_path.is_absolute() {
        return json_error(StatusCode::BAD_REQUEST, "Absolute paths are not allowed");
    }
    if raw_path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return json_error(StatusCode::BAD_REQUEST, "Path traversal is not allowed");
    }
    let candidate_path = uploads_root.join(raw_path);

    let resolved_path = match fs::canonicalize(&candidate_path).await {
        Ok(path) => path,
        Err(_) => return json_error(StatusCode::NOT_FOUND, "Upload not found"),
    };

    if !resolved_path.starts_with(&uploads_root) {
        return json_error(StatusCode::FORBIDDEN, "Access denied");
    }

    let bytes = match fs::read(&resolved_path).await {
        Ok(bytes) => bytes,
        Err(_) => return json_error(StatusCode::NOT_FOUND, "Upload not found"),
    };

    let content_type = media_content_type_for_path(&resolved_path);
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
