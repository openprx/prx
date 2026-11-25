use super::{AppState, extract_resource_auth_token};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::runtime_prompt::build_runtime_system_prompt;
use crate::agent::turn_contracts::ScopeContext;
use crate::agent::turn_engine::run_turn;
use crate::gateway::jobs;
use crate::memory::MemoryFabric;
use crate::observability::NoopObserver;
#[cfg(test)]
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

pub(super) use rf_delete::{post_batch_delete, post_message_delete, post_reply_delete};

pub(super) use rf_message::{create_route_auth, message_route_auth};

pub(super) use rf_replay::{post_run_cancel, rf_job_cancel};


pub(super) use target::TARGET_EXCLUDED_TOOLS;
use target::{
    AgentField, SelectedTurn, TargetError, TargetUnavailable, TargetedTurn, TurnTarget, console_owner_isolated,
    prepare_target, resolve_incarnation, resolve_target,
};

#[derive(Serialize)]
pub(super) struct SessionSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<serde_json::Value>,
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
    /// The agent the row belongs to (`main` or a name), on a caller-keyed
    /// session only; a Console session's body keeps its previous shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
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
    /// On a caller-keyed session: only the rows of this agent (`main` or a
    /// configured name).
    agent: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ApprovalInboxQuery {
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct SendMessageRequest {
    message: String,
    /// Present-or-absent is kept apart from `null`: an omitted field is Main,
    /// an explicit `null` is refused.
    #[serde(default, deserialize_with = "present_value")]
    agent: Option<serde_json::Value>,
}

fn present_value<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_json::Value::deserialize(deserializer).map(Some)
}

/// The files one multipart request creates. They are removed when the guard
/// drops (a parse error, a refused target, a failed admission, a cancelled
/// request) unless [`UploadGuard::release`] handed them over.
///
/// Ownership precedes creation: a path is registered with [`UploadGuard::begin`]
/// before the file exists. Every path carries a per-request uuid, so it can only
/// ever name a file this request creates. Creation runs in a blocking worker that
/// outlives a cancelled future, so the guard also publishes an `abandoned` flag
/// that the worker checks right after creating the file: whichever of the
/// worker's create and the guard's drop comes last removes the file.
#[derive(Default)]
struct UploadGuard {
    paths: Vec<PathBuf>,
    abandoned: std::sync::Arc<std::sync::atomic::AtomicBool>,
    released: bool,
}

impl UploadGuard {
    /// Register `path` for cleanup before it is created, and return the flag
    /// the creating worker must hand to [`create_upload_file`].
    fn begin(&mut self, path: PathBuf) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.paths.push(path);
        std::sync::Arc::clone(&self.abandoned)
    }

    /// Settle the outcome of creating `path`. A failed create (including
    /// `AlreadyExists`) means this request did not create the file, so the path
    /// is unregistered and the guard can never unlink a file it does not own.
    fn settle(&mut self, path: &StdPath, created: std::io::Result<std::fs::File>) -> std::io::Result<std::fs::File> {
        if created.is_err() {
            self.paths.retain(|registered| registered != path);
        }
        created
    }

    /// The request was admitted: the files now belong to the turn.
    fn release(mut self) {
        self.released = true;
        self.paths.clear();
    }
}

impl Drop for UploadGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        // Publish the abandonment before removing, so a worker that creates its
        // file after the removal below sees the flag and removes it itself.
        self.abandoned.store(true, std::sync::atomic::Ordering::SeqCst);
        for path in self.paths.drain(..) {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                // Never created, or already removed by its worker.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(error = %error, "Failed to remove an upload of an unadmitted request");
                }
            }
        }
    }
}

/// Exclusively create an upload file. If the owning guard was abandoned
/// meanwhile (the request was cancelled while this worker was running), the
/// file is removed again instead of being orphaned.
fn create_upload_file(path: &StdPath, abandoned: &std::sync::atomic::AtomicBool) -> std::io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    if abandoned.load(std::sync::atomic::Ordering::SeqCst) {
        if let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %error, "Failed to remove an upload of an abandoned request");
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "upload request was abandoned",
        ));
    }
    Ok(file)
}

