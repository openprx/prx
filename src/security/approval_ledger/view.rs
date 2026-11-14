//! Shared approval DTO and source-scope helper used by the run-keyed
//! exact-call approvals (moved unchanged from the deleted `pending.rs`).
use crate::memory::{MessageEvent, MessageEventScope};
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct DecisionApprovalView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) call_id: Option<String>,
    pub(crate) decision_id: String,
    pub(crate) tool: Option<String>,
    pub(crate) arguments: Option<serde_json::Value>,
    pub(crate) arguments_redacted: bool,
    pub(crate) original_call_available: bool,
    pub(crate) effect: String,
    pub(crate) call_digest: String,
    pub(crate) pending_expires_at: DateTime<Utc>,
    pub(crate) single_use: bool,
}

pub(crate) fn scope_from_source(source: &MessageEvent) -> MessageEventScope {
    let mut scope = MessageEventScope::new(source.source.clone(), source.visibility.clone());
    scope.owner_id = source.owner_id.clone();
    scope.channel = source.channel.clone();
    scope.session_key = source.session_key.clone();
    scope.run_id = source.run_id.clone();
    scope.parent_session_key = source.parent_session_key.clone();
    scope.parent_run_id = source.parent_run_id.clone();
    scope.agent_id = source.agent_id.clone();
    scope.persona_id = source.persona_id.clone();
    scope.sender = source.sender.clone();
    scope.recipient = source.recipient.clone();
    scope.causation_event_id = Some(source.event_id.clone());
    scope.config_generation_id = source.config_generation_id;
    scope.config_source_revision = source.config_source_revision.clone();
    scope.attempt_id = source.attempt_id.clone();
    scope.lease_epoch = source.lease_epoch;
    scope
}
