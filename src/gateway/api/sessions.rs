use super::{AppState, extract_resource_auth_token};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::runtime_prompt::build_runtime_system_prompt;
use crate::agent::turn_contracts::ScopeContext;
use crate::agent::turn_engine::run_turn;
use crate::gateway::jobs;
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

/// D-037 grants authenticated API tokens full operator access to the selected
/// transcript. This read principal never enters the turn envelope or tool scope.
fn operator_history_principal(
    _authenticated: &super::ApiPrincipal,
    envelope: &RuntimeEnvelope,
) -> crate::memory::MemoryPrincipal {
    let mut principal = envelope.memory_principal();
    principal.owner_id = Some("system:*".to_owned());
    principal.agent_id = Some("system".to_owned());
    principal.require_owner_match = false;
    principal
}

async fn recover_console_history(
    state: &AppState,
    runtime: &crate::gateway::TurnRuntimeGeneration,
    _authenticated: &super::ApiPrincipal,
    session_key: &str,
) -> anyhow::Result<()> {
    if !runtime.config_generation.effective.causal_tree.enabled {
        return Ok(());
    }
    let workspace = runtime.config_generation.effective.workspace_dir.to_string_lossy();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let mut cleanup = None;
    let recovery = async {
        let mut cursor = 0;
        loop {
            anyhow::ensure!(
                !cancellation.is_cancelled(),
                "Console recovery cancelled during shutdown"
            );
            let events = state
                .mem
                .list_console_terminal_events(&workspace, session_key, cursor, 128)
                .await?;
            if events.is_empty() {
                break;
            }
            for event in events {
                anyhow::ensure!(
                    event.id > cursor
                        && event.workspace_id == workspace
                        && event.session_key.as_deref() == Some(session_key)
                        && event.source == crate::memory::MessageEventSource::Console
                        && event.event_type == "turn.finalized",
                    "Console recovery returned an invalid marker"
                );
                cursor = event.id;
                let payload: serde_json::Value = serde_json::from_str(
                    event
                        .raw_payload_json
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("Console terminal payload missing"))?,
                )?;
                if payload.get("decision_id").is_none_or(serde_json::Value::is_null)
                    && payload.get("terminal_replay_v1").is_none()
                    && payload.get("terminal_replay_mac_v1").is_none()
                {
                    // A stripped marker must not masquerade as a legacy turn
                    // when a durable decision claim still identifies the run.
                    let claim = if let Some(run) = event.run_id.as_deref() {
                        state
                            .mem
                            .find_message_event_by_idempotency_key(&workspace, &format!("turn:{run}:decision-claim"))
                            .await?
                    } else {
                        None
                    };
                    if claim.is_none() {
                        continue;
                    }
                }
                if cleanup.is_none() {
                    cleanup = Some(runtime.decision_slot.cleanup.register(cancellation.clone())?);
                }
                let terminal_id = event
                    .run_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Console terminal run missing"))?;
                anyhow::ensure!(
                    event.idempotency_key.as_deref() == Some(format!("turn:{terminal_id}:final").as_str()),
                    "Console recovery marker identity changed"
                );
                let owner = event
                    .owner_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Console terminal owner missing"))?;
                let service = runtime.decision_slot.publish(runtime, state).await?;
                service.rejoin_terminal(terminal_id, owner).await?;
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), recovery)
        .await
        .map_err(|_| anyhow::anyhow!("Console history recovery timed out; retry required"))??;
    if let Some(ticket) = cleanup {
        ticket.finish(true);
    }
    Ok(())
}

async fn append_console_turn(
    state: &AppState,
    envelope: &RuntimeEnvelope,
    role: &str,
    content: &str,
    record_message_event: bool,
    durable_projection: bool,
) -> anyhow::Result<String> {
    // The console persists its reply twice, as a message event and as the
    // transcript turn: both are redacted like the terminal commit (D-053).
    let content: std::borrow::Cow<'_, str> = if role == "assistant" {
        std::borrow::Cow::Owned(crate::agent::terminal::redact_persisted_text(content))
    } else {
        std::borrow::Cow::Borrowed(content)
    };
    let event = if record_message_event {
        let fabric = MemoryFabric::new(state.mem.clone(), envelope.workspace_id.clone());
        Some(if role == "assistant" {
            fabric
                .record_assistant_message(envelope.message_scope(), &*content)
                .await?
        } else {
            fabric
                .record_inbound_user_message(envelope.message_scope(), &*content, None, None)
                .await?
        })
    } else {
        None
    };
    if durable_projection {
        let event = event
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Console projection requires a recorded event"))?;
        state.mem.project_console_message(event).await?;
        return Ok(event.event_id.clone());
    }
    let owner_id = envelope.resolved_owner_id();

    state
        .mem
        .append_conversation_turn(
            &envelope.session_key,
            envelope.channel.as_deref().unwrap_or("console"),
            envelope.sender.as_deref().unwrap_or("console-user"),
            role,
            &content,
            None,
            event.as_ref().map(|event| event.event_id.as_str()),
            Some(owner_id.as_str()),
        )
        .await?;

    Ok(event.map_or_else(String::new, |event| event.event_id))
}

struct ConsoleTurnResult {
    reply: String,
    awaiting_approval: bool,
    trace: crate::agent::turn_contracts::ToolLoopTrace,
    route_decision: crate::llm::route_decision::RouteDecision,
    provider_started_at: chrono::DateTime<chrono::Utc>,
    history_commit_len: usize,
    envelope: RuntimeEnvelope,
}

struct ConsoleDecisionTurn<'a> {
    service: std::sync::Arc<crate::runtime::turn_decision::TurnDecisionService>,
    context: std::sync::Arc<crate::runtime::turn_decision::TurnDecisionContext>,
    proof: &'a crate::gateway::decision_source::GatewayTurnProof,
    cancellation: tokio_util::sync::CancellationToken,
    provider_trace: Option<(
        std::sync::Arc<crate::agent::turn_contracts::RetainedProviderTrace>,
        std::sync::Arc<crate::llm::route_decision::RouteDecision>,
    )>,
}

fn console_route_decision(
    runtime: &crate::gateway::TurnRuntimeGeneration,
    envelope: &RuntimeEnvelope,
    message: &str,
) -> anyhow::Result<crate::llm::route_decision::RouteDecision> {
    let provider = runtime
        .provider_binding
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Console provider binding missing"))?;
    Ok(crate::llm::route_decision::RouteDecision::single_candidate_for_context(
        provider.provider_name().to_string(),
        runtime.model.clone(),
        envelope.resolved_owner_id(),
        envelope.session_key.clone(),
        envelope.source_message_event_id.clone(),
        None,
        "console_message",
        u32::try_from(message.chars().count() / 4).unwrap_or(u32::MAX),
        !runtime.tools_registry.is_empty(),
        false,
    ))
}