/// A parsed message request. `uploads` are the files already stored for a
/// multipart request; they are removed again unless the request is admitted.
struct ParsedMessage {
    message: String,
    agent: AgentField,
    uploads: UploadGuard,
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

/// Content-free category of a Console request failure, safe to log.
///
/// The error chain itself is never logged: a storage or decode error can quote
/// stored input. Only these static names leave this function.
fn console_error_kind(error: &anyhow::Error) -> &'static str {
    if error.is::<crate::runtime::ingress_recovery::AlreadyAccepted>()
        || error.is::<crate::runtime::ingress_recovery::IngressConflict>()
    {
        return "conflict";
    }
    if error.is::<crate::memory::AgentScopeRefused>() {
        return "unauthorized";
    }
    for cause in error.chain() {
        if cause.is::<serde_json::Error>() {
            return "decode";
        }
        if cause.is::<rusqlite::Error>() || cause.is::<tokio_postgres::Error>() {
            return "storage";
        }
        if cause.is::<std::io::Error>() {
            return "io";
        }
        if cause.is::<tokio::time::error::Elapsed>() {
            return "timeout";
        }
    }
    "other"
}

/// Logs a cleanup-owner refusal, at most once per throttle interval per owner.
///
/// A failed owner refuses every request until recovery, so an unthrottled
/// warning would follow the request rate. Only a static category and the static
/// route name are logged.
fn warn_cleanup_refused(
    owner: &crate::runtime::turn_cleanup::CleanupTracker,
    route: &'static str,
    error: &anyhow::Error,
) {
    if owner.refusal_log_due() {
        tracing::warn!(
            error_kind = console_error_kind(error),
            route,
            "Console request refused by its runtime cleanup owner; repeats are logged at most once per minute"
        );
    }
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

fn history_partition(
    envelope: &RuntimeEnvelope,
    owner_id: &str,
    targeted: Option<&TargetedTurn>,
    incarnation: Option<&str>,
) -> Result<crate::memory::AgentPartition, Box<Response>> {
    let key = match targeted {
        None => crate::memory::PartitionKey::Main,
        Some(turn) => {
            let Some(raw) = incarnation else {
                return Err(Box::new(json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Targeted agent incarnation is missing",
                )));
            };
            let incarnation = uuid::Uuid::parse_str(raw).map_err(|_| {
                // The parse error can quote a character of the stored value, so
                // only a static category and the validated agent name are logged.
                tracing::warn!(
                    error_kind = "invalid_uuid",
                    agent = turn.name.as_str(),
                    "Targeted agent incarnation is not a valid UUID"
                );
                Box::new(json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Targeted agent incarnation is invalid",
                ))
            })?;
            crate::memory::PartitionKey::Named {
                name: turn.name.clone(),
                incarnation,
            }
        }
    };
    Ok(crate::memory::AgentPartition {
        workspace_id: envelope.workspace_id.clone(),
        owner_id: owner_id.to_owned(),
        key,
    })
}

/// OV-13: console turns are recorded, recovered and approved run-keyed only
/// with event recording on and a recovery repository present.
fn console_recordable(config: &crate::config::Config, memory: &dyn crate::memory::Memory) -> bool {
    let recording = config.memory.event_recording_config();
    recording.enabled && recording.record_user_messages && memory.recovery_repository().is_some()
}

/// Run-keyed approvals need recording on and a recovery repository (OV-13).
/// Recording turned off is the operator's choice (409, disabled); a memory
/// backend that keeps no repository fails closed (503), never "disabled".
fn approvals_unavailable(config: &crate::config::Config, memory: &dyn crate::memory::Memory) -> Option<Response> {
    let recording = config.memory.event_recording_config();
    if !(recording.enabled && recording.record_user_messages) {
        return Some(json_error(StatusCode::CONFLICT, "Decision approvals are disabled"));
    }
    if memory.recovery_repository().is_none() {
        return Some(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Decision approvals are unavailable: the memory backend keeps no recovery repository",
        ));
    }
    None
}

