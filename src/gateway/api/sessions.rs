use super::{AppState, extract_resource_auth_token};
use crate::agent::loop_::{
    DocumentIngestRuntime, ScopeContext, ToolConcurrencyGovernanceConfig, build_context_with_shared_events_and_scope,
    build_runtime_system_prompt, run_tool_call_loop_traced, select_prompt_skills,
};
use crate::memory::MemoryFabric;
use crate::observability::NoopObserver;
use crate::providers::ChatMessage;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::security::policy::ResourceRiskLevel;
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

fn console_runtime_envelope(state: &AppState, session_id: &str, channel: &str, sender: &str) -> RuntimeEnvelope {
    let generation = state.config.pin();
    let workspace_id = generation.effective.workspace_dir.to_string_lossy().to_string();
    console_runtime_envelope_for_workspace(workspace_id, session_id, channel, sender)
        .with_config_generation(&generation)
}

/// Build the console runtime envelope for a session.
///
/// D4 C5 — console keeps its external session_id on the legacy basis. The console
/// `session_id` is a user-visible, external contract value: it is the path
/// parameter (`GET/POST /sessions/{session_id}/...`) and the id returned by the
/// session list. Unlike the gateway *fabric* path (C4), the console durable
/// `session_key` is therefore deliberately NOT canonicalized — doing so would
/// change the path/list id and break the frontend. The envelope passes the raw
/// `session_id` straight through as the durable key (no recipient component) and
/// carries no `legacy_session_key`, so console persistence and recall stay on the
/// single external id. Internal recall could read-merge in the future by setting
/// a legacy key on the principal, but the external id format never changes.
fn console_runtime_envelope_for_workspace(
    workspace_id: impl Into<String>,
    session_id: &str,
    channel: &str,
    sender: &str,
) -> RuntimeEnvelope {
    RuntimeEnvelope::console(workspace_id, session_id.to_string())
        .with_channel(channel.to_string())
        .with_sender(sender.to_string())
}

async fn append_console_turn(
    state: &AppState,
    envelope: &RuntimeEnvelope,
    role: &str,
    content: &str,
    record_message_event: bool,
) -> anyhow::Result<String> {
    let event = if record_message_event {
        let fabric = MemoryFabric::new(state.mem.clone(), envelope.workspace_id.clone());
        Some(if role == "assistant" {
            fabric
                .record_assistant_message(envelope.message_scope(), content)
                .await?
        } else {
            fabric
                .record_inbound_user_message(envelope.message_scope(), content, None, None)
                .await?
        })
    } else {
        None
    };
    let owner_id = envelope.resolved_owner_id();

    state
        .mem
        .append_conversation_turn(
            &envelope.session_key,
            envelope.channel.as_deref().unwrap_or("console"),
            envelope.sender.as_deref().unwrap_or("console-user"),
            role,
            content,
            None,
            event.as_ref().map(|event| event.event_id.as_str()),
            Some(owner_id.as_str()),
        )
        .await?;

    Ok(event.map_or_else(String::new, |event| event.event_id))
}

struct ConsoleTurnResult {
    reply: String,
    trace: crate::agent::loop_::ToolLoopTrace,
    route_decision: crate::llm::route_decision::RouteDecision,
    provider_started_at: chrono::DateTime<chrono::Utc>,
    history_commit_len: usize,
    envelope: RuntimeEnvelope,
}

fn console_tool_descriptions(config: &crate::config::Config) -> Vec<(&'static str, &'static str)> {
    let mut tool_descs = vec![
        ("shell", "Execute terminal commands"),
        ("file_read", "Read file contents"),
        ("file_write", "Write file contents"),
        ("memory_store", "Save to memory"),
        ("memory_recall", "Search memory"),
        ("memory_forget", "Delete a memory entry"),
        ("document_search", "Search stored document chunks with source anchors"),
        ("document_get_chunk", "Read a stored document chunk by id"),
    ];
    if config.composio.enabled {
        tool_descs.push(("composio", "Execute configured Composio app actions"));
    }
    if !config.agents.is_empty() {
        tool_descs.push(("delegate", "Delegate a sub-task to a specialized agent"));
    }
    tool_descs
}

