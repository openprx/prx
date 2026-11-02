//! Supervised approval over the bridge.
//!
//! `full` autonomy returns a grant and puts nothing on the wire. Supervised
//! mode sends `approval.request` with redacted arguments and the SHA-256 of
//! the canonical full arguments, then waits for `approval.decision` until
//! `expires_at_ms`. Chat text is never a decision.

use super::authorization::{Authority, EffectiveAutonomy};
use super::protocol::{self, ApprovalDecisionKind, ApprovalRequestParams};
use super::rpc::RpcHub;
use super::socket::unix_ms;
use crate::agent::turn_contracts::{ApprovalDecision, ApprovalResolver};
use crate::approval::{ApprovalRequest, ApprovalRisk};
use crate::channels::rich_turn;
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved,
    Denied,
    Cancelled,
}

/// Waits for `approval.decision` and implements [`ApprovalResolver`].
struct Pending {
    sender: super::super::rich::Sender,
    conversation: String,
    tx: oneshot::Sender<ApprovalOutcome>,
}

pub struct ApprovalGate {
    rpc: Arc<RpcHub>,
    auth: Arc<Authority>,
    wait_ms: u64,
    waiters: Mutex<HashMap<String, Pending>>,
    senders: Mutex<HashMap<String, super::super::rich::Sender>>,
}

impl ApprovalGate {
    #[must_use]
    pub fn new(rpc: Arc<RpcHub>, auth: Arc<Authority>, wait_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            rpc,
            auth,
            wait_ms: wait_ms.max(1),
            waiters: Mutex::new(HashMap::new()),
            senders: Mutex::new(HashMap::new()),
        })
    }

    pub fn bind_sender(&self, turn: &str, sender: super::super::rich::Sender) {
        self.senders.lock().insert(turn.to_owned(), sender);
    }
    pub fn unbind_sender(&self, turn: &str) {
        self.senders.lock().remove(turn);
    }

    /// Only the host's explicit decision from the initiating principal/device
    /// in this conversation can resolve the outstanding request.
    pub fn decide(
        &self,
        approval_id: &str,
        decision: ApprovalDecisionKind,
        conversation: &str,
        sender: &super::super::rich::Sender,
    ) -> bool {
        let mut waiters = self.waiters.lock();
        let Some(pending) = waiters.get(approval_id) else {
            return false;
        };
        if pending.conversation != conversation
            || pending.sender.principal_id != sender.principal_id
            || pending.sender.device_id != sender.device_id
            || sender.kind != super::super::rich::SenderKind::Human
            || sender.trust != super::super::rich::Trust::Verified
        {
            return false;
        }
        let Some(pending) = waiters.remove(approval_id) else {
            return false;
        };
        drop(waiters);
        let outcome = match decision {
            ApprovalDecisionKind::Approved => ApprovalOutcome::Approved,
            ApprovalDecisionKind::Denied => ApprovalOutcome::Denied,
            ApprovalDecisionKind::Expired => ApprovalOutcome::Cancelled,
        };
        pending.tx.send(outcome).is_ok()
    }

    /// Resolve one tool call to the three-way bridge outcome.
    pub async fn resolve_outcome(&self, request: &ApprovalRequest) -> ApprovalOutcome {
        if self.auth.effective() == EffectiveAutonomy::Full {
            return ApprovalOutcome::Approved;
        }
        let Some(active) = rich_turn::active_turn() else {
            tracing::warn!("ramflux approval refused because no turn is active");
            return ApprovalOutcome::Cancelled;
        };
        let turn = (active.conversation_id, active.turn_id);
        let Some(sender) = self.senders.lock().get(&turn.1).cloned() else {
            tracing::warn!("ramflux approval has no bound initiating sender");
            return ApprovalOutcome::Cancelled;
        };
        let approval_id = Uuid::new_v4().to_string();
        let expires_at_ms = request
            .expires_at_ms
            .unwrap_or_else(|| unix_ms().saturating_add(self.wait_ms));
        let args_hash = match request.args_hash.clone() {
            Some(hash) => hash,
            None => match protocol::args_hash(&request.arguments) {
                Ok(hash) => hash,
                Err(error) => {
                    tracing::warn!(error = %error, "ramflux approval could not hash arguments");
                    return ApprovalOutcome::Cancelled;
                }
            },
        };
        let mut redacted = request.arguments.clone();
        crate::security::redaction::redact_json_value(&mut redacted);
        let risk = risk_label(request.risk);
        let params = ApprovalRequestParams {
            conversation_id: &turn.0,
            turn_id: &turn.1,
            approval_id: &approval_id,
            tool_name: &request.tool_name,
            arguments_redacted: &redacted,
            args_hash: &args_hash,
            risk,
            expires_at_ms,
            rationale: None,
        };
        let payload = match serde_json::to_value(&params) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(error = %error, "ramflux approval payload could not be encoded");
                return ApprovalOutcome::Cancelled;
            }
        };
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().insert(
            approval_id.clone(),
            Pending {
                sender,
                conversation: turn.0.clone(),
                tx,
            },
        );
        let mut guard = WaiterGuard {
            gate: self,
            id: approval_id.clone(),
            armed: true,
        };
        let status = super::super::rich::TurnStatus {
            conversation_id: turn.0.clone(),
            turn_id: turn.1.clone(),
            state: super::super::rich::TurnState::AwaitingApproval,
            error: None,
            approval_id: Some(approval_id.clone()),
        };
        if let Err(error) = super::outbound::send_status(&self.rpc, &status).await {
            tracing::warn!(%error, "ramflux approval status was not sent");
            return ApprovalOutcome::Cancelled;
        }
        let ack = self
            .rpc
            .request(protocol::METHOD_APPROVAL_REQUEST, &payload, Duration::from_secs(10))
            .await;
        if let Err(error) = ack {
            tracing::warn!(error = %error, approval_id = %approval_id, "approval.request was not sent");
            return ApprovalOutcome::Cancelled;
        }
        let remaining = expires_at_ms.saturating_sub(unix_ms());
        let outcome = match tokio::time::timeout(Duration::from_millis(remaining.max(1)), rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) | Err(_) => ApprovalOutcome::Cancelled,
        };
        guard.armed = false;
        self.waiters.lock().remove(&approval_id);
        outcome
    }
}

/// Removes the waiter if the resolve future is dropped before a decision.
struct WaiterGuard<'a> {
    gate: &'a ApprovalGate,
    id: String,
    armed: bool,
}

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.gate.waiters.lock().remove(&self.id);
        }
    }
}

#[async_trait]
impl ApprovalResolver for ApprovalGate {
    async fn resolve(&self, request: &ApprovalRequest, _channel: &str) -> ApprovalDecision {
        match self.resolve_outcome(request).await {
            ApprovalOutcome::Approved => ApprovalDecision::Grant,
            ApprovalOutcome::Denied | ApprovalOutcome::Cancelled => ApprovalDecision::Deny,
        }
    }
}

const fn risk_label(risk: Option<ApprovalRisk>) -> &'static str {
    match risk {
        Some(ApprovalRisk::Low) => "low",
        Some(ApprovalRisk::High) => "high",
        Some(ApprovalRisk::Medium) | None => "medium",
    }
}