async fn recover_console_history(
    state: &AppState,
    runtime: &crate::gateway::TurnRuntimeGeneration,
    _authenticated: &super::ApiPrincipal,
    session_key: &str,
) -> anyhow::Result<()> {
    if !console_recordable(&runtime.config_generation.effective, state.mem.as_ref()) {
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
                if payload.get("decision_id").is_none_or(serde_json::Value::is_null) {
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
                    cleanup = Some(runtime.publication_slot.cleanup.register(cancellation.clone())?);
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
                let service = runtime.publication_slot.publish(runtime, state).await?;
                service
                    .runtime_holder()
                    .recovery_scope()
                    .rejoin_terminal(terminal_id, owner)
                    .await
                    .map_err(|error| {
                        if error
                            .downcast_ref::<crate::runtime::turn_terminal::restart_verify::TerminalNotReplayable>()
                            .is_some()
                        {
                            error.context(ConsoleTerminalRefused {
                                run_id: terminal_id.to_owned(),
                            })
                        } else {
                            error
                        }
                    })?;
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), recovery)
        .await
        .map_err(|_| anyhow::anyhow!("Console history recovery timed out; retry required"))
        .and_then(|result| result);
    match outcome {
        Ok(()) => {
            if let Some(ticket) = cleanup {
                ticket.finish(true);
            }
            Ok(())
        }
        Err(error) => {
            if let Some(refused) = error.downcast_ref::<ConsoleTerminalRefused>() {
                // Restart verification refuses before any write, so no write of
                // this recovery is unconfirmed: the ticket ends confirmed and
                // the refusal stays with its session. Store faults, timeouts
                // and projection-write failures keep the unfinished ticket,
                // which closes the owner until recovery (turn_cleanup).
                let reason = error
                    .downcast_ref::<crate::runtime::turn_terminal::restart_verify::TerminalNotReplayable>()
                    .map_or("unspecified", |refusal| refusal.0);
                tracing::error!(
                    run_id = %refused.run_id,
                    reason,
                    "Console terminal is not replayable; the refusal is isolated to its session"
                );
                if let Some(ticket) = cleanup {
                    ticket.finish(true);
                }
            }
            Err(error)
        }
    }
}

/// A Console terminal that restart verification refused
/// (`TerminalNotReplayable`) during history recovery: an error of that
/// session only, already logged with its run id; admission stays open.
#[derive(Debug)]
struct ConsoleTerminalRefused {
    run_id: String,
}

impl std::fmt::Display for ConsoleTerminalRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Console terminal {} is not replayable", self.run_id)
    }
}

/// The response of a failed history recovery. A session-isolated refusal was
/// logged where it was classified; every other failure is logged here.
fn history_recovery_failed(error: &anyhow::Error) -> Response {
    if error.downcast_ref::<ConsoleTerminalRefused>().is_none() {
        tracing::error!(error = %error, "Console history recovery failed");
    }
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Console history recovery is incomplete",
    )
}

async fn append_console_turn(
    state: &AppState,
    envelope: &RuntimeEnvelope,
    role: &str,
    content: &str,
    record_message_event: bool,
    durable_projection: bool,
) -> anyhow::Result<String> {
    append_console_turn_with(
        state,
        envelope,
        role,
        content,
        record_message_event,
        durable_projection,
        None,
    )
    .await
}

/// An rf `context` input: its user event and its attempt row commit in one
/// transaction (AC-1b2a), then it is projected like any recorded input.
async fn append_admitted_console_turn(
    state: &AppState,
    envelope: &RuntimeEnvelope,
    content: &str,
    durable_projection: bool,
    admission: crate::memory::RfAdmission,
) -> anyhow::Result<String> {
    append_console_turn_with(
        state,
        envelope,
        "user",
        content,
        true,
        durable_projection,
        Some(admission),
    )
    .await
}