async fn run_console_runtime_turn(
    state: &AppState,
    envelope: &RuntimeEnvelope,
    visible_message: &str,
    previous_turns: Vec<crate::memory::ConversationTurn>,
    source_message_event_id: Option<String>,
) -> anyhow::Result<ConsoleTurnResult> {
    // D2: this runtime turn rebuilds a `SecurityPolicy` (below) that gates tool
    // side-effects for the turn, so the config snapshot it derives from MUST be the
    // pinned TurnRuntimeGeneration, not an independently cached config owner. This
    // makes a reloaded autonomy / security.audit take effect for console-driven
    // turns without a restart; all non-security fields used for the prompt come from
    // the same snapshot, so the turn stays internally consistent.
    let turn_runtime = state.pin_turn_runtime();
    let config_snapshot = (*turn_runtime.config_generation.effective).clone();
    let provider_label = config_snapshot
        .default_provider
        .as_deref()
        .unwrap_or("openrouter")
        .to_string();
    let native_tools = turn_runtime
        .provider
        .capabilities_for(
            &turn_runtime.model,
            crate::providers::traits::ProviderRequestMode::NonStreaming,
        )
        .native_tool_calling;
    let skill_embedder =
        crate::memory::create_embedder_from_config(&config_snapshot, config_snapshot.api_key.as_deref());
    let skills = crate::skills::load_skills_with_embeddings(
        &config_snapshot.workspace_dir,
        &config_snapshot,
        skill_embedder.as_ref(),
    )
    .await?;
    let selected_skills =
        select_prompt_skills(visible_message, &skills, &config_snapshot, skill_embedder.as_ref()).await;
    let tool_descs = console_tool_descriptions(&config_snapshot);
    let system_prompt = build_runtime_system_prompt(
        &config_snapshot,
        &turn_runtime.model,
        &tool_descs,
        &selected_skills,
        native_tools,
        turn_runtime.tools_registry.as_ref(),
    );

    let mut turn_envelope = envelope
        .clone()
        .with_run_id(uuid::Uuid::new_v4().to_string())
        .with_config_generation(&turn_runtime.config_generation);
    if let Some(event_id) = source_message_event_id.as_ref() {
        turn_envelope = turn_envelope.with_source_message_event_id(event_id.clone());
    }
    let semantic_scope = turn_envelope.memory_write_context("private");
    let mem_context = build_context_with_shared_events_and_scope(
        state.mem.as_ref(),
        turn_envelope.memory_principal(),
        visible_message,
        config_snapshot.memory.min_relevance_score,
        Some(&semantic_scope),
    )
    .await;
    let enriched_message = if mem_context.preamble.is_empty() {
        visible_message.to_string()
    } else {
        format!("{}{}", mem_context.preamble, visible_message)
    };

    let mut history = Vec::with_capacity(previous_turns.len() + 2);
    history.push(ChatMessage::system(system_prompt));
    history.extend(previous_turns.into_iter().map(|turn| ChatMessage {
        role: turn.role,
        content: turn.content,
    }));
    history.push(ChatMessage::user(enriched_message));

    // D2 / FIX-P1-31: build via the shared `build_security_policy` helper so this
    // per-turn gateway authz site cannot drift from (or forget) the audit-config
    // wiring — construction is byte-for-byte identical to the former local
    // `from_config(&autonomy, &workspace_dir)` + audit-config of `security.audit`.
    // `config_snapshot` is already the hot SharedConfig (D) snapshot (see above), so
    // a reloaded autonomy / security.audit gates this console turn without a restart.
    let security = crate::runtime::bootstrap::build_security_policy(&config_snapshot);
    let scope_owner_id = turn_envelope.resolved_owner_id();
    let scope_ctx = ScopeContext {
        policy: security.as_ref(),
        sender: turn_envelope.sender.as_deref().unwrap_or("console-user"),
        channel: turn_envelope.channel.as_deref().unwrap_or("console"),
        chat_type: "private",
        chat_id: &turn_envelope.session_key,
        owner_id: Some(&scope_owner_id),
        topic_id: turn_envelope.topic_id.as_deref(),
        task_id: turn_envelope.resolved_task_id(),
        source_message_event_id: turn_envelope.source_message_event_id.as_deref(),
        config_generation_id: turn_envelope.config_generation_id,
        config_source_revision: turn_envelope.config_source_revision.as_deref(),
    };
    let noop_observer = NoopObserver;

    let route_decision = crate::llm::route_decision::RouteDecision::single_candidate_for_context(
        provider_label.clone(),
        turn_runtime.model.clone(),
        scope_owner_id.clone(),
        turn_envelope.session_key.clone(),
        source_message_event_id.clone(),
        None,
        "console_message",
        u32::try_from(visible_message.chars().count() / 4).unwrap_or(u32::MAX),
        !turn_runtime.tools_registry.is_empty(),
        false,
    );
    let provider_started_at = chrono::Utc::now();
    let loop_result = run_tool_call_loop_traced(
        turn_runtime.provider.as_ref(),
        &mut history,
        std::sync::Arc::clone(&turn_runtime.tools_registry),
        &noop_observer,
        state.hooks.as_ref(),
        &provider_label,
        &turn_runtime.model,
        turn_runtime.temperature,
        true,
        None,
        "console",
        &config_snapshot.multimodal,
        config_snapshot.agent.max_tool_iterations,
        config_snapshot.agent.parallel_tools,
        config_snapshot.agent.read_only_tool_concurrency_window,
        config_snapshot.agent.read_only_tool_timeout_secs,
        config_snapshot.agent.priority_scheduling_enabled,
        config_snapshot.agent.low_priority_tools.clone(),
        ToolConcurrencyGovernanceConfig {
            kill_switch_force_serial: config_snapshot.agent.concurrency_kill_switch_force_serial,
            rollout_stage: config_snapshot.agent.concurrency_rollout_stage.clone(),
            rollout_sample_percent: config_snapshot.agent.concurrency_rollout_sample_percent,
            rollout_channels: config_snapshot.agent.concurrency_rollout_channels.clone(),
            auto_rollback_enabled: config_snapshot.agent.concurrency_auto_rollback_enabled,
            rollback_timeout_rate_threshold: config_snapshot.agent.concurrency_rollback_timeout_rate_threshold,
            rollback_cancel_rate_threshold: config_snapshot.agent.concurrency_rollback_cancel_rate_threshold,
            rollback_error_rate_threshold: config_snapshot.agent.concurrency_rollback_error_rate_threshold,
        },
        Some(&config_snapshot.agent.compaction),
        None,
        None,
        Some(&scope_ctx),
        None,
        Some(&config_snapshot.tool_tiering),
        Some(
            DocumentIngestRuntime::from_envelope(state.mem.clone(), &turn_envelope)
                .with_source_message_event_id(source_message_event_id),
        ),
        crate::agent::loop_::ChatMode::default(),
    )
    .await;
    let (reply, trace) = match loop_result {
        Ok(result) => result,
        Err(error) => {
            let provider_outcome = crate::llm::route_decision::ProviderExecutionOutcome::failed_for_decision(
                &route_decision,
                provider_started_at,
                &error,
            );
            let terminal_id = turn_envelope
                .run_id
                .clone()
                .unwrap_or_else(|| provider_outcome.decision_id.clone());
            let fabric = MemoryFabric::new(state.mem.clone(), turn_envelope.workspace_id.clone());
            if let Err(finalize_error) = crate::agent::terminal::finalize_turn(
                &fabric,
                crate::agent::terminal::TurnTerminalCommit {
                    terminal_id,
                    scope: turn_envelope.message_scope(),
                    status: crate::agent::terminal::TurnTerminalStatus::Failed,
                    history: None,
                    history_scope: None,
                    provider_outcome: Some(provider_outcome),
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: error.to_string(),
                        started_at: provider_started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
                },
                &config_snapshot.cost,
                &config_snapshot.workspace_dir,
            )
            .await
            {
                tracing::warn!(error = %finalize_error, "Failed to commit failed console terminal event");
            }
            return Err(error);
        }
    };
    Ok(ConsoleTurnResult {
        reply,
        trace,
        route_decision,
        provider_started_at,
        history_commit_len: history.len(),
        envelope: turn_envelope,
    })
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
        || has_extension(file_name, &[".mp4", ".webm", ".mov", ".m4v", ".avi", ".mkv", ".ogg"])
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
        return Err(json_error(StatusCode::BAD_REQUEST, "message must not be empty"));
    }

    Ok(message)
}