async fn run_console_runtime_turn(
    turn_runtime: std::sync::Arc<crate::gateway::TurnRuntimeGeneration>,
    state: &AppState,
    visible_message: &str,
    previous_turns: Vec<crate::memory::ConversationTurn>,
    decision: &ConsoleDecisionTurn<'_>,
) -> anyhow::Result<ConsoleTurnResult> {
    decision.proof.verify_runtime_input(&turn_runtime, visible_message)?;
    let turn_envelope = decision.proof.envelope().clone();
    // D2: this runtime turn rebuilds a `SecurityPolicy` (below) that gates tool
    // side-effects for the turn, so the config snapshot it derives from MUST be the
    // pinned TurnRuntimeGeneration, not an independently cached config owner. This
    // makes a reloaded autonomy / security.audit take effect for console-driven
    // turns without a restart; all non-security fields used for the prompt come from
    // the same snapshot, so the turn stays internally consistent.
    let config_snapshot = (*turn_runtime.config_generation.effective).clone();
    let provider_label = turn_runtime
        .provider_binding
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Console provider binding missing"))?
        .provider_name()
        .to_string();
    let native_tools = turn_runtime
        .provider
        .capabilities_for(
            &turn_runtime.model,
            crate::providers::traits::ProviderRequestMode::NonStreaming,
        )
        .native_tool_calling;
    let selected_skills = decision
        .service
        .agent_prompt_skills(decision.proof, &decision.context)?;
    let system_prompt =
        build_runtime_system_prompt(&config_snapshot, &turn_runtime.model, &selected_skills, native_tools);

    let mem_context = crate::agent::context_recall::RecalledMemoryContext {
        preamble: crate::agent::memory_loader::format_selected_context(None, decision.context.retrieved()?)?,
        ids: Vec::new(),
    };
    let enriched_message = if mem_context.preamble.is_empty() {
        visible_message.to_string()
    } else {
        format!("{}{}", mem_context.preamble, visible_message)
    };

    let mut history = Vec::with_capacity(previous_turns.len() + 2);
    history.push(ChatMessage::system(system_prompt));
    // stored console turns are neutralised like fresh input
    history.extend(
        previous_turns
            .into_iter()
            .map(|turn| crate::agent::turn_contracts::stored_turn_message(turn.role, turn.content)),
    );
    history.push(crate::agent::turn_contracts::inbound_user_message(&enriched_message));

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
        session_key: Some(&turn_envelope.session_key),
        owner_id: Some(&scope_owner_id),
        owner_isolated: turn_envelope.owner_isolated,
        topic_id: turn_envelope.topic_id.as_deref(),
        task_id: turn_envelope.resolved_task_id(),
        source_message_event_id: turn_envelope.source_message_event_id.as_deref(),
        config_generation_id: turn_envelope.config_generation_id,
        config_source_revision: turn_envelope.config_source_revision.as_deref(),
    };
    let noop_observer = NoopObserver;

    let route_decision = match &decision.provider_trace {
        Some((_, route)) => route.as_ref().clone(),
        None => console_route_decision(&turn_runtime, &turn_envelope, visible_message)?,
    };
    // OQ-23 / R5: budget on the *effective* window of the model this turn
    // serves (the operator/discovery/catalog model limits and
    // `reserved_output_tokens`), with the same arguments the CLI, TUI and
    // webhook resolve with, so one request is trimmed the same way on every
    // surface of this process.
    let mut effective_compaction = decision
        .service
        .effective_compaction(&provider_label, &turn_runtime.model);
    effective_compaction.config = decision.context.main_compaction().clone();
    crate::router::context::trace_effective_compaction_resolution(&effective_compaction);
    let provider_started_at = chrono::Utc::now();
    let loop_result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "console",
            scope: Some(&scope_ctx),
            mode: crate::agent::turn_contracts::ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &config_snapshot.multimodal,
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: config_snapshot.agent.read_only_tool_concurrency_window,
                priority_scheduling_enabled: config_snapshot.agent.priority_scheduling_enabled,
                low_priority_tool_names: config_snapshot.agent.low_priority_tools.clone(),
            },
            compaction: Some(&effective_compaction.config),
            decision: crate::agent::turn_request::TurnDecision {
                service: &decision.service,
                context: &decision.context,
                proof: decision.proof,
            },
        },
        crate::agent::turn_request::TurnServices {
            provider: turn_runtime.provider.as_ref(),
            provider_name: &provider_label,
            model: &turn_runtime.model,
            temperature: turn_runtime.temperature,
            tools_registry: std::sync::Arc::clone(&turn_runtime.tools_registry),
            observer: &noop_observer,
            hooks: state.hooks.as_ref(),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(crate::agent::turn_contracts::ToolLoopRuntimeAdapter {
                verify_chat_completion: false,
                provider_trace: decision
                    .provider_trace
                    .as_ref()
                    .map(|(trace, _)| std::sync::Arc::clone(trace)),
                events: None,
                stream_provider: false,
                tool_execution_service: None,
                tool_execution_context: crate::tools::ToolExecutionContext::new(turn_envelope.clone(), "private")
                    .with_chat_id(&turn_envelope.session_key),
                allowed_tool_names: None,
            }),
            memory: crate::agent::turn_request::ToolLoopMemory::new(
                &state.mem,
                &config_snapshot.workspace_dir,
                Some(
                    DocumentIngestRuntime::from_envelope(state.mem.clone(), &turn_envelope)
                        .with_source_message_event_id(turn_envelope.source_message_event_id.clone()),
                ),
            )
            .with_event_fabric(
                MemoryFabric::new(state.mem.clone(), turn_envelope.workspace_id.clone())
                    .with_event_recording(config_snapshot.memory.event_recording_config()),
            )
            .with_request_event_scope(turn_envelope.message_scope()),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: Some(decision.cancellation.clone()),
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await;
    let (outcome, trace) = loop_result?;
    let awaiting_approval = matches!(
        &outcome,
        crate::agent::turn_contracts::ToolLoopOutcome::AwaitingApproval { .. }
    );
    if decision.context.approval_collection_proof().is_some() {
        anyhow::ensure!(
            awaiting_approval,
            "Console collection returned without a durable pending approval"
        );
    }
    let reply = outcome.into_text();
    anyhow::ensure!(
        trace.execution_trace.as_ref() == Some(&decision.context.terminal_execution_trace()),
        "console execution trace differs from owning decision"
    );
    Ok(ConsoleTurnResult {
        reply,
        awaiting_approval,
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

#[allow(clippy::result_large_err)] // the axum `Response` is the error contract of these parsers
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

#[allow(clippy::result_large_err)]
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

#[allow(clippy::result_large_err)]
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateSessionRequest {}

pub async fn post_session(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<super::ApiPrincipal>,
    Json(_body): Json<CreateSessionRequest>,
) -> Response {
    if let Err(error) =
        super::authorize_resource_mutation(&state, "gateway_api:sessions:create", ResourceRiskLevel::Low)
    {
        return error.into_response();
    }
    let session_id = format!("console-{}", uuid::Uuid::new_v4());
    if let Err(error) = state
        .mem
        .create_conversation_session(&session_id, &principal.owner_id)
        .await
    {
        tracing::error!(%error, "Failed to create Console session");
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create session");
    }
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"session_id": session_id, "status":"empty", "message_count":0})),
    )
        .into_response()
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
    axum::Extension(api_principal): axum::Extension<super::ApiPrincipal>,
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
    let runtime = state.pin_turn_runtime();
    if let Err(error) = recover_console_history(&state, &runtime, &api_principal, &session_id).await {
        tracing::error!(error = %error, "Console history recovery failed");
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Console history recovery is incomplete",
        );
    }
    let principal = operator_history_principal(
        &api_principal,
        &console_runtime_envelope(&state, &session_id, &session.channel, &session.sender),
    );
    let turns = match state
        .mem
        .list_console_conversation_turns(&principal, &session_id, limit, offset)
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DecisionApprovalRequest {
    approved: bool,
}

pub(super) async fn get_decision_approval(
    State(state): State<AppState>,
    Path((session_id, decision_id)): Path<(String, String)>,
    axum::Extension(principal): axum::Extension<super::ApiPrincipal>,
) -> Response {
    let decision_id = match crate::causal_tree::ids::DecisionId::parse(&decision_id) {
        Ok(id) => id,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "Invalid decision id"),
    };
    let runtime = state.pin_turn_runtime();
    if !runtime.config_generation.effective.causal_tree.enabled {
        return json_error(StatusCode::CONFLICT, "Decision approvals are disabled");
    }
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cleanup =
        match crate::runtime::turn_cleanup::CleanupScope::new(Some(&runtime.decision_slot.cleanup), cancellation) {
            Ok(cleanup) => cleanup,
            Err(_) => return json_error(StatusCode::SERVICE_UNAVAILABLE, "Gateway is draining"),
        };
    let result = cleanup
        .run(Box::pin(async {
            let service = runtime.decision_slot.publish(&runtime, &state).await?;
            service
                .inspect_pending_decision_approval(&principal.owner_id, &session_id, decision_id)
                .await
        }))
        .await;
    result.map_or_else(
        |_| json_error(StatusCode::CONFLICT, "Decision approval is unavailable or expired"),
        |view| Json(view).into_response(),
    )
}

pub(super) async fn post_decision_approval(
    State(state): State<AppState>,
    Path((session_id, decision_id)): Path<(String, String)>,
    axum::Extension(principal): axum::Extension<super::ApiPrincipal>,
    Json(request): Json<DecisionApprovalRequest>,
) -> Response {
    change_decision_approval(
        state,
        session_id,
        decision_id,
        principal,
        DecisionApprovalChange::Review(request.approved),
    )
    .await
}

pub(super) async fn delete_decision_approval(
    State(state): State<AppState>,
    Path((session_id, decision_id)): Path<(String, String)>,
    axum::Extension(principal): axum::Extension<super::ApiPrincipal>,
) -> Response {
    change_decision_approval(
        state,
        session_id,
        decision_id,
        principal,
        DecisionApprovalChange::Revoke,
    )
    .await
}

enum DecisionApprovalChange {
    Review(bool),
    Revoke,
}

// This control-plane check does not grant permission to execute a file effect.
// Only the existing signed review issuer and exact-call resume consumer do that.
#[derive(Clone, Copy, Debug)]
enum ConsoleDecisionControl {
    Review,
    Reject,
    Revoke,
    Resume,
}

fn console_decision_control_policy(config: &crate::config::Config) -> Result<(), &'static str> {
    crate::security::SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)
        .enforce_tool_operation(crate::security::policy::ToolOperation::Act, "console decision control")
        .map_err(|_| "Decision control is not permitted by policy")
}

async fn authorize_console_decision_control(
    service: &crate::runtime::turn_decision::TurnDecisionService,
    principal: &super::ApiPrincipal,
    session: &str,
    decision_id: crate::causal_tree::ids::DecisionId,
    action: ConsoleDecisionControl,
) -> anyhow::Result<()> {
    // Authenticate the durable pending record, its original source and decision
    // chain under the same pinned publication; an API identity alone is insufficient.
    if matches!(action, ConsoleDecisionControl::Reject | ConsoleDecisionControl::Revoke) {
        service
            .load_pending_decision_approval(&principal.owner_id, session, decision_id)
            .await?;
    } else {
        service
            .inspect_pending_decision_approval(&principal.owner_id, session, decision_id)
            .await?;
    }
    if matches!(action, ConsoleDecisionControl::Resume) {
        // Pending metadata remains inspectable after a grant is consumed.
        // Refuse a closed or unreviewed operation before recording new ingress;
        // the final execution path still owns the atomic one-use claim.
        service
            .validate_pending_decision_resume(&principal.owner_id, session, decision_id)
            .await?;
    }
    tracing::info!(
        owner = %principal.owner_id,
        decision_id = %decision_id,
        action = ?action,
        "Authenticated owner decision control request authorized; execution not granted"
    );
    Ok(())
}

async fn change_decision_approval(
    state: AppState,
    session_id: String,
    decision_id: String,
    principal: super::ApiPrincipal,
    change: DecisionApprovalChange,
) -> Response {
    let decision_id = match crate::causal_tree::ids::DecisionId::parse(&decision_id) {
        Ok(id) => id,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "Invalid decision id"),
    };
    let runtime = state.pin_turn_runtime();
    if !runtime.config_generation.effective.causal_tree.enabled {
        return json_error(StatusCode::CONFLICT, "Decision approvals are disabled");
    }
    if let Err(message) = console_decision_control_policy(&runtime.config_generation.effective) {
        return json_error(StatusCode::FORBIDDEN, message);
    }
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cleanup =
        match crate::runtime::turn_cleanup::CleanupScope::new(Some(&runtime.decision_slot.cleanup), cancellation) {
            Ok(cleanup) => cleanup,
            Err(_) => return json_error(StatusCode::SERVICE_UNAVAILABLE, "Gateway is draining"),
        };
    let result = cleanup
        .run(Box::pin(async {
            let service = runtime.decision_slot.publish(&runtime, &state).await?;
            let action = match change {
                DecisionApprovalChange::Review(true) => ConsoleDecisionControl::Review,
                DecisionApprovalChange::Review(false) => ConsoleDecisionControl::Reject,
                DecisionApprovalChange::Revoke => ConsoleDecisionControl::Revoke,
            };
            authorize_console_decision_control(&service, &principal, &session_id, decision_id, action).await?;
            let receipt = match change {
                DecisionApprovalChange::Review(approved) => serde_json::to_value(
                    service
                        .review_pending_decision_approval(&principal.owner_id, &session_id, decision_id, approved)
                        .await?,
                )?,
                DecisionApprovalChange::Revoke => serde_json::to_value(
                    service
                        .revoke_pending_decision_approval(&principal.owner_id, &session_id, decision_id)
                        .await?,
                )?,
            };
            Ok::<_, anyhow::Error>(receipt)
        }))
        .await;
    match result {
        Ok(receipt) => Json(receipt).into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "Decision approval change failed");
            json_error(
                StatusCode::CONFLICT,
                "Decision approval is unavailable, expired, or already resolved",
            )
        }
    }
}