async fn append_console_turn_with(
    state: &AppState,
    envelope: &RuntimeEnvelope,
    role: &str,
    content: &str,
    record_message_event: bool,
    durable_projection: bool,
    admission: Option<crate::memory::RfAdmission>,
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
        Some(match admission {
            _ if role == "assistant" => {
                fabric
                    .record_assistant_message(envelope.message_scope(), &*content)
                    .await?
            }
            Some(admission) => {
                fabric
                    .record_admitted_user_message(envelope.message_scope(), &*content, None, admission)
                    .await?
            }
            None => {
                fabric
                    .record_inbound_user_message(envelope.message_scope(), &*content, None, None)
                    .await?
            }
        })
    } else {
        None
    };
    if matches!(
        envelope.agent_partition.as_ref().map(|partition| &partition.key),
        Some(crate::memory::PartitionKey::Named { .. })
    ) && event.is_none()
    {
        return Err(crate::memory::AgentScopeRefused.into());
    }
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
            envelope.agent_partition.as_ref(),
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
    service: std::sync::Arc<crate::runtime::turn_publication::TurnRuntimeService>,
    context: std::sync::Arc<crate::runtime::turn_context::OpenedTurn>,
    advice: Option<crate::decision_engine::IntentAnalysis>,
    proof: &'a crate::gateway::turn_source::GatewayTurnProof,
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
    target: Option<&TargetedTurn>,
) -> anyhow::Result<ConsoleTurnResult> {
    decision.proof.verify_runtime_input(&turn_runtime, visible_message)?;
    let turn_envelope = decision.proof.envelope().clone();
    // D2: this runtime turn rebuilds a `SecurityPolicy` (below) that gates tool
    // side-effects for the turn, so the config snapshot it derives from MUST be the
    // pinned TurnRuntimeGeneration, not an independently cached config owner. This
    // makes a reloaded autonomy / security.audit take effect for console-driven
    // turns without a restart; all non-security fields used for the prompt come from
    // the same snapshot, so the turn stays internally consistent.
    let mut config_snapshot = (*turn_runtime.config_generation.effective).clone();
    if let Some(target) = target {
        // The target's own level (`agent_level`); the same level the target's
        // decision slot publishes its tool policy with.
        config_snapshot.autonomy.level = target.autonomy;
    }
    // A message-woken run is its own root, owned by the receiving session and
    // built explicitly by the mailbox receiver. Every other turn resolves its
    // root from the stored source event.
    let woken_root = mailbox::woken_scope();
    let woken = woken_root.is_some();
    let live_scope = if let Some(root) = woken_root {
        Some(root)
    } else if let (Some(store), Some(source)) = (state.mem.sync_store(), turn_envelope.source_message_event_id.clone())
    {
        let owner = turn_envelope.resolved_owner_id();
        crate::runtime::blocking::spawn_blocking(move || store.live_scope(&owner, &source)).await??
    } else {
        None
    };
    let live = live_scope.map(|scope| {
        let wait_secs = target
            .and_then(|target| config_snapshot.agents.get(target.name.as_str()))
            .map_or(config_snapshot.agent.approval_wait_secs, |agent| {
                agent.approval_wait_secs
            });
        std::sync::Arc::new(crate::gateway::run_approvals::RunApprovalResolver {
            scope,
            streams: std::sync::Arc::clone(&state.rf_streams),
            approvals: std::sync::Arc::clone(&state.rf_approvals),
            // The resolver owns a child of the run token: run cancellation reaches its
            // approval waits, while dropping it never cancels the run or the job.
            cancellation: decision.cancellation.child_token(),
            wait_secs,
            woken,
        }) as std::sync::Arc<dyn crate::agent::live_frames::LiveTurn>
    });
    let live_frames = live.as_ref().map(|live| {
        live.frames(
            &turn_envelope,
            &crate::agent::sanitize::known_tool_names(turn_runtime.tools_registry.as_ref()),
        )
    });
    let stream_provider = live.is_some() && turn_runtime.provider.supports_streaming();
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
            if stream_provider {
                crate::providers::traits::ProviderRequestMode::Streaming
            } else {
                crate::providers::traits::ProviderRequestMode::NonStreaming
            },
        )
        .native_tool_calling;
    let selected_skills = decision
        .service
        .agent_prompt_skills(decision.proof, &decision.context)?;
    let system_prompt = if let Some(target) = target {
        // The standard sections at the target's own level, with the target's
        // own identity in place of the main persona.
        crate::agent::runtime_prompt::build_named_agent_system_prompt(
            &config_snapshot,
            target.autonomy,
            &turn_runtime.model,
            &selected_skills,
            native_tools,
            &target.system_prompt,
        )
    } else {
        build_runtime_system_prompt(&config_snapshot, &turn_runtime.model, &selected_skills, native_tools)
    };

    // The turn opening: recall, the stored attachment row, the replay of the
    // earlier turns with their tool rounds, and the fit to the budget.
    let crate::context::api::open::TurnOpening {
        mut history,
        history_event_ids,
        compaction: effective_compaction,
        recalled_ids: _,
    } = crate::context::api::open::open_turn(crate::context::api::open::OpenInput {
        memory: &state.mem,
        envelope: &turn_envelope,
        memory_config: &config_snapshot.memory,
        chat_type: "private",
        visible_message,
        card: None,
        previous_turns,
        replay: crate::context::api::open::ReplayMode::FitToBudget,
        system_prompt,
        service: &decision.service,
        context: &decision.context,
        provider_label: &provider_label,
        model: &turn_runtime.model,
    })
    .await;

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
    let provider_started_at = chrono::Utc::now();
    let loop_result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            advice: decision.advice.clone(),
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
            compaction: Some(&effective_compaction),
            decision: crate::agent::turn_request::TurnDecision {
                service: &decision.service,
                context: &decision.context,
                runtime: decision.context.runtime(),
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
                live_frames,
                guarded_delivery: false,
                provider_trace: decision
                    .provider_trace
                    .as_ref()
                    .map(|(trace, _)| std::sync::Arc::clone(trace)),
                events: None,
                stream_provider,
                tool_execution_service: None,
                tool_execution_context: {
                    let mut context = crate::tools::ToolExecutionContext::new(turn_envelope.clone(), "private")
                        .with_dispatch_origin(crate::tools::DispatchOrigin::ModelTurn);
                    context.live_turn = live
                        .as_ref()
                        .map(|live| crate::agent::live_frames::LiveTurnRef(std::sync::Arc::clone(live)))
                        // A message-woken turn with no approval surface refuses a call
                        // that needs approval at once: no pending row nobody can act on.
                        .or_else(|| {
                            (target.is_none() && mailbox::woken_scope().is_some())
                                .then(crate::runtime::mailbox_child::no_surface)
                        });
                    if target.is_some() && live.is_none() {
                        // Until MA-04 a targeted call that needs approval ends
                        // denied: no durable ledger route, no pending decision.
                        context.with_pending_approval_denied()
                    } else {
                        context.with_approval_ledger(decision.service.execution_approval_ledger())
                    }
                    .with_chat_id(&turn_envelope.session_key)
                },
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
            .with_request_event_scope(turn_envelope.message_scope())
            .with_history_event_ids(history_event_ids)
            .with_rendered_stored_handoffs(),
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
async fn parse_json_message(state: &AppState, request: Request) -> Result<ParsedMessage, Response> {
    let Json(payload) = Json::<SendMessageRequest>::from_request(request, state)
        .await
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "Invalid JSON payload"))?;

    let message = payload.message.trim().to_string();
    if message.is_empty() {
        return Err(json_error(StatusCode::BAD_REQUEST, "message must not be empty"));
    }

    Ok(ParsedMessage {
        message,
        agent: payload.agent.map_or(AgentField::Omitted, AgentField::from_json),
        uploads: UploadGuard::default(),
    })
}