async fn parse_multipart_message(state: &AppState, request: Request) -> Result<String, Response> {
    let mut multipart = Multipart::from_request(request, state)
        .await
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "Invalid multipart payload"))?;

    let uploads_root = {
        let config = state.config.load_full();
        config.workspace_dir.join(UPLOADS_DIR_NAME)
    };
    let mut uploads_root_prepared = false;

    let mut message = String::new();
    let mut attachments = Vec::new();
    let mut file_count = 0usize;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "Failed to parse multipart form fields"))?
    {
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
                    return Err(json_error(StatusCode::BAD_REQUEST, "Too many files (max 10)"));
                }

                let original_name = field.file_name().unwrap_or("upload.bin").to_string();
                let safe_name = sanitize_upload_filename(&original_name);
                let content_type = field.content_type().unwrap_or("").to_string();
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|_| json_error(StatusCode::BAD_REQUEST, "Failed to read uploaded file content"))?;

                if bytes.len() > MAX_UPLOAD_FILE_SIZE_BYTES {
                    return Err(json_error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "File too large (max 20MB per file)",
                    ));
                }

                super::authorize_resource_mutation(state, "gateway_api:sessions:upload", ResourceRiskLevel::Low)
                    .map_err(|error| error.into_response())?;

                if !uploads_root_prepared {
                    fs::create_dir_all(&uploads_root).await.map_err(|error| {
                        tracing::error!("Failed to create uploads directory: {error}");
                        json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to prepare uploads directory")
                    })?;
                    uploads_root_prepared = true;
                }

                let stored_name = format!("{}_{}_{}", Utc::now().timestamp_millis(), file_count, safe_name);
                let stored_path = uploads_root.join(stored_name);

                fs::write(&stored_path, &bytes).await.map_err(|error| {
                    tracing::error!("Failed to persist uploaded file: {error}");
                    json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to persist uploaded file")
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
        return Err(json_error(StatusCode::BAD_REQUEST, "message must not be empty"));
    }

    Ok(combined)
}