pub async fn post_session_message(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    request: Request,
) -> impl IntoResponse {
    let Some(api_principal) = request.extensions().get::<super::ApiPrincipal>().cloned() else {
        return json_error(StatusCode::UNAUTHORIZED, "Authenticated API principal missing");
    };
    let turn_runtime = state.pin_turn_runtime();
    let headers = request.headers().clone();
    let async_mode = jobs::wants_async(request.uri().query(), &headers);
    let message = match parse_message_from_request(&state, request).await {
        Ok(message) => message,
        Err(error_response) => return error_response,
    };

    submit_console_message(
        state,
        session_id,
        api_principal,
        turn_runtime,
        message,
        async_mode,
        ConsoleTurnMode::Message,
    )
    .await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConsoleTurnMode {
    Message,
    Resume,
}

#[cfg_attr(
    test,
    expect(
        clippy::large_enum_variant,
        reason = "Test-only source variant must not add indirection to production admission"
    )
)]
enum ConsoleTurnSource {
    #[cfg(test)]
    Message(crate::gateway::decision_source::GatewayTurnProof),
    Admitted {
        mode: ConsoleTurnMode,
        proof: crate::gateway::decision_source::GatewayTurnProof,
        guard: crate::gateway::decision_source::GatewayAdmissionGuard,
    },
}

struct ConsolePendingInput {
    state: AppState,
    runtime: std::sync::Arc<crate::gateway::TurnRuntimeGeneration>,
    principal: super::ApiPrincipal,
    envelope: RuntimeEnvelope,
    ticket: crate::runtime::turn_cleanup::CleanupTicket,
}

impl ConsolePendingInput {
    async fn record(
        self,
        message: String,
    ) -> anyhow::Result<(
        crate::gateway::decision_source::GatewayTurnProof,
        crate::gateway::decision_source::GatewayAdmissionGuard,
    )> {
        let run = self
            .envelope
            .run_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Console input run is missing"))?;
        let key = format!("console:{run}:input");
        let recording = self.runtime.config_generation.effective.memory.event_recording_config();
        let fabric = MemoryFabric::new(self.state.mem.clone(), self.envelope.workspace_id.clone())
            .with_event_recording(recording)
            .with_ingress_recovery(&self.runtime.config_generation.effective, &self.ticket)
            .await?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut ticket = Some(self.ticket);
            let mut admission = None;
            let operation = async {
                let event = fabric
                    .record_inbound_user_message(
                        self.envelope.message_scope(),
                        message.clone(),
                        Some(key.clone()),
                        None,
                    )
                    .await?;
                anyhow::ensure!(
                    event.id <= 0
                        || (event.idempotency_key.as_deref() == Some(key.as_str())
                            && event.workspace_id == self.envelope.workspace_id
                            && event.owner_id.as_deref() == Some(self.principal.owner_id.as_str())),
                    "Console input identity changed during persistence"
                );
                let event_id = if event.id > 0 { event.event_id.as_str() } else { "" };
                let proof = crate::gateway::decision_source::GatewaySourceReceipt::console_proof(
                    self.state.mem.as_ref(),
                    &self.runtime,
                    &self.principal,
                    &self.envelope,
                    &message,
                    event_id,
                )
                .await?;
                anyhow::ensure!(
                    event.id <= 0
                        || proof.envelope().source_message_event_id.as_deref() == Some(event.event_id.as_str()),
                    "Console durable receipt lost its original input"
                );
                admission = Some(crate::gateway::decision_source::GatewayAdmissionGuard::new(
                    &proof,
                    fabric.clone(),
                    ticket
                        .take()
                        .ok_or_else(|| anyhow::anyhow!("Console input cleanup ticket missing"))?,
                    None,
                ));
                if recording.enabled && recording.record_user_messages {
                    if self.runtime.config_generation.effective.causal_tree.enabled {
                        self.state.mem.project_console_message(&event).await?;
                    } else {
                        self.state
                            .mem
                            .append_conversation_turn(
                                &self.envelope.session_key,
                                self.envelope.channel.as_deref().unwrap_or("console"),
                                self.envelope.sender.as_deref().unwrap_or("console-user"),
                                "user",
                                &message,
                                None,
                                Some(&event.event_id),
                                Some(&self.envelope.resolved_owner_id()),
                            )
                            .await?;
                    }
                }
                Ok::<_, anyhow::Error>(proof)
            };
            let result = tokio::time::timeout(std::time::Duration::from_secs(15), operation)
                .await
                .map_err(|_| anyhow::anyhow!("Console input persistence timed out"))
                .and_then(std::convert::identity);
            let result = match result {
                Ok(proof) => admission
                    .take()
                    .map(|guard| (proof, guard))
                    .ok_or_else(|| anyhow::anyhow!("Console input owner did not issue a guard")),
                Err(_error) if fabric.ingress_already_accepted() => {
                    if let Some(ticket) = ticket.take() {
                        ticket.finish(true);
                    }
                    Err(crate::runtime::ingress_recovery::AlreadyAccepted.into())
                }
                Err(error) => {
                    if admission.is_none() {
                        let recovery = async {
                            let event = self
                                .state
                                .mem
                                .find_owned_message_event_by_idempotency_key(
                                    &self.envelope.workspace_id,
                                    &self.principal.owner_id,
                                    &key,
                                )
                                .await?
                                .ok_or_else(|| anyhow::anyhow!("Console input commit is unconfirmed"))?;
                            anyhow::ensure!(
                                event.id > 0
                                    && !event.event_id.is_empty()
                                    && event.idempotency_key.as_deref() == Some(key.as_str())
                                    && event.workspace_id == self.envelope.workspace_id
                                    && event.owner_id.as_deref() == Some(self.principal.owner_id.as_str()),
                                "Console recovery input identity changed"
                            );
                            let proof = crate::gateway::decision_source::GatewaySourceReceipt::console_proof(
                                self.state.mem.as_ref(),
                                &self.runtime,
                                &self.principal,
                                &self.envelope,
                                &message,
                                &event.event_id,
                            )
                            .await?;
                            anyhow::ensure!(
                                proof.envelope().source_message_event_id.as_deref() == Some(event.event_id.as_str()),
                                "Console recovery input is not durable"
                            );
                            Ok::<_, anyhow::Error>(proof)
                        };
                        if let Ok(Ok(proof)) = tokio::time::timeout(std::time::Duration::from_secs(15), recovery).await
                            && let Some(ticket) = ticket.take()
                        {
                            admission = Some(crate::gateway::decision_source::GatewayAdmissionGuard::new(
                                &proof, fabric, ticket, None,
                            ));
                        }
                    }
                    if let Some(mut guard) = admission {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(15),
                            guard.finish_failure(sender.is_closed(), "Console input admission failed"),
                        )
                        .await;
                    } else if let Some(ticket) = ticket {
                        ticket.finish(false);
                    }
                    Err(error)
                }
            };
            // A disconnected HTTP request drops the guard carried by this
            // response, retaining cleanup ownership even before job launch.
            let _ = sender.send(result);
        });
        receiver
            .await
            .map_err(|_| anyhow::anyhow!("Console input owner ended without a receipt"))?
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DecisionResumeRequest {
    Stored(StoredDecisionResumeRequest),
    Explicit(ExplicitDecisionResumeRequest),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDecisionResumeRequest {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExplicitDecisionResumeRequest {
    tool: String,
    arguments: serde_json::Value,
}

pub(super) async fn post_decision_resume(
    State(state): State<AppState>,
    Path((session_id, decision_id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let Some(principal) = request.extensions().get::<super::ApiPrincipal>().cloned() else {
        return json_error(StatusCode::UNAUTHORIZED, "Authenticated API principal missing");
    };
    let decision_id = match crate::causal_tree::ids::DecisionId::parse(&decision_id) {
        Ok(id) => id,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "Invalid decision id"),
    };
    let runtime = state.pin_turn_runtime();
    if !runtime.config_generation.effective.causal_tree.enabled {
        return json_error(StatusCode::CONFLICT, "Decision approvals are disabled");
    }
    if let Err(message) = console_decision_control_policy(&runtime.config_generation.effective) {
        return json_error(StatusCode::FORBIDDEN, message);
    }
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cleanup =
        match crate::runtime::turn_cleanup::CleanupScope::new(Some(&runtime.decision_slot.cleanup), cancellation) {
            Ok(cleanup) => cleanup,
            Err(_) => return json_error(StatusCode::SERVICE_UNAVAILABLE, "Gateway is draining"),
        };
    let authorization = cleanup
        .run(Box::pin(async {
            let service = runtime.decision_slot.publish(&runtime, &state).await?;
            authorize_console_decision_control(
                &service,
                &principal,
                &session_id,
                decision_id,
                ConsoleDecisionControl::Resume,
            )
            .await
        }))
        .await;
    if authorization.is_err() {
        return json_error(StatusCode::CONFLICT, "Decision resume is unavailable or not permitted");
    }
    let async_mode = jobs::wants_async(request.uri().query(), request.headers());
    let Json(body) = match Json::<DecisionResumeRequest>::from_request(request, &state).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let message = match body {
        DecisionResumeRequest::Stored(_) => serde_json::json!({
            "decision_id": decision_id.to_string(),
        }),
        DecisionResumeRequest::Explicit(body) => serde_json::json!({
            "decision_id": decision_id.to_string(),
            "tool": body.tool,
            "arguments": body.arguments,
        }),
    }
    .to_string();
    submit_console_message(
        state,
        session_id,
        principal,
        runtime,
        message,
        async_mode,
        ConsoleTurnMode::Resume,
    )
    .await
}

async fn submit_console_message(
    state: AppState,
    session_id: String,
    api_principal: super::ApiPrincipal,
    turn_runtime: std::sync::Arc<crate::gateway::TurnRuntimeGeneration>,
    message: String,
    async_mode: bool,
    mode: ConsoleTurnMode,
) -> Response {
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
    let runtime_envelope = console_runtime_envelope_for_workspace(
        turn_runtime
            .config_generation
            .effective
            .workspace_dir
            .to_string_lossy()
            .to_string(),
        &session_id,
        &session.channel,
        &session.sender,
    )
    .with_owner_id(api_principal.owner_id.clone())
    .with_owner_isolation()
    .with_run_id(uuid::Uuid::new_v4().to_string())
    .with_config_generation(&turn_runtime.config_generation);

    if let Err(error) = recover_console_history(&state, &turn_runtime, &api_principal, &session_id).await {
        tracing::error!(error = %error, "Console history recovery failed");
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Console history recovery is incomplete",
        );
    }
    let principal = operator_history_principal(&api_principal, &runtime_envelope);
    let turns = match state
        .mem
        .list_console_conversation_turns(&principal, &session_id, MAX_PAGE_LIMIT, 0)
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

    let input_cancellation = tokio_util::sync::CancellationToken::new();
    let ticket = match turn_runtime.decision_slot.cleanup.register(input_cancellation) {
        Ok(ticket) => ticket,
        Err(_) => return json_error(StatusCode::SERVICE_UNAVAILABLE, "Console input admission is closed"),
    };
    let pending = ConsolePendingInput {
        state: state.clone(),
        runtime: std::sync::Arc::clone(&turn_runtime),
        principal: api_principal,
        envelope: runtime_envelope,
        ticket,
    };
    let (proof, mut guard) = match pending.record(message.clone()).await {
        Ok(admission) => admission,
        Err(error) if error.is::<crate::runtime::ingress_recovery::AlreadyAccepted>() => {
            return json_error(
                StatusCode::CONFLICT,
                "Input already accepted; execution was not repeated and outcome remains unconfirmed",
            );
        }
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Console input persistence is unconfirmed",
            );
        }
    };
    // Establish monitor ownership synchronously, before constructing work that
    // cancellation or registry abortion can drop without ever polling.
    let (cleanup_sender, cleanup_receiver) = tokio::sync::oneshot::channel();
    if guard.attach_acknowledgement(cleanup_sender).is_err() {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Console input cleanup ownership is unconfirmed",
        );
    }
    let source = ConsoleTurnSource::Admitted { mode, proof, guard };

    // Everything up to here is bounded bookkeeping. What follows is the agent
    // turn itself, which has no upper bound — tool chains, sub-agents, and
    // compaction can legitimately run for hours, and PRX no longer cuts them off
    // on a clock. Awaiting it inside this handler would tie the turn's lifetime
    // to the HTTP connection, so a closed tab or a proxy hang-up would destroy a
    // turn mid-flight, after its side effects had already begun. It therefore
    // runs as a detached job: visible in `prx tasks list`, killable by id, and
    // still there when the client comes back.
    let job = jobs::submit_with_cleanup(
        jobs::KIND_SESSION_MESSAGE,
        format!("gateway:session_message:{session_id}"),
        run_console_message_job(turn_runtime, state, message, turns, source),
        Some(cleanup_receiver),
    );

    if async_mode {
        return (StatusCode::ACCEPTED, Json(job.accepted_body())).into_response();
    }
    job.wait().await.into_response()
}