#[allow(clippy::result_large_err)]
async fn parse_multipart_message(state: &AppState, request: Request) -> Result<ParsedMessage, Response> {
    let owner_authenticated = super::owner_authenticated(state, request.headers());
    let mut multipart = Multipart::from_request(request, state)
        .await
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "Invalid multipart payload"))?;

    let uploads_root = {
        let config = state.config.load_full();
        config.workspace_dir.join(UPLOADS_DIR_NAME)
    };
    let mut uploads_root_prepared = false;

    let mut message = String::new();
    let mut agent = AgentField::Omitted;
    let mut attachments = Vec::new();
    let mut uploads = UploadGuard::default();
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
            "agent" => {
                let text = field.text().await.map_err(|_| {
                    json_error(StatusCode::BAD_REQUEST, "Failed to decode agent from multipart payload")
                })?;
                agent = AgentField::Text(text);
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

                if !owner_authenticated {
                    super::authorize_resource_mutation(state, "gateway_api:sessions:upload", ResourceRiskLevel::Low)
                        .map_err(|error| error.into_response())?;
                }

                if !uploads_root_prepared {
                    fs::create_dir_all(&uploads_root).await.map_err(|error| {
                        tracing::error!("Failed to create uploads directory: {error}");
                        json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to prepare uploads directory")
                    })?;
                    uploads_root_prepared = true;
                }

                // A per-request unique name, created exclusively: concurrent
                // requests never share a path, and cleanup never reaches a file
                // this request did not create.
                let stored_name = format!(
                    "{}_{}_{}_{}",
                    Utc::now().timestamp_millis(),
                    uuid::Uuid::new_v4().simple(),
                    file_count,
                    safe_name
                );
                let stored_path = uploads_root.join(stored_name);

                let persist_failed = |error: std::io::Error| {
                    tracing::error!("Failed to persist uploaded file: {error}");
                    json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to persist uploaded file")
                };
                {
                    use tokio::io::AsyncWriteExt;
                    // Ownership first: the guard knows the path before the
                    // blocking worker can create it.
                    let abandoned = uploads.begin(stored_path.clone());
                    let worker_path = stored_path.clone();
                    let created = tokio::task::spawn_blocking(move || create_upload_file(&worker_path, &abandoned))
                        .await
                        .map_err(|error| persist_failed(std::io::Error::other(error)))?;
                    let created = uploads.settle(&stored_path, created).map_err(persist_failed)?;
                    let mut file = fs::File::from_std(created);
                    file.write_all(&bytes).await.map_err(persist_failed)?;
                    file.flush().await.map_err(persist_failed)?;
                }

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

    Ok(ParsedMessage {
        message: combined,
        agent,
        uploads,
    })
}