async fn parse_message_from_request(state: &AppState, request: Request) -> Result<String, Response> {
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

pub async fn get_sessions(State(state): State<AppState>, Query(query): Query<SessionsQuery>) -> impl IntoResponse {
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
            if !matches_session_filters(&session, status_filter.as_deref(), search_filter.as_deref()) {
                continue;
            }

            if filtered_offset < offset {
                filtered_offset += 1;
                continue;
            }

            let status = derive_session_status(&session.last_message_preview, session.message_count);
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
    let Some(session) = (match state.mem.get_conversation_session(&session_id).await {
        Ok(session) => session,
        Err(error) => {
            tracing::error!("Failed to load session metadata from DB: {error}");
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

    let limit = normalize_limit(query.limit);
    let offset = normalize_offset(query.offset);
    let principal = console_runtime_envelope(&state, &session_id, &session.channel, &session.sender).memory_principal();
    let turns = match state
        .mem
        .list_conversation_turns(&principal, &session_id, limit, offset)
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
    let runtime_envelope = console_runtime_envelope(&state, &session_id, &session.channel, &session.sender);

    let principal = runtime_envelope.memory_principal();
    let turns = match state
        .mem
        .list_conversation_turns(&principal, &session_id, MAX_PAGE_LIMIT, 0)
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

    if let Err(error) =
        super::authorize_resource_mutation(&state, "gateway_api:sessions:message", ResourceRiskLevel::Low)
    {
        return error.into_response();
    }

    let user_message_event_id = match append_console_turn(&state, &runtime_envelope, "user", &message, true).await {
        Ok(event_id) => event_id,
        Err(error) => {
            tracing::error!("Failed to persist user session turn: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to persist session message" })),
            )
                .into_response();
        }
    };

    let turn =
        match run_console_runtime_turn(&state, &runtime_envelope, &message, turns, Some(user_message_event_id)).await {
            Ok(turn) => turn,
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
    let reply = turn.reply.clone();

    if let Err(error) =
        super::authorize_resource_mutation(&state, "gateway_api:sessions:assistant_message", ResourceRiskLevel::Low)
    {
        return error.into_response();
    }

    let provider_outcome =
        crate::agent::terminal::provider_outcome_from_trace(&turn.route_decision, turn.provider_started_at, turn.trace);
    let terminal_id = turn
        .envelope
        .run_id
        .clone()
        .unwrap_or_else(|| provider_outcome.decision_id.clone());
    let terminal_fabric = MemoryFabric::new(state.mem.clone(), turn.envelope.workspace_id.clone());
    let (cost_config, workspace_dir) = {
        let config = state.config.load();
        (config.cost.clone(), config.workspace_dir.clone())
    };
    let terminal_committed = match crate::agent::terminal::finalize_turn(
        &terminal_fabric,
        crate::agent::terminal::TurnTerminalCommit {
            terminal_id,
            scope: turn.envelope.message_scope(),
            status: crate::agent::terminal::TurnTerminalStatus::Completed,
            history: Some(crate::agent::terminal::TurnHistoryProjection {
                assistant_content: reply.clone(),
                history_commit_len: turn.history_commit_len,
            }),
            history_scope: None,
            provider_outcome: Some(provider_outcome),
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary: "console turn completed".to_string(),
                started_at: turn.provider_started_at,
                finished_at: chrono::Utc::now(),
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
        },
        &cost_config,
        &workspace_dir,
    )
    .await
    {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(error = %error, "Failed to commit shared console terminal event");
            false
        }
    };

    if let Err(error) = append_console_turn(&state, &turn.envelope, "assistant", &reply, !terminal_committed).await {
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
        let config = state.config.load_full();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::gateway::{GatewayRateLimiter, IdempotencyStore};
    use crate::hooks::HookManager;
    use crate::memory::{ConversationTurn, Memory, MemoryCategory, MemoryEntry};
    use crate::providers::{ChatRequest, ChatResponse, Provider};
    use crate::security::pairing::PairingGuard;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::broadcast;

    #[test]
    fn console_runtime_envelope_preserves_session_channel_and_sender() {
        let envelope = console_runtime_envelope_for_workspace("workspace", "signal_alice", "signal", "alice");
        let scope = envelope.message_scope();

        assert_eq!(scope.source, "console");
        assert_eq!(scope.session_key.as_deref(), Some("signal_alice"));
        assert_eq!(scope.channel.as_deref(), Some("signal"));
        assert_eq!(scope.sender.as_deref(), Some("alice"));
    }

    // D4 C5: the console external session_id is NOT canonicalized. The durable
    // session_key stays the raw external id (path/list contract value), and no
    // legacy_session_key is carried (console persistence/recall stay single-key on
    // the external id). This guards against accidentally migrating the console
    // durable key the way the gateway *fabric* path was migrated in C4.
    #[test]
    fn d4_console_session_id_is_kept_legacy_not_canonicalized() {
        let session_id = "signal_alice";
        let envelope = console_runtime_envelope_for_workspace("workspace", session_id, "signal", "alice");

        // Durable write key == external id (no recipient-aware canonical).
        assert_eq!(envelope.session_key, session_id);
        assert_eq!(envelope.message_scope().session_key.as_deref(), Some(session_id));
        // No legacy key carried -> single-key recall on the external id.
        let principal = envelope.memory_principal();
        assert_eq!(principal.session_key.as_deref(), Some(session_id));
        assert_eq!(principal.legacy_session_key, None);
        assert_eq!(principal.session_key_candidates(), vec![session_id.to_string()]);
        // The canonical derivation differs from the external id; C5 deliberately
        // does NOT adopt it for the durable key (would break the path/list id).
        assert_ne!(envelope.canonical_session_key(), session_id);
        assert_eq!(envelope.canonical_session_key(), "console:signal:alice:-");
    }

    #[derive(Default)]
    struct TestMemory;

    #[async_trait]
    impl Memory for TestMemory {
        fn name(&self) -> &str {
            "test"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct CapturingProvider {
        requests: parking_lot::Mutex<Vec<Vec<ChatMessage>>>,
    }

    #[async_trait]
    impl Provider for CapturingProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            anyhow::bail!("console runtime should use structured Provider::chat");
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            self.requests.lock().push(request.messages.to_vec());
            Ok(ChatResponse {
                text: Some("console-runtime-ok".to_string()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }
    }

    struct TestConfigParticipant;
    struct TestPreparedConfig;

    impl crate::config::ConfigGenerationParticipant for TestConfigParticipant {
        fn name(&self) -> &'static str {
            "gateway_sessions_test_runtime"
        }

        fn supports_rebuild_field(&self, _field: &str) -> bool {
            true
        }

        fn supports_controlled_restart_field(&self, _field: &str) -> bool {
            true
        }

        fn prepares_for_field(&self, _field: &str) -> bool {
            true
        }

        fn prepare(
            &self,
            _generation: Arc<crate::config::ConfigGeneration>,
            _changed_fields: &[String],
        ) -> anyhow::Result<Box<dyn crate::config::PreparedConfigGeneration>> {
            Ok(Box::new(TestPreparedConfig))
        }
    }

    impl crate::config::PreparedConfigGeneration for TestPreparedConfig {
        fn commit(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn rollback(&mut self) {}
    }

    fn test_app_state(config: Config, provider: Arc<dyn Provider>) -> AppState {
        static PARTICIPANT: std::sync::OnceLock<Arc<dyn crate::config::ConfigGenerationParticipant>> =
            std::sync::OnceLock::new();
        let config = crate::config::new_shared(config);
        config.register_participant(PARTICIPANT.get_or_init(|| Arc::new(TestConfigParticipant)));
        AppState {
            config,
            provider,
            model: "test-model".into(),
            temperature: 0.0,
            mem: Arc::new(TestMemory),
            auto_save: false,
            tools_registry: Arc::new(vec![]),
            mcp_tool: None,
            turn_runtime: None,
            hooks: Arc::new(HookManager::new(std::env::temp_dir())),
            webhook_token_hash: None,
            webhook_signing_secret: None,
            pairing: Arc::new(PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
            whatsapp: None,
            signal: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            observer: Arc::new(NoopObserver),
            start_time: Instant::now(),
            gateway_port: 0,
            logs_broadcast_tx: broadcast::channel(16).0,
            #[cfg(feature = "wasm-plugins")]
            plugin_runtime: None,
        }
    }

    #[tokio::test]
    async fn console_runtime_turn_uses_shared_tool_loop_history() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.workspace_dir = tmp.path().to_path_buf();
        let provider_impl = Arc::new(CapturingProvider::default());
        let state = test_app_state(config, provider_impl.clone());
        let envelope = console_runtime_envelope_for_workspace(
            tmp.path().to_string_lossy().to_string(),
            "console-session",
            "console",
            "alice",
        );
        let previous_turns = vec![ConversationTurn {
            id: 1,
            session_key: "console-session".to_string(),
            role: "assistant".to_string(),
            content: "previous answer".to_string(),
            timestamp: "2026-05-27T00:00:00Z".to_string(),
            message_id: Some("previous-event".to_string()),
        }];

        let turn = run_console_runtime_turn(
            &state,
            &envelope,
            "current question",
            previous_turns,
            Some("user-event".into()),
        )
        .await
        .unwrap();

        assert_eq!(turn.reply, "console-runtime-ok");
        let requests = provider_impl.requests.lock();
        assert_eq!(requests.len(), 1);
        let history = requests.first().expect("request should be recorded");
        assert_eq!(history.first().map(|m| m.role.as_str()), Some("system"));
        assert!(
            history
                .iter()
                .any(|m| m.role == "assistant" && m.content == "previous answer")
        );
        assert_eq!(history.last().map(|m| m.role.as_str()), Some("user"));
        assert!(history.last().is_some_and(|m| m.content.contains("current question")));
    }

    /// Gateway authorization reads the active ConfigGeneration, so a config
    /// reload that lowers autonomy to ReadOnly flips a previously-allowed mutation
    /// to denied without a process restart or a second cached config owner.
    #[test]
    fn authz_reads_hot_shared_config_after_reload() {
        use crate::security::policy::{AutonomyLevel, ResourceRiskLevel};

        // Start in the default autonomous policy: a low-risk gateway mutation is
        // allowed.
        let provider: Arc<dyn Provider> = Arc::new(CapturingProvider::default());
        let state = test_app_state(Config::default(), provider);
        assert!(
            crate::gateway::api::authorize_resource_mutation(
                &state,
                "gateway_api:config:update",
                ResourceRiskLevel::Low,
            )
            .is_ok(),
            "default autonomous policy should allow a low-risk mutation"
        );

        // Hot-reload: publish a ReadOnly config into D only. C is intentionally left
        // stale to prove authorization no longer depends on it.
        let read_only = Config {
            autonomy: crate::config::AutonomyConfig {
                level: AutonomyLevel::ReadOnly,
                ..crate::config::AutonomyConfig::default()
            },
            ..Config::default()
        };
        state
            .config
            .apply_runtime_config(read_only, crate::config::ConfigReloadTrigger::Test)
            .expect("apply read-only config");

        // Same call, same risk — now denied because the authz path reads D.
        let denied = crate::gateway::api::authorize_resource_mutation(
            &state,
            "gateway_api:config:update",
            ResourceRiskLevel::Low,
        )
        .expect_err("ReadOnly autonomy published to SharedConfig must deny the mutation");
        assert_eq!(denied.0, axum::http::StatusCode::FORBIDDEN);
        assert!(
            denied
                .1
                .0
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .contains("read-only mode"),
            "denial reason should reflect read-only autonomy"
        );
    }

    /// D2 /修3 regression: `post_config` merges the incoming delta onto the hot
    /// SharedConfig (D), NOT the cached C Mutex. We seed D with a hot field value that
    /// differs from C, POST an unrelated delta, and assert the hot field SURVIVES — if
    /// the merge had used stale C as its base it would silently revert the hot field.
    #[tokio::test]
    async fn post_config_merge_base_is_hot_shared_config_not_stale_cache() {
        use super::super::config::post_config;

        let tmp = tempfile::tempdir().expect("tempdir");
        let config_path = tmp.path().join("config.toml");
        let workspace = tmp.path().join("workspace");

        // C (cached) base: default temperature, valid config path on disk.
        let mut cached = Config::default();
        cached.config_path = config_path.clone();
        cached.workspace_dir = workspace.clone();
        cached.default_temperature = 0.0;

        let provider: Arc<dyn Provider> = Arc::new(CapturingProvider::default());
        let state = test_app_state(cached, provider);

        // D (hot) holds a DIFFERENT default_temperature — simulating a prior hot
        // reload that C has not yet observed.
        let mut hot = Config::default();
        hot.config_path = config_path.clone();
        hot.workspace_dir = workspace;
        hot.default_temperature = 0.42;
        state
            .config
            .apply_runtime_config(hot, crate::config::ConfigReloadTrigger::Test)
            .expect("apply hot config");

        // POST a delta that does NOT touch default_temperature.
        let resp = post_config(
            axum::extract::State(state.clone()),
            axum::Json(serde_json::json!({ "auto_save": true })),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        // The hot value from D survived the merge (would be 0.0 if C were the base).
        let after = state.config.load_full();
        assert!(
            (after.default_temperature - 0.42).abs() < 1e-9,
            "merge base must be D: hot default_temperature should be preserved, got {}",
            after.default_temperature
        );
        // C is re-synced to D (C == D invariant restored).
        assert!((state.config.load_full().default_temperature - 0.42).abs() < 1e-9);
    }

    /// D2 / 修1: the `/api/config/reload` route builds its OWN authorization gate from
    /// the hot SharedConfig (D). Publishing ReadOnly to D causes the reload's gate to
    /// deny, proving reload authz reads D and not the stale C Mutex.
    ///
    /// This test is hardened against the "500 could be IO, not authz" ambiguity in two
    /// independent ways (Codex review hardening):
    ///   (a) the D config_path points at a REAL, loadable `config.toml` on disk, so the
    ///       `ConfigReloadTool` load step (`Config::load_from_path`) CANNOT fail — any
    ///       500 therefore can only come from the read-only authorization gate; and
    ///   (b) we assert the surfaced error body carries the gate's signature string
    ///       (`"read-only mode"`, produced by `SideEffectGate::authorize_resource_operation`
    ///       under `AutonomyLevel::ReadOnly`), not a `"Failed to load merged config"` /
    ///       `"Config path is not set"` IO message; and
    ///   (c) a positive control: with the SAME on-disk path but D = Supervised, the very
    ///       same request SUCCEEDS (200). The only thing that changed between the two runs
    ///       is the autonomy level in D, so the difference is proven to come from authz,
    ///       not from config loading.
    #[tokio::test]
    async fn config_reload_route_authz_reads_hot_shared_config() {
        use super::super::config::post_config_reload;

        let tmp = tempfile::tempdir().expect("tempdir");
        let config_path = tmp.path().join("config.toml");
        let workspace = tmp.path().join("workspace");
        // Write a REAL, valid config.toml so the reload's load step cannot fail. With a
        // loadable path, a 500 can only originate from the authorization gate.
        std::fs::create_dir_all(&workspace).expect("create workspace dir");
        std::fs::write(&config_path, "default_temperature = 0.3\n").expect("write config.toml");

        let mut base = Config::default();
        base.config_path = config_path.clone();
        base.workspace_dir = workspace.clone();

        // --- Positive control: D = Supervised, SAME on-disk path → reload SUCCEEDS. ---
        // Proves the config_path is genuinely loadable and the only variable across the
        // two assertions below is the autonomy level published to D.
        let provider: Arc<dyn Provider> = Arc::new(CapturingProvider::default());
        let allow_state = test_app_state(base.clone(), provider);
        let mut supervised = Config::default();
        supervised.config_path = config_path.clone();
        supervised.workspace_dir = workspace.clone();
        supervised.autonomy.level = crate::security::policy::AutonomyLevel::Supervised;
        allow_state
            .config
            .apply_runtime_config(supervised, crate::config::ConfigReloadTrigger::Test)
            .expect("apply supervised config");

        let allow_resp = post_config_reload(axum::extract::State(allow_state)).await;
        assert_eq!(
            allow_resp.status(),
            axum::http::StatusCode::OK,
            "Supervised autonomy with a valid config_path must allow the reload (positive control)"
        );

        // --- Denial: D = ReadOnly, SAME on-disk path → reload BLOCKED by the gate. ---
        let provider: Arc<dyn Provider> = Arc::new(CapturingProvider::default());
        let state = test_app_state(base, provider);
        // Publish ReadOnly to D only; C stays permissive (default Full).
        let mut read_only = Config::default();
        read_only.config_path = config_path;
        read_only.workspace_dir = workspace;
        read_only.autonomy.level = crate::security::policy::AutonomyLevel::ReadOnly;
        state
            .config
            .apply_runtime_config(read_only, crate::config::ConfigReloadTrigger::Test)
            .expect("apply read-only config");

        let resp = post_config_reload(axum::extract::State(state)).await;
        // ReadOnly gate denial surfaces as INTERNAL_SERVER_ERROR carrying the gate error.
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        // Inspect the error BODY: it must be the authorization-gate signature, NOT an IO
        // failure. This distinguishes "blocked by read-only authz" from "failed to load
        // config", which is the whole point of the D2/修1 fix.
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read reload error body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("error body is JSON");
        let error_text = json
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(
            error_text.contains("read-only mode"),
            "reload denial must come from the read-only authorization gate, got: {error_text}"
        );
        assert!(
            !error_text.contains("Failed to load merged config") && !error_text.contains("Config path is not set"),
            "denial must NOT be an IO/load failure, got: {error_text}"
        );
    }
}