/// The unbounded half of `POST /sessions/{id}/message`, run as a job.
///
/// Returns the exact `(status, body)` the synchronous handler used to produce,
/// so waiting clients see no contract change and polling clients see the same
/// payload under `result`.
fn run_console_message_job(
    turn_runtime: std::sync::Arc<crate::gateway::TurnRuntimeGeneration>,
    state: AppState,
    message: String,
    turns: Vec<crate::memory::ConversationTurn>,
    source: ConsoleTurnSource,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<jobs::JobOutput, String>> + Send>> {
    // Keep the owned Console work out of the generic job submitter's stack.
    // Construction does not poll it; the same input guard retains its sender.
    Box::pin(run_console_message_job_work(
        turn_runtime,
        state,
        message,
        turns,
        source,
    ))
}

async fn run_console_message_job_work(
    turn_runtime: std::sync::Arc<crate::gateway::TurnRuntimeGeneration>,
    state: AppState,
    message: String,
    turns: Vec<crate::memory::ConversationTurn>,
    source: ConsoleTurnSource,
) -> Result<jobs::JobOutput, String> {
    let (mode, source_proof, input_guard) = match source {
        #[cfg(test)]
        ConsoleTurnSource::Message(proof) => (ConsoleTurnMode::Message, proof, None),
        ConsoleTurnSource::Admitted { mode, proof, guard } => (mode, proof, Some(guard)),
    };
    let runtime_envelope = source_proof.envelope().clone();
    let cancellation =
        jobs::current_cancellation().map_or_else(tokio_util::sync::CancellationToken::new, |token| token.child_token());
    let _cancel_guard = cancellation.clone().drop_guard();
    let config = &turn_runtime.config_generation.effective;
    let cleanup_ticket = turn_runtime
        .decision_slot
        .cleanup
        .register(cancellation.clone())
        .map_err(|_| "Console turn admission is closed".to_owned())?;
    // Register the job's cancellation/acknowledgement owner before releasing
    // the HTTP input ticket. This transfer has no await or unowned interval.
    let inherited_fabric = input_guard.as_ref().map(|guard| guard.ingress_fabric());
    let admission_acknowledgement = if let Some(input_guard) = input_guard {
        let (input_ticket, acknowledgement) = input_guard
            .into_terminal_parts()
            .map_err(|_| "Console input ownership transfer failed".to_owned())?;
        input_ticket.finish(true);
        acknowledgement
    } else {
        jobs::register_terminal_cleanup()
    };
    let admission_fabric = inherited_fabric.unwrap_or_else(|| {
        MemoryFabric::new(state.mem.clone(), runtime_envelope.workspace_id.clone())
            .with_event_recording(config.memory.event_recording_config())
    });
    let mut admission_guard = crate::gateway::decision_source::GatewayAdmissionGuard::new(
        &source_proof,
        admission_fabric,
        cleanup_ticket,
        admission_acknowledgement,
    );
    // Prediction transport accounting and later work retain the same owner.
    // Register its monitor acknowledgement before the first admission await.
    let child_cleanup = match crate::runtime::turn_cleanup::CleanupScope::new(
        Some(&turn_runtime.decision_slot.cleanup),
        cancellation.child_token(),
    ) {
        Ok(scope) => scope.with_acknowledgement(jobs::register_terminal_cleanup()),
        Err(_) => {
            let error = "Console child cleanup admission is closed";
            admission_guard
                .finish_failure(cancellation.is_cancelled(), error)
                .await
                .map_err(|_| "Console admission failure persistence is unconfirmed".to_owned())?;
            return Err(error.to_owned());
        }
    };
    let decision = {
        let proof = &source_proof;
        let admission = child_cleanup
            .scope_admission(Box::pin(async {
                let admitted: Result<_, String> = async {
                    proof
                        .verify_runtime_input(&turn_runtime, &message)
                        .map_err(|error| crate::providers::sanitize_api_error(&error.to_string()))?;
                    let service = turn_runtime
                        .decision_slot
                        .publish(&turn_runtime, &state)
                        .await
                        .map_err(|_| "Console decision publication failed".to_owned())?;
                    let kind = crate::runtime::turn_decision::AgentTurnKind::SingleShot;
                    let context = match mode {
                        ConsoleTurnMode::Message => service.decide_agent_initial(proof, kind, &cancellation).await,
                        ConsoleTurnMode::Resume => service.decide_agent_resume(proof, kind, &cancellation).await,
                    }
                    .map_err(|error| crate::providers::sanitize_api_error(&error.to_string()))?;
                    Ok::<_, String>((service, context))
                }
                .await;
                admitted.map_err(anyhow::Error::msg)
            }))
            .await;
        let (service, context) = match admission {
            Ok(admitted) => admitted,
            Err(error) => {
                // No future is still polling admission. Close/drain its owned
                // attempts before acknowledging the failed input terminal.
                let admission_error = crate::providers::sanitize_api_error(&format!("{error:#}"));
                let drained = child_cleanup.run(Box::pin(async { Err::<(), _>(error) })).await;
                let error = match drained {
                    Err(error) if error.is::<crate::runtime::turn_cleanup::CleanupIncomplete>() => format!(
                        "{admission_error}; Console admission cleanup is unconfirmed: {}",
                        crate::providers::sanitize_api_error(&format!("{error:#}")),
                    ),
                    _ => admission_error,
                };
                if admission_guard
                    .finish_failure(cancellation.is_cancelled(), &error)
                    .await
                    .is_err()
                {
                    return Err(format!("{error}; Console admission failure persistence is unconfirmed"));
                }
                return Err(error);
            }
        };
        let provider_trace = if context.approval_collection_proof().is_some() {
            Some((
                std::sync::Arc::new(crate::agent::turn_contracts::RetainedProviderTrace::default()),
                std::sync::Arc::new(
                    console_route_decision(&turn_runtime, &runtime_envelope, &message)
                        .map_err(|error| error.to_string())?,
                ),
            ))
        } else {
            None
        };
        ConsoleDecisionTurn {
            service: std::sync::Arc::clone(service),
            provider_trace,
            context,
            proof,
            cancellation: cancellation.clone(),
        }
    };
    let (cleanup_ticket, admission_acknowledgement) = admission_guard
        .into_terminal_parts()
        .map_err(|error| error.to_string())?;
    let terminal_guard = crate::gateway::decision_runtime::GatewayTerminalGuard::new(
        cleanup_ticket,
        std::sync::Arc::clone(&decision.service),
        std::sync::Arc::clone(&decision.context),
        MemoryFabric::new(state.mem.clone(), runtime_envelope.workspace_id.clone()),
        std::sync::Arc::clone(config),
        runtime_envelope.clone(),
        admission_acknowledgement,
    )
    .map_err(|error| error.to_string())?;
    let terminal_guard = if let Some((trace, route)) = &decision.provider_trace {
        terminal_guard.with_provider_trace(std::sync::Arc::clone(trace), route.as_ref().clone())
    } else {
        terminal_guard
    };
    let work = async {
        match decision.context.disposition().map_err(|error| error.to_string())? {
            crate::runtime::turn_decision::TurnDisposition::Close { reply } => {
                let ports = crate::gateway::decision_runtime::GatewayDecisionClose {
                    resource: "gateway_api:sessions:assistant_message",
                    history_commit_len: turns.len() + 2,
                    terminal_guard: &terminal_guard,
                    service: &decision.service,
                    config,
                    envelope: &runtime_envelope,
                    started_at: chrono::Utc::now(),
                };
                decision
                    .service
                    .close_agent_directive_turn(
                        decision.proof,
                        std::sync::Arc::clone(&decision.context),
                        &ports,
                        &cancellation,
                    )
                    .await
                    .map_err(|error| crate::providers::sanitize_api_error(&error.to_string()))?;
                if !config.causal_tree.enabled {
                    append_console_turn(&state, &runtime_envelope, "assistant", &reply, false, false)
                        .await
                        .map_err(|_| "Failed to persist assistant response".to_owned())?;
                }
                // Enabled terminal rejoin already projected the transcript.
                return Ok(jobs::JobOutput::new(
                    StatusCode::OK,
                    serde_json::json!({ "status": "ok", "reply": reply }),
                ));
            }
            crate::runtime::turn_decision::TurnDisposition::Run => {}
            crate::runtime::turn_decision::TurnDisposition::CollectApprovalArguments => {
                if mode != ConsoleTurnMode::Message || decision.provider_trace.is_none() {
                    return Err("Console collection requires the owning main-turn trace".to_owned());
                }
            }
        }
        if mode == ConsoleTurnMode::Resume {
            crate::gateway::authorize_gateway_config_resource_mutation(
                config,
                "gateway_api:sessions:assistant_message",
                ResourceRiskLevel::Low,
            )
            .map_err(|_| "Console resume history authorization denied".to_owned())?;
            return run_console_resume(&decision, &terminal_guard, turns.len() + 2, &cancellation)
                .await
                .map_err(|error| crate::providers::sanitize_api_error(&error.to_string()));
        }
        let turn = match run_console_runtime_turn(
            std::sync::Arc::clone(&turn_runtime),
            &state,
            &message,
            turns,
            &decision,
        )
        .await
        {
            Ok(turn) => turn,
            Err(error) => {
                // A stable HTTP error response still represents a failed turn.
                // Finalize it before returning a successful job transport result.
                {
                    terminal_guard
                        .finish_failure(&error, cancellation.is_cancelled())
                        .await
                        .map_err(|_| "Console terminal cleanup persistence is unconfirmed".to_owned())?;
                }
                if let Some(tools) = crate::providers::tool_schema_refused_tools(&error) {
                    // A configuration problem, not a failed job: the caller
                    // gets a stable code with every refused tool, and the log
                    // keeps the whole refusal, unclipped.
                    tracing::error!(
                        error = %crate::security::redaction::redact_secrets(&format!("{error:#}")),
                        "Console turn refused before transport: the provider cannot accept some offered tool schemas"
                    );
                    return Ok(jobs::JobOutput::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        serde_json::json!({
                            "error": "tool_schema_refused",
                            "tools": tools,
                            "message": "The configured provider cannot accept some of the tool schemas this turn \
                                        offered, so nothing was sent. The gateway log names the configuration \
                                        to change.",
                        }),
                    ));
                }
                return Err(crate::providers::sanitize_api_error(&error.to_string()));
            }
        };
        let reply = turn.reply.clone();

        if let Err((status, body)) = crate::gateway::authorize_gateway_config_resource_mutation(
            config,
            "gateway_api:sessions:assistant_message",
            ResourceRiskLevel::Low,
        ) {
            {
                terminal_guard
                    .finish_error(&anyhow::anyhow!("Console assistant persistence authorization denied"))
                    .await
                    .map_err(|_| "Console terminal cleanup persistence is unconfirmed".to_owned())?;
            }
            return Ok(jobs::JobOutput::new(status, body.0));
        }

        let provider_outcome = crate::agent::terminal::provider_outcome_from_trace(
            &turn.route_decision,
            turn.provider_started_at,
            turn.trace,
        );
        let terminal_id = turn
            .envelope
            .run_id
            .clone()
            .ok_or_else(|| "Console admitted run missing".to_owned())?;
        let terminal_fabric = MemoryFabric::new(state.mem.clone(), turn.envelope.workspace_id.clone());
        let config = &turn_runtime.config_generation.effective;
        let settlement = decision
            .service
            .terminal_settlement_ref(std::sync::Arc::clone(&decision.context))
            .map_err(|error| error.to_string())?;
        match crate::gateway::decision_runtime::commit_terminal(
            Some(&terminal_guard),
            &terminal_fabric,
            crate::agent::terminal::TurnTerminalDraft {
                decision: Some(settlement),
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
                    summary: if turn.awaiting_approval {
                        "console turn awaiting human approval"
                    } else {
                        "console turn completed"
                    }
                    .to_string(),
                    started_at: turn.provider_started_at,
                    finished_at: chrono::Utc::now(),
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
            },
            config,
        )
        .await
        {
            Ok(_) => (),
            Err(error) => {
                tracing::warn!(error = %error, "Failed to commit shared console terminal event");
                return Err("Console terminal persistence is unconfirmed".to_owned());
            }
        };

        if !config.causal_tree.enabled {
            if let Err(error) = append_console_turn(&state, &turn.envelope, "assistant", &reply, false, false).await {
                tracing::error!("Failed to persist assistant session turn: {error}");
                return Err("Failed to persist assistant response".to_string());
            }
        }

        let body = if turn.awaiting_approval {
            serde_json::json!({
                "status": "awaiting_approval",
                "reply": reply,
                "decision_id": decision.context.decision_id().to_string(),
            })
        } else {
            serde_json::to_value(SendMessageResponse {
                status: "ok".to_string(),
                reply,
            })
            .map_err(|error| format!("Failed to encode session reply: {error}"))?
        };
        Ok(jobs::JobOutput::new(StatusCode::OK, body))
    };
    let result = child_cleanup
        .run(Box::pin(async {
            let result: Result<jobs::JobOutput, String> = work.await;
            result.map_err(anyhow::Error::msg)
        }))
        .await
        .map_err(|error| crate::providers::sanitize_api_error(&error.to_string()));
    if let Err(error) = &result {
        terminal_guard
            .finish_failure(&anyhow::anyhow!(error.clone()), cancellation.is_cancelled())
            .await
            .map_err(|_| "Console terminal cleanup persistence is unconfirmed".to_owned())?;
    }
    result
}