#[allow(clippy::result_large_err)]
async fn parse_message_from_request(state: &AppState, request: Request) -> Result<ParsedMessage, Response> {
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

/// `{}` creates a Console session as before; `{"session_key"}` is the rf
/// get-or-create (AC-1b1). Present-or-absent is kept apart from `null`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateSessionRequest {
    #[serde(default, deserialize_with = "present_value")]
    session_key: Option<serde_json::Value>,
}

pub async fn post_session(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<super::ApiPrincipal>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Json(body): Json<CreateSessionRequest>,
) -> Response {
    if let Some(key) = body.session_key {
        return rf_message::post_keyed_session(&state, &principal, &headers, &uri, key).await;
    }
    if !super::owner_authenticated(&state, &headers)
        && let Err(error) =
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
    let mut identity = crate::hooks::HookIdentity::for_session_key(session_id.clone(), "gateway");
    identity.owner = Some(principal.owner_id.clone());
    state
        .hooks
        .emit_for(
            &identity,
            crate::hooks::HookEvent::SessionOpened,
            serde_json::json!({"resumed": false, "surface": "console", "title": null}),
        )
        .await;
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"session_id": session_id, "status":"empty", "message_count":0})),
    )
        .into_response()
}

pub async fn get_sessions(
    State(state): State<AppState>,
    Query(query): Query<SessionsQuery>,
    headers: axum::http::HeaderMap,
    axum::Extension(api_principal): axum::Extension<super::ApiPrincipal>,
) -> impl IntoResponse {
    // MA-01h: a paired bearer (the install owner) lists only its own
    // sessions, so no preview shows another owner's content. With pairing
    // off the listing is unchanged.
    let owner_filter = super::owner_authenticated(&state, &headers).then_some(api_principal.owner_id.as_str());
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
        let listed = match owner_filter {
            Some(owner_id) => {
                state
                    .mem
                    .list_owner_conversation_sessions(owner_id, fetch_batch, source_offset, query.channel.as_deref())
                    .await
            }
            None => {
                state
                    .mem
                    .list_conversation_sessions(fetch_batch, source_offset, query.channel.as_deref())
                    .await
            }
        };
        let sessions = match listed {
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
            let metadata = if matches!(session.channel.as_str(), "cron" | "xin") {
                let memory = std::sync::Arc::clone(&state.mem);
                let owner = api_principal.owner_id.clone();
                let key = session.session_key.clone();
                match crate::runtime::blocking::spawn_blocking(move || {
                    memory
                        .sync_store()
                        .map_or(Ok(None), |sync| sync.session_metadata(&owner, &key))
                })
                .await
                {
                    Ok(Ok(metadata)) => metadata,
                    _ => {
                        return super::capabilities::coded_error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "storage_unavailable",
                            "scheduled session metadata unavailable",
                        );
                    }
                }
            } else {
                None
            };
            response.push(SessionSummary {
                metadata,
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
    if rf_message::is_caller_session_key(&session_id)
        && let Err(response) =
            rf_message::require_session_owner(&state, &session_id, &api_principal.owner_id, false).await
    {
        return *response;
    }
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
        // An id in the caller-key grammar names an rf session (Console ids
        // start with `console-`, which the grammar excludes): a missing one
        // is the coded `404 session_unknown`, like another owner's.
        if rf_message::is_caller_session_key(&session_id) {
            return rf_message::session_unknown();
        }
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Session not found" })),
        )
            .into_response();
    };

    let limit = normalize_limit(query.limit);
    let offset = normalize_offset(query.offset);
    // Recovery settles turns a ready runtime ran; a runtime standing by for a
    // provider credential has run none since it started, and the stored
    // history is still served.
    if let Ok(runtime) = state.admit_turn_runtime()
        && let Err(error) = recover_console_history(&state, &runtime, &api_principal, &session_id).await
    {
        return history_recovery_failed(&error);
    }
    if session.channel == crate::memory::CALLER_KEYED_SESSION_CHANNEL {
        return rf_message::rf_history(&state, &session, &api_principal, query.agent.as_deref(), limit, offset).await;
    }
    if matches!(session.channel.as_str(), "cron" | "xin") {
        let memory = std::sync::Arc::clone(&state.mem);
        let owner = api_principal.owner_id.clone();
        let key = session.session_key.clone();
        let named = crate::runtime::blocking::spawn_blocking(move || {
            memory.sync_store().map_or(Ok(false), |sync| {
                sync.session_metadata(&owner, &key).map(|metadata| metadata.is_some())
            })
        })
        .await;
        match named {
            Ok(Ok(true)) => {
                return rf_message::rf_history(
                    &state,
                    &session,
                    &api_principal,
                    query.agent.as_deref().or(Some(session.sender.as_str())),
                    limit,
                    offset,
                )
                .await;
            }
            Ok(Ok(false)) => {}
            _ => {
                return super::capabilities::coded_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "storage_unavailable",
                    "scheduled session metadata unavailable",
                );
            }
        }
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
            agent: None,
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
    if let Some(view) = state.rf_approvals.root_view(&principal.owner_id, &session_id, &decision_id) {
        return Json(view).into_response();
    }
    call_approval_action(state, session_id, decision_id, principal, None, false).await
}

const INBOX_DEFAULT_LIMIT: usize = 50;
const INBOX_MAX_LIMIT: usize = 100;

fn normalize_inbox_limit(limit: Option<usize>) -> usize {
    match limit {
        Some(0) | None => INBOX_DEFAULT_LIMIT,
        Some(value) => value.min(INBOX_MAX_LIMIT),
    }
}

#[allow(clippy::result_large_err)] // the axum `Response` is the error contract of this parser
fn parse_inbox_cursor(cursor: Option<&str>) -> Result<i64, Response> {
    let Some(cursor) = cursor.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(0);
    };
    cursor
        .parse::<i64>()
        .ok()
        .filter(|id| *id >= 0)
        .ok_or_else(|| json_error(StatusCode::BAD_REQUEST, "Invalid approval cursor"))
}

#[derive(Serialize)]
struct ApprovalInboxItem {
    session_id: String,
    #[serde(flatten)]
    view: crate::security::approval_ledger::view::DecisionApprovalView,
}

#[derive(Serialize)]
struct ApprovalInboxResponse {
    approvals: Vec<ApprovalInboxItem>,
    next_cursor: Option<String>,
}

/// Owner-scoped pending approval inbox. Foreign rows are omitted.
pub(super) async fn get_session_approvals(
    State(state): State<AppState>,
    Query(query): Query<ApprovalInboxQuery>,
    axum::Extension(principal): axum::Extension<super::ApiPrincipal>,
) -> Response {
    let runtime = match state.admit_turn_runtime() {
        Ok(runtime) => runtime,
        Err(refusal) => return super::provider_not_ready(refusal),
    };
    if let Some(response) = approvals_unavailable(&runtime.config_generation.effective, state.mem.as_ref()) {
        return response;
    }
    let after_id = match parse_inbox_cursor(query.cursor.as_deref()) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let limit = normalize_inbox_limit(query.limit);
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cleanup =
        match crate::runtime::turn_cleanup::CleanupScope::new(Some(&runtime.publication_slot.cleanup), cancellation) {
            Ok(cleanup) => cleanup,
            Err(error) => {
                warn_cleanup_refused(&runtime.publication_slot.cleanup, "Console approval inbox", &error);
                return json_error(StatusCode::SERVICE_UNAVAILABLE, "Gateway is draining");
            }
        };
    let result = cleanup
        .run(Box::pin(async {
            let service = runtime.publication_slot.publish(&runtime, &state).await?;
            let mut envelope = RuntimeEnvelope::console(
                runtime
                    .config_generation
                    .effective
                    .workspace_dir
                    .to_string_lossy()
                    .as_ref(),
                "approval-inbox",
            );
            envelope.owner_id = Some(principal.owner_id.clone());
            let adapter = service.call_approval_strategy(envelope)?;
            let calls = adapter
                .port
                .list_pending_calls(&principal.owner_id, after_id, limit)
                .await?;
            let mut rows = Vec::new();
            use crate::security::approval_ledger::keys::ApprovalAuthority;
            for item in calls.items {
                let event = service
                    .memory()
                    .find_message_event_by_idempotency_key(adapter.port.workspace, &item.authority().pending_key())
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("pending call disappeared"))?;
                rows.push((
                    event.id,
                    ApprovalInboxItem {
                        session_id: item.rf.as_ref().map(|root| root.root_session.clone())
                            .or_else(|| item.subject().session_key.clone()).unwrap_or_default(),
                        view: { let mut view = item.view(); if let Some(root) = &item.rf { view.tool = Some(root.tool.clone()); } view },
                    },
                ));
            }
            // Do not advance past the reader's bounded scan frontier.
            let frontier = calls.next_cursor;
            if let Some(frontier) = frontier {
                rows.retain(|(id, _)| *id <= frontier);
            }
            rows.sort_by_key(|(id, _)| *id);
            let more = rows.len() > limit;
            rows.truncate(limit);
            let next_cursor = if more {
                rows.last().map(|(id, _)| id.to_string())
            } else {
                frontier.map(|id| id.to_string())
            };
            Ok::<_, anyhow::Error>(ApprovalInboxResponse {
                approvals: rows.into_iter().map(|(_, item)| item).collect(),
                next_cursor,
            })
        }))
        .await;
    match result {
        Ok(page) => Json(page).into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "approval inbox failed");
            json_error(StatusCode::SERVICE_UNAVAILABLE, "Approval inbox is unavailable")
        }
    }
}