async fn run_console_resume(
    decision: &ConsoleDecisionTurn<'_>,
    guard: &crate::gateway::decision_runtime::GatewayTerminalGuard,
    history_commit_len: usize,
    cancellation: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<jobs::JobOutput> {
    use crate::agent::terminal::{TurnTerminalDraft, TurnTerminalStatus};
    use crate::tools::ToolExecutionStatus;
    let envelope = decision.proof.envelope();
    let started_at = Utc::now();
    let outcome = decision
        .service
        .execute_agent_resume(
            decision.proof,
            &decision.context,
            &crate::tools::ToolExecutionContext::new(envelope.clone(), "private").with_chat_id(&envelope.session_key),
            cancellation,
        )
        .await?;
    let (status, reply) = match outcome.status {
        ToolExecutionStatus::Succeeded => (
            TurnTerminalStatus::Completed,
            crate::security::redaction::redact_secrets(&outcome.model_content),
        ),
        ToolExecutionStatus::Cancelled => (
            TurnTerminalStatus::Cancelled,
            "Approved operation was cancelled.".to_owned(),
        ),
        _ => (
            TurnTerminalStatus::Failed,
            "Approved operation could not be completed.".to_owned(),
        ),
    };
    guard
        .commit(TurnTerminalDraft {
            decision: Some(
                decision
                    .service
                    .settlement_ref(std::sync::Arc::clone(&decision.context))?,
            ),
            terminal_id: envelope
                .run_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Resume run missing"))?,
            scope: envelope.message_scope(),
            status,
            history: Some(crate::agent::terminal::TurnHistoryProjection {
                assistant_content: reply.clone(),
                history_commit_len,
            }),
            history_scope: None,
            // The approved call runs directly. Decision-stage model usage is
            // already recorded by that stage; no provider turn is fabricated.
            provider_outcome: None,
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary: "console approved operation finished".to_owned(),
                started_at,
                finished_at: Utc::now(),
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
        })
        .await?;
    Ok(jobs::JobOutput::new(
        StatusCode::OK,
        serde_json::json!({
            "status": if status == TurnTerminalStatus::Completed { "ok" } else { "failed" },
            "execution_status": outcome.status,
            "reply": reply,
        }),
    ))
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
    mod collection;
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

    fn test_app_state(mut config: Config, provider: Arc<dyn Provider>) -> AppState {
        config.default_provider.get_or_insert_with(|| "ollama".to_owned());
        config.default_model.get_or_insert_with(|| "test-model".to_owned());
        config.causal_tree.enabled = false;
        test_app_state_with_config(config, provider)
    }

    fn test_app_state_with_config(config: Config, provider: Arc<dyn Provider>) -> AppState {
        let model = config.default_model.clone().expect("fixture config model");
        static PARTICIPANT: std::sync::OnceLock<Arc<dyn crate::config::ConfigGenerationParticipant>> =
            std::sync::OnceLock::new();
        let config = crate::config::new_shared(config);
        config.register_participant(PARTICIPANT.get_or_init(|| Arc::new(TestConfigParticipant)));
        AppState {
            config,
            provider,
            model,
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
            idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_mins(5), 1000)),
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
    async fn session_creation_route_binds_owner_and_creates_no_message() {
        use tower::ServiceExt;
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_owned();
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        let mut state = test_app_state(config.clone(), Arc::new(CapturingProvider::default()));
        let memory = Arc::new(crate::memory::SqliteMemory::new(temp.path()).unwrap());
        state.mem = memory.clone();
        let router = super::super::router(state.clone()).with_state(state.clone());
        let request = |body: &'static str| {
            axum::http::Request::builder()
                .method("POST")
                .uri("/sessions")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        };
        let response = router.clone().oneshot(request("{}")).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        let id = body.get("session_id").unwrap().as_str().unwrap();
        let stored = memory.get_conversation_session(id).await.unwrap().unwrap();
        assert_eq!(stored.message_count, 0);
        assert_eq!(stored.channel, "console");
        assert_eq!(stored.sender, "api:local");
        assert_eq!(stored.last_message_preview, "");
        assert!(memory.create_conversation_session(id, "api:other").await.is_err());
        assert_eq!(
            memory.get_conversation_session(id).await.unwrap().unwrap().sender,
            "api:local"
        );
        assert_eq!(
            router
                .oneshot(request(r#"{"owner_id":"api:other"}"#))
                .await
                .unwrap()
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        let mut denied = test_app_state(config, Arc::new(CapturingProvider::default()));
        denied.mem = memory.clone();
        let response = super::super::router(denied.clone())
            .with_state(denied)
            .oneshot(request("{}"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(memory.list_conversation_sessions(100, 0, None).await.unwrap().len(), 1);
    }

    fn console_provider_fixture_config(config: &mut Config, workspace: &std::path::Path, model: &str, endpoint: &str) {
        config.workspace_dir = workspace.to_owned();
        config.config_path = workspace.join("config.toml");
        config.default_provider = Some("ollama".to_owned());
        config.default_model = Some(model.to_owned());
        config.cost.prices.insert(
            format!("ollama/{model}"),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
        assert!(matches!(crate::llm::route_decision::usage_cost_resolution(
            "ollama", model, 1, 0, 0, 0, &config.cost,
        ), crate::llm::route_decision::UsagePrice::Priced(value) if value > 0.0));
        let native_tools = model == CONSOLE_WINDOW_MODEL;
        config.providers.models.push(
            toml::from_str(&format!(
                "provider = \"ollama\"\nmodel = {model:?}\noperations = [\"chat\"]\nnative_tools = {native_tools}\n"
            ))
            .unwrap(),
        );
        let selected = crate::providers::selection::check(
            config,
            "ollama",
            model,
            &if native_tools {
                crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools)
            } else {
                crate::providers::catalog::Requirement::chat()
            },
            crate::providers::catalog::UnknownPolicy::Invocation,
        )
        .unwrap();
        assert_eq!(
            selected.verification(),
            &crate::providers::catalog::Verification::Verified
        );
        crate::agent::agent::native_decision_fixture::configure(config, endpoint).unwrap();
    }

    async fn console_test_source(
        state: &mut AppState,
        envelope: RuntimeEnvelope,
        input: &str,
    ) -> (
        Arc<crate::gateway::TurnRuntimeGeneration>,
        RuntimeEnvelope,
        String,
        crate::gateway::decision_source::GatewayTurnProof,
    ) {
        if state.mem.name() == "test" {
            state.mem =
                Arc::new(crate::memory::SqliteMemory::new(&state.config.pin().effective.workspace_dir).unwrap());
        }
        let mut runtime = state.pin_turn_runtime();
        let config = &runtime.config_generation.effective;
        let provider_name = config.default_provider.as_deref().unwrap();
        let catalog = crate::providers::selection::catalog_for(config);
        let resolved = crate::providers::selection::check_with(
            config,
            &catalog,
            provider_name,
            &runtime.model,
            crate::providers::selection::credential_for(config, provider_name),
            &crate::providers::catalog::Requirement::chat(),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )
        .unwrap();
        let binding = crate::gateway::decision_source::GatewayProviderBinding::new(runtime.provider.clone(), resolved);
        let security = Arc::new(crate::security::SecurityPolicy::from_config(
            &config.autonomy,
            &config.workspace_dir,
        ));
        let mutable = Arc::get_mut(&mut runtime).unwrap();
        mutable.provider_binding = Some(Arc::new(binding));
        mutable.decision_slot = Arc::new(crate::gateway::decision_runtime::GatewayDecisionSlot::new(security, 0));
        let principal = super::super::ApiPrincipal::of_token("prx-test-token");
        let envelope = envelope
            .with_run_id(uuid::Uuid::new_v4().to_string())
            .with_config_generation(&runtime.config_generation)
            .with_owner_id(principal.owner_id.clone())
            .with_owner_isolation();
        let event_id = append_console_turn(state, &envelope, "user", input, true, false)
            .await
            .unwrap();
        let proof = crate::gateway::decision_source::GatewaySourceReceipt::console_proof(
            state.mem.as_ref(),
            &runtime,
            &principal,
            &envelope,
            input,
            &event_id,
        )
        .await
        .unwrap();
        (runtime, proof.envelope().clone(), event_id, proof)
    }

    #[tokio::test]
    async fn console_runtime_turn_uses_shared_tool_loop_history() {
        let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
            .await
            .unwrap();
        let endpoint = driver.endpoint().to_owned();
        driver
            .run(true, async move {
                let tmp = tempfile::tempdir().unwrap();
                let cleanup =
                    crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                        .unwrap();
                cleanup
                    .run(Box::pin(async {
                        let mut config = Config::default();
                        console_provider_fixture_config(&mut config, tmp.path(), "test-model", &endpoint);
                        let provider_impl = Arc::new(CapturingProvider::default());
                        let mut state = test_app_state_with_config(config, provider_impl.clone());
                        let envelope = console_runtime_envelope_for_workspace(
                            tmp.path().to_string_lossy().to_string(),
                            "console-session",
                            "console",
                            "prx_user",
                        );
                        let previous_turns = vec![ConversationTurn {
                            id: 1,
                            session_key: "console-session".to_string(),
                            role: "assistant".to_string(),
                            content: "previous answer".to_string(),
                            timestamp: "2026-05-27T00:00:00Z".to_string(),
                            message_id: Some("previous-event".to_string()),
                        }];

                        let (runtime, _, _, proof) =
                            console_test_source(&mut state, envelope, "current question").await;
                        let service = runtime.decision_slot.publish(&runtime, &state).await.unwrap().clone();
                        let cancellation = tokio_util::sync::CancellationToken::new();
                        let context = service
                            .decide_agent_initial(
                                &proof,
                                crate::runtime::turn_decision::AgentTurnKind::SingleShot,
                                &cancellation,
                            )
                            .await
                            .unwrap();
                        let decision = ConsoleDecisionTurn {
                            service: service.clone(),
                            context,
                            proof: &proof,
                            cancellation,
                            provider_trace: None,
                        };
                        assert!(
                            run_console_runtime_turn(
                                runtime.clone(),
                                &state,
                                "different unadmitted input",
                                Vec::new(),
                                &decision,
                            )
                            .await
                            .is_err()
                        );
                        assert!(
                            provider_impl.requests.lock().is_empty(),
                            "source mismatch must fail before provider transport"
                        );
                        let turn =
                            run_console_runtime_turn(runtime, &state, "current question", previous_turns, &decision)
                                .await
                                .unwrap();

                        service
                            .shutdown(std::time::Instant::now() + Duration::from_secs(2))
                            .await;
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
                        Ok::<(), anyhow::Error>(())
                    }))
                    .await
                    .expect("actual fixture decision transport and cleanup ownership");
            })
            .await;
    }

    /// A plain Console message uses the default enabled builtin decision plane
    /// and reaches the serving provider through the same admitted runtime as a
    /// production Console request.
    #[tokio::test]
    async fn console_plain_message_reaches_the_provider_with_default_causal_config() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
            .expect("test: Console cleanup owner");
        cleanup
            .run(Box::pin(async {
                let mut config = Config::default();
                config.workspace_dir = tmp.path().to_owned();
                config.config_path = tmp.path().join("config.toml");
                config.default_provider = Some("ollama".to_owned());
                config.default_model = Some("test-model".to_owned());
                config.providers.models.push(
                    toml::from_str(
                        "provider = \"ollama\"\nmodel = \"test-model\"\noperations = [\"chat\"]\nnative_tools = false\n",
                    )
                    .expect("test: Console model declaration"),
                );
                config.cost.prices.insert(
                    "ollama/test-model".to_owned(),
                    crate::config::schema::ModelPricing {
                        input: 1.0,
                        output: 1.0,
                        cache_write: 1.0,
                        cache_read: 1.0,
                    },
                );
                assert_eq!(
                    config.causal_tree,
                    crate::causal_tree::CausalTreeConfig::default(),
                    "the Console proof must retain the default causal configuration"
                );

                let provider_impl = Arc::new(CapturingProvider::default());
                let mut state = test_app_state_with_config(config, provider_impl.clone());
                let envelope = console_runtime_envelope_for_workspace(
                    tmp.path().to_string_lossy().to_string(),
                    "console-plain-session",
                    "console",
                    "prx_user",
                );
                let input = "hello";
                let (runtime, _, _, proof) = console_test_source(&mut state, envelope, input).await;
                let service = runtime.decision_slot.publish(&runtime, &state).await?.clone();
                let cancellation = tokio_util::sync::CancellationToken::new();
                let context = service
                    .decide_agent_initial(
                        &proof,
                        crate::runtime::turn_decision::AgentTurnKind::SingleShot,
                        &cancellation,
                    )
                    .await?;
                assert!(matches!(
                    context.directive(),
                    crate::causal_tree::directive::TurnExecutionDirective::AnswerDirectly
                ));
                let decision = ConsoleDecisionTurn {
                    service: Arc::clone(&service),
                    context,
                    proof: &proof,
                    cancellation,
                    provider_trace: None,
                };
                let turn = run_console_runtime_turn(runtime, &state, input, Vec::new(), &decision).await?;
                service
                    .shutdown(std::time::Instant::now() + Duration::from_secs(2))
                    .await;

                assert_eq!(turn.reply, "console-runtime-ok");
                assert_eq!(provider_impl.requests.lock().len(), 1);
                Ok::<(), anyhow::Error>(())
            }))
            .await
            .expect("test: default Console turn and cleanup complete");
    }

    // ── R5 / OQ-23: the Console turn budgets on the model's effective window ──

    const CONSOLE_BULKY_TOOL: &str = "file_read";
    const CONSOLE_BULKY_FILE: &str = "prx-console-window.txt";
    const CONSOLE_BULKY_MARKER: &str = "CONSOLE-BULKY";
    const CONSOLE_WINDOW_MODEL: &str = "narrow-window-model";
    const CONSOLE_WINDOW_PROVIDER: &str = "ollama";
    const CONSOLE_ROUTER_WINDOW: usize = 33_200;

    /// A read-only tool whose output overruns the narrow fixture window but
    /// fits the 128,000-token base window and the unbudgeted inline cap.
    struct ConsoleBulkyTool {
        inner: crate::tools::file_read::FileReadTool,
        executions: Arc<std::sync::atomic::AtomicUsize>,
        expected: String,
    }

    #[async_trait]
    impl crate::tools::Tool for ConsoleBulkyTool {
        fn name(&self) -> &str {
            self.inner.name()
        }
        fn description(&self) -> &str {
            self.inner.description()
        }
        fn parameters_schema(&self) -> serde_json::Value {
            self.inner.parameters_schema()
        }
        fn tier(&self) -> crate::tools::traits::ToolTier {
            self.inner.tier()
        }
        fn categories(&self) -> &'static [crate::tools::traits::ToolCategory] {
            self.inner.categories()
        }
        fn supports_owner_filesystem(&self) -> bool {
            self.inner.supports_owner_filesystem()
        }
        async fn execute_with_cancellation(
            &self,
            args: serde_json::Value,
            cancellation: Option<tokio_util::sync::CancellationToken>,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            self.executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let result = self.inner.execute_with_cancellation(args, cancellation).await?;
            assert!(result.success, "actual private FileRead must succeed: {result:?}");
            assert_eq!(
                result.output, self.expected,
                "the full physical file must reach budgeting"
            );
            Ok(result)
        }
        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
            self.execute_with_cancellation(args, None).await
        }
    }

    /// Native-tool spy: asks for the bulky tool on its first call, answers in
    /// text afterwards, and records every request it serves.
    #[derive(Default)]
    struct ToolRoundSpyProvider {
        requests: parking_lot::Mutex<Vec<Vec<ChatMessage>>>,
    }

    #[async_trait]
    impl Provider for ToolRoundSpyProvider {
        fn capabilities(&self) -> crate::providers::ProviderCapabilities {
            crate::providers::ProviderCapabilities {
                native_tool_calling: true,
                vision: false,
            }
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("summary".into())
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            let call_index = {
                let mut requests = self.requests.lock();
                requests.push(request.messages.to_vec());
                requests.len()
            };
            let tool_calls = if call_index == 1 {
                vec![crate::providers::ToolCall {
                    id: "call-console-bulky-1".into(),
                    name: CONSOLE_BULKY_TOOL.into(),
                    arguments: serde_json::json!({"path": CONSOLE_BULKY_FILE}).to_string(),
                }]
            } else {
                Vec::new()
            };
            Ok(ChatResponse {
                text: tool_calls.is_empty().then(|| "done".to_string()),
                tool_calls,
                reasoning_content: None,
            })
        }
    }

    /// Base config: the default 128,000-token window (not set explicitly), and
    /// a `[[router.models]]` entry that gives the served model a narrow window.
    /// Only the effective resolution sees the router entry. The fixture window
    /// is large enough for the sealed decision request's existing reservation;
    /// the paired payload remains larger than the main-turn share.
    fn narrow_router_window_config(workspace: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.workspace_dir = workspace.to_path_buf();
        config.default_provider = Some(CONSOLE_WINDOW_PROVIDER.to_string());
        config.default_model = Some(CONSOLE_WINDOW_MODEL.to_string());
        config.agent.compaction = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Off,
            ..crate::config::AgentCompactionConfig::default()
        };
        config.router.models = vec![crate::config::RouterModelConfig {
            model_id: CONSOLE_WINDOW_MODEL.to_string(),
            provider: CONSOLE_WINDOW_PROVIDER.to_string(),
            cost_per_million_tokens: 1.0,
            max_context: CONSOLE_ROUTER_WINDOW,
            reserved_output_tokens: Some(256),
            latency_ms: 1_000,
            categories: vec!["conversation".to_string()],
            elo_rating: 1_000.0,
        }];
        config.tool_tiering.always_include = vec![CONSOLE_BULKY_TOOL.to_string()];
        config
    }

    /// A Console turn budgets on the served model's effective window, the one
    /// the CLI, TUI and webhook budget on (OQ-23 / R5).
    ///
    /// The base config keeps the 128,000-token default, so a turn budgeted on
    /// the base window forwards the whole token-dense tool payload to the
    /// follow-up request. The router entry gives the served model a narrower
    /// window; budgeted on that, the follow-up must fit it.
    ///
    /// MUTATION GUARD: pass `Some(&config_snapshot.agent.compaction)` to the
    /// loop again and the follow-up request carries the whole payload.
    #[tokio::test]
    async fn console_turn_budgets_on_the_served_models_effective_window() {
        let driver = crate::agent::agent::native_decision_fixture::Driver::start("filesystem")
            .await
            .unwrap();
        let endpoint = driver.endpoint().to_owned();
        driver
            .run(true, async move {
                let tmp = tempfile::tempdir().expect("test: tempdir");
                let cleanup =
                    crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                        .unwrap();
                cleanup.run(Box::pin(async {
                let workspace = tmp.path().join("workspace");
                let private = tmp.path().join("private");
                std::fs::create_dir_all(&workspace).unwrap();
                std::fs::create_dir_all(&private).unwrap();
                let mut config = narrow_router_window_config(&workspace);
                console_provider_fixture_config(&mut config, &workspace, CONSOLE_WINDOW_MODEL, &endpoint);
                config.config_path = private.join("config.toml");
                let provider_impl = Arc::new(ToolRoundSpyProvider::default());
                // Keep the fixture's existing autonomy; owner isolation still gates
                // the actual registered private-filesystem backend independently.
                config.autonomy.level = crate::security::AutonomyLevel::Full;
                let mut state = test_app_state_with_config(config, provider_impl.clone());
                state.model = CONSOLE_WINDOW_MODEL.into();
                let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let mut payload = String::with_capacity(47_000);
                let mut index = 0usize;
                while payload.len() < 47_000 {
                    use std::fmt::Write as _;
                    let _ = write!(payload, "{CONSOLE_BULKY_MARKER}-{index}-갇걇겇곇괇굇궇귇긇깇꺇껇꼇꽇꾇꿇뀇끇낇냇넇녇놇뇇 ");
                    index += 1;
                }
                assert!(payload.len() < 48_000, "fixture must remain below the native inline cap");
                assert!(
                    payload.matches(CONSOLE_BULKY_MARKER).count() > 500,
                    "the base-window mutation must retain enough payload segments to fail"
                );
                assert!(
                    crate::agent::context_budget::count_tokens(&payload) > 31_000,
                    "the paired payload must overrun the narrow main-turn window"
                );
                let principal = super::super::ApiPrincipal::of_token("prx-test-token");
                let filesystem = crate::tools::owner_filesystem::OwnerFilesystem::open(
                    &private, workspace.to_string_lossy().as_ref(), &principal.owner_id, &workspace,
                ).unwrap();
                let target = filesystem.resolve(CONSOLE_BULKY_FILE).unwrap();
                filesystem.write_authorized(&target, payload.as_bytes()).unwrap();
                let security = Arc::new(crate::security::SecurityPolicy::from_config(
                    &state.config.pin().effective.autonomy, &workspace,
                ));
                state.tools_registry = Arc::new(vec![Box::new(ConsoleBulkyTool {
                    inner: crate::tools::file_read::FileReadTool::new(security, false),
                    executions: Arc::clone(&executions), expected: payload,
                })]);
                let envelope = console_runtime_envelope_for_workspace(
                    workspace.to_string_lossy().to_string(),
                    "console-window-session",
                    "console",
                    "prx_user",
                );

                // Tool publication is not selection authority. Name the actual
                // fixture capability in the raw source admitted by the service.
                const INPUT: &str = "file_read prx-console-window.txt: summarise the console bulky probe";
                let (runtime, _, _, proof) =
                    console_test_source(&mut state, envelope, INPUT).await;
                let service = runtime.decision_slot.publish(&runtime, &state).await.unwrap().clone();
                let cancellation = tokio_util::sync::CancellationToken::new();
                let (context, observed) = crate::causal_tree::decision_engine::original_observation::capture(
                    "console-owned-file-window-diagnostic",
                    service.decide_agent_initial(
                        &proof,
                        crate::runtime::turn_decision::AgentTurnKind::SingleShot,
                        &cancellation,
                    ),
                ).await.expect("bounded actual Console decision observation");
                let context = context.unwrap();
                let hops = context.audit_hops().collect::<Vec<_>>();
                let diagnostic = hops.iter().map(|hop| serde_json::json!({
                    "decision_id":hop.decision_id.to_string(),
                    "selection":format!("{:?}", hop.selection),
                    "fallback_cause":format!("{:?}", hop.fallback_cause),
                    "factory_prediction":format!("{:?}", hop.prediction),
                    "factory_attempts":hop.prediction.as_ref().map(|prediction| format!("{:?}", prediction.attempts)),
                    "request_compiled":hop.request_compiled,
                    "scored_count":hop.scored_count,
                    "candidates":format!("{:?}", hop.candidates),
                    "post_rehearsal":format!("{:?}", hop.post_rehearsal),
                    "pre_commit":format!("{:?}", hop.pre_commit),
                    "audit":format!("{:?}", hop.audit_trace),
                })).collect::<Vec<_>>();
                eprintln!("actual_console_window_hops={}", serde_json::to_string(&diagnostic).unwrap());
                let sidecar = observed.checked_sidecar(&hops);
                eprintln!("actual_console_window_original={sidecar:?}");
                sidecar.expect("original objects must match the actual returned hop inventory");
                let decision = ConsoleDecisionTurn {
                    service: service.clone(),
                    context,
                    proof: &proof,
                    cancellation,
                    provider_trace: None,
                };
                let turn = run_console_runtime_turn(
                    runtime,
                    &state,
                    INPUT,
                    Vec::new(),
                    &decision,
                )
                .await
                .expect("test: console turn completes");
                service
                    .shutdown(std::time::Instant::now() + Duration::from_secs(2))
                    .await;
                assert_eq!(turn.reply, "done");
                assert_eq!(
                    executions.load(std::sync::atomic::Ordering::SeqCst),
                    1,
                    "the full bulky payload must be physically produced once"
                );

                let requests = provider_impl.requests.lock().clone();
                assert_eq!(requests.len(), 2, "one tool round, then the answer");
                let follow_up = requests.get(1).expect("test: the follow-up request was recorded");
                let payload_occurrences: usize = follow_up
                    .iter()
                    .map(|message| message.content.matches(CONSOLE_BULKY_MARKER).count())
                    .sum();
                assert!(
                    payload_occurrences < 500,
                    "the follow-up request must be budgeted on the model's {CONSOLE_ROUTER_WINDOW}-token window, \
             not the 128,000-token base window; found {payload_occurrences} payload segments"
                );
                let follow_up_chars: usize = follow_up.iter().map(|message| message.content.chars().count()).sum();
                assert!(
                    follow_up_chars < CONSOLE_ROUTER_WINDOW * 4,
                    "the follow-up request must fit the effective window; it has {follow_up_chars} characters"
                );
                assert!(
                    follow_up.iter().any(|message| message.role == "user"
                        && message.content.contains("summarise the console bulky probe")),
                    "the turn's own request must survive the budget (D-011)"
                );
                    Ok::<(), anyhow::Error>(())
                })).await.expect("actual fixture decision transport and cleanup ownership");
            })
            .await;
    }

    /// C2b-2: the console session API returns a tool-schema refusal as a
    /// stable code with every refused tool. Before, the job failed with the
    /// aggregate clipped to 200 characters, which cut the tool list.
    #[tokio::test]
    async fn a_console_tool_schema_refusal_returns_a_stable_code_and_the_tools() {
        let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
            .await
            .unwrap();
        let endpoint = driver.endpoint().to_owned();
        driver
            .run(true, async move {
                let tmp = tempfile::tempdir().unwrap();
                let mut config = Config::default();
                console_provider_fixture_config(&mut config, tmp.path(), "test-model", &endpoint);
                let mut state = test_app_state_with_config(
                    config,
                    Arc::new(crate::providers::tool_schema_refusal_tests::RefusingProvider),
                );
                let envelope = console_runtime_envelope_for_workspace(
                    tmp.path().to_string_lossy().to_string(),
                    "console-session",
                    "console",
                    "prx_user",
                );

                let (runtime, _, _, proof) = console_test_source(&mut state, envelope, "hello").await;
                let output = match run_console_message_job(
                    runtime,
                    state,
                    "hello".to_string(),
                    Vec::new(),
                    ConsoleTurnSource::Message(proof),
                )
                .await
                {
                    Ok(output) => output,
                    Err(message) => {
                        panic!("the refusal must be answered with a stable code, not a failed job: {message}")
                    }
                };
                assert_eq!(output.status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
                assert_eq!(
                    output.body.get("error"),
                    Some(&serde_json::json!("tool_schema_refused")),
                    "{}",
                    output.body
                );
                assert_eq!(
                    output.body.get("tools"),
                    Some(&serde_json::json!(["fixture_conditional", "fixture_alternatives"])),
                    "{}",
                    output.body
                );
            })
            .await;
    }

    // ── D-053 / REDACT-TURN: console turns persist no provider secret ──

    /// Answers every structured chat with a reply that echoes a secret.
    struct SecretReplyProvider(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait]
    impl Provider for SecretReplyProvider {
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
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let secret = crate::agent::terminal::persistence_probe::PROBE_SECRET;
            Ok(ChatResponse {
                text: Some(format!("Here is the key {secret} you asked for")),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }
    }

    /// Fails every structured chat with an error that echoes a secret.
    struct SecretErrorProvider(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait]
    impl Provider for SecretErrorProvider {
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
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let secret = crate::agent::terminal::persistence_probe::PROBE_SECRET;
            anyhow::bail!("upstream rejected key {secret}");
        }
    }

    /// A console state whose memory is a real SQLite store and whose
    /// workspace (cost ledger, skills) is the same tempdir, so everything the
    /// turn persists lands under `tmp`. The user turn is recorded first, as
    /// `post_session_message` does, and its event id returned.
    async fn console_probe_state(
        tmp: &tempfile::TempDir,
        provider: Arc<dyn Provider>,
        session_id: &str,
        endpoint: &str,
    ) -> (
        AppState,
        Arc<crate::gateway::TurnRuntimeGeneration>,
        RuntimeEnvelope,
        String,
        crate::gateway::decision_source::GatewayTurnProof,
    ) {
        let mut config = Config::default();
        console_provider_fixture_config(&mut config, tmp.path(), "test-model", endpoint);
        let mut state = test_app_state_with_config(config, provider);
        state.mem = Arc::new(crate::memory::SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
        let envelope = console_runtime_envelope_for_workspace(
            tmp.path().to_string_lossy().to_string(),
            session_id,
            "console",
            "prx_user",
        );
        let (runtime, envelope, event_id, proof) = console_test_source(&mut state, envelope, "what is my key?").await;
        (state, runtime, envelope, event_id, proof)
    }

    /// D-053 / REDACT-TURN probe: drives `run_console_message_job` through the
    /// completed `finalize_turn` commit and the assistant `append_console_turn`
    /// with a reply that echoes a secret; it must fail before the REDACT-TURN commits.
    #[tokio::test]
    async fn console_persists_no_secret_from_a_reply() {
        let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
            .await
            .unwrap();
        let endpoint = driver.endpoint().to_owned();
        driver
            .run(true, async move {
                let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let tmp = tempfile::tempdir().expect("test: tempdir");
                let session_id = "console-probe-reply";
                let (state, runtime, envelope, _, proof) = console_probe_state(
                    &tmp,
                    Arc::new(SecretReplyProvider(Arc::clone(&calls))),
                    session_id,
                    &endpoint,
                )
                .await;
                let mem = Arc::clone(&state.mem);
                let principal = envelope.memory_principal();

                // Keep the actual writer/runtime alive through the full file scan.
                let retained_runtime = Arc::clone(&runtime);
                let service = runtime.decision_slot.publish(&runtime, &state).await.unwrap().clone();
                let output = run_console_message_job(
                    runtime,
                    state,
                    "what is my key?".to_string(),
                    Vec::new(),
                    ConsoleTurnSource::Message(proof),
                )
                .await
                .expect("test: the console turn completes");
                assert_eq!(output.status, axum::http::StatusCode::OK, "{}", output.body);

                let turns = mem
                    .list_conversation_turns(&principal, session_id, 50, 0)
                    .await
                    .expect("test: list the console turns");
                assert!(
                    turns
                        .iter()
                        .any(|turn| turn.role == "assistant" && turn.content.contains("[REDACTED]")),
                    "the reply must be persisted as a redacted assistant turn: {turns:?}"
                );

                assert_eq!(
                    calls.load(std::sync::atomic::Ordering::SeqCst),
                    1,
                    "the controlled provider must actually run once"
                );
                retained_runtime
                    .decision_slot
                    .cleanup
                    .drain(tokio::time::Instant::now() + Duration::from_secs(5))
                    .await
                    .expect("Console cleanup must drain before persisted-file inspection");
                assert!(
                    service.drain_writes(Instant::now() + Duration::from_secs(5)).await,
                    "actual decision writes must drain before persisted-file inspection"
                );
                crate::agent::terminal::persistence_probe::assert_nothing_under_persists_the_probe_secret(tmp.path());
                service.shutdown(Instant::now() + Duration::from_secs(2)).await;
                let retirement = Arc::clone(&retained_runtime.decision_slot.cleanup);
                drop(retained_runtime);
                retirement
                    .drain(tokio::time::Instant::now() + Duration::from_secs(5))
                    .await
                    .expect("Console runtime retirement must drain before scratch removal");
            })
            .await;
    }

    /// D-053 / REDACT-TURN probe: drives `run_console_message_job` into the
    /// failed `finalize_turn` commit of `run_console_runtime_turn` with a provider
    /// error that echoes a secret; it must fail before the REDACT-TURN commits.
    #[tokio::test]
    async fn console_persists_no_secret_from_a_provider_error() {
        let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
            .await
            .unwrap();
        let endpoint = driver.endpoint().to_owned();
        driver
            .run(true, async move {
                let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let tmp = tempfile::tempdir().expect("test: tempdir");
                let (state, runtime, _, _, proof) = console_probe_state(
                    &tmp,
                    Arc::new(SecretErrorProvider(Arc::clone(&calls))),
                    "console-probe-error",
                    &endpoint,
                )
                .await;

                // Keep the actual writer/runtime alive through the full file scan.
                let retained_runtime = Arc::clone(&runtime);
                let service = runtime.decision_slot.publish(&runtime, &state).await.unwrap().clone();
                let result = run_console_message_job(
                    runtime,
                    state,
                    "what is my key?".to_string(),
                    Vec::new(),
                    ConsoleTurnSource::Message(proof),
                )
                .await;
                assert!(
                    result.is_err(),
                    "the provider failure must fail the console job, not answer it"
                );

                assert_eq!(
                    calls.load(std::sync::atomic::Ordering::SeqCst),
                    1,
                    "the controlled provider must actually run once"
                );
                retained_runtime
                    .decision_slot
                    .cleanup
                    .drain(tokio::time::Instant::now() + Duration::from_secs(5))
                    .await
                    .expect("Console cleanup must drain before persisted-file inspection");
                assert!(
                    service.drain_writes(Instant::now() + Duration::from_secs(5)).await,
                    "actual decision writes must drain before persisted-file inspection"
                );
                crate::agent::terminal::persistence_probe::assert_nothing_under_persists_the_probe_secret(tmp.path());
                service.shutdown(Instant::now() + Duration::from_secs(2)).await;
                let retirement = Arc::clone(&retained_runtime.decision_slot.cleanup);
                drop(retained_runtime);
                retirement
                    .drain(tokio::time::Instant::now() + Duration::from_secs(5))
                    .await
                    .expect("Console runtime retirement must drain before scratch removal");
            })
            .await;
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

    /// D2 / fix 3 regression: `post_config` merges the incoming delta onto the hot
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
            axum::extract::Query(crate::gateway::api::config::ConfigWriteQuery::default()),
            axum::http::HeaderMap::new(),
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

    /// D2 / fix 1: the `/api/config/reload` route builds its OWN authorization gate from
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
        // config", which is the whole point of the D2 / fix 1 change.
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