pub(super) async fn post_decision_approval(
    State(state): State<AppState>,
    Path((session_id, decision_id)): Path<(String, String)>,
    axum::Extension(principal): axum::Extension<super::ApiPrincipal>,
    headers: HeaderMap,
    Json(request): Json<serde_json::Value>,
) -> Response {
    let owner_authenticated = super::owner_authenticated(&state, &headers);
    let mut request = request;
    match rf_message::keyed_session(&state, &session_id).await {
        Some(true) => {
            if !owner_authenticated {
                return super::capabilities::coded_error(
                    StatusCode::UNAUTHORIZED,
                    "auth_failed",
                    "paired bearer required",
                );
            }
            // A message-woken turn raises an ordinary call approval with no rf
            // root; decide it through the ordinary call approval path.
            let plain = crate::gateway::run_approvals::is_plain_call(&state, &decision_id).await;
            if plain {
                let approved = match request.get("decision").and_then(serde_json::Value::as_str) {
                    Some("approved") => true,
                    Some("denied") => false,
                    _ => {
                        return super::capabilities::coded_error(
                            StatusCode::BAD_REQUEST,
                            "field_invalid",
                            "decision must be approved or denied",
                        );
                    }
                };
                request = serde_json::json!({ "approved": approved });
            } else {
                return crate::gateway::run_approvals::decide(
                    &state,
                    &principal.owner_id,
                    &session_id,
                    &decision_id,
                    request,
                )
                .await;
            }
        }
        None => {
            return super::capabilities::coded_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
                "session lookup failed",
            );
        }
        Some(false) => {}
    }
    let Ok(request) = serde_json::from_value::<DecisionApprovalRequest>(request) else {
        return json_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid approval body");
    };
    if state.rf_approvals.root_view(&principal.owner_id, &session_id, &decision_id).is_some() {
        if !owner_authenticated {
            return json_error(StatusCode::FORBIDDEN, "Paired owner approval is required");
        }
        let decision = if request.approved { "approved" } else { "denied" };
        return crate::gateway::run_approvals::decide(&state, &principal.owner_id, &session_id, &decision_id,
            serde_json::json!({"decision":decision})).await;
    }
    call_approval_action(
        state,
        session_id,
        decision_id,
        principal,
        Some(DecisionApprovalChange::Review(request.approved)),
        owner_authenticated,
    )
    .await
}

pub(super) async fn delete_decision_approval(
    State(state): State<AppState>,
    Path((session_id, decision_id)): Path<(String, String)>,
    axum::Extension(principal): axum::Extension<super::ApiPrincipal>,
    headers: HeaderMap,
) -> Response {
    let owner_authenticated = super::owner_authenticated(&state, &headers);
    call_approval_action(
        state,
        session_id,
        decision_id,
        principal,
        Some(DecisionApprovalChange::Revoke),
        owner_authenticated,
    )
    .await
}

enum DecisionApprovalChange {
    Review(bool),
    Revoke,
}

fn console_decision_control_policy(config: &crate::config::Config) -> Result<(), &'static str> {
    crate::security::SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)
        .enforce_tool_operation(crate::security::policy::ToolOperation::Act, "console decision control")
        .map_err(|_| "Decision control is not permitted by policy")
}
