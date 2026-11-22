//! Live rf approve-and-continue. Keys and grants remain owned by the exact-call ledger.
use crate::agent::live_frames::{LiveFrameSink, LiveTurn};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::security::approval_ledger::{
    keys::{ApprovalAuthority, RunCallAuthority},
    run_call::{ApprovalLedgerPort, PendingCall, RfPending},
    strategy::ExecutionApprovalLedger,
};
use crate::tools::{ToolApprovalDecision, ToolApprovalRequest};
use anyhow::Context;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Decision {
    Approved,
    Denied,
    Expired,
    Cancelled,
}
impl Decision {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }
}
#[derive(Default)]
pub struct ApprovalHub {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
}
struct Entry {
    ledger: ExecutionApprovalLedger,
    pending: PendingCall,
    claimed: AtomicBool,
    outcome: watch::Sender<Option<Decision>>,
    cancel: CancellationToken,
}
impl Entry {
    fn port(&self) -> anyhow::Result<ApprovalLedgerPort<'_>> {
        Ok(ApprovalLedgerPort {
            memory: &self.ledger.memory,
            state_key: &self.ledger.state_key,
            workspace: &self.ledger.workspace,
            keyring: crate::acl::approval_grant::WitnessKeyring::global()?,
        })
    }
    async fn finish(&self, decision: Decision) -> anyhow::Result<Decision> {
        if self
            .claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            let mut rx = self.outcome.subscribe();
            loop {
                let observed = *rx.borrow_and_update();
                if let Some(value) = observed {
                    return Ok(value);
                }
                rx.changed().await.context("approval waiter closed")?;
            }
        }
        let result = async {
            let port = self.port()?;
            if decision == Decision::Approved {
                port.review(
                    &self.pending.subject().owner_id,
                    self.pending
                        .subject()
                        .session_key
                        .as_deref()
                        .context("approval session missing")?,
                    self.pending.authority(),
                    true,
                )
                .await?;
            }
            port.record_rf_decision(&self.pending, decision.as_str()).await?;
            Ok::<_, anyhow::Error>(decision)
        }
        .await;
        match result {
            Ok(value) => {
                self.outcome.send_replace(Some(value));
                Ok(value)
            }
            Err(error) => {
                self.outcome.send_replace(Some(Decision::Cancelled));
                // Fail closed and wake every waiter if the source cannot be committed.
                // The run can now reach its terminal, which closes the pending source transactionally.
                Err(error)
            }
        }
    }
}

pub(crate) struct RunApprovalResolver {
    pub(crate) scope: Value,
    pub(crate) streams: Arc<super::live_frames::StreamHub>,
    pub(crate) approvals: Arc<ApprovalHub>,
    pub(crate) cancellation: CancellationToken,
    pub(crate) wait_secs: u64,
}
impl Drop for RunApprovalResolver {
    fn drop(&mut self) {
        let root = self.scope.get("run_id").and_then(Value::as_str);
        self.approvals.entries.lock().retain(|_, entry| {
            if entry
                .pending
                .rf
                .as_ref()
                .is_some_and(|rf| Some(rf.root_run_id.as_str()) == root)
            {
                entry.cancel.cancel();
                false
            } else {
                true
            }
        });
    }
}
#[async_trait::async_trait]
impl LiveTurn for RunApprovalResolver {
    fn frames(&self, envelope: &RuntimeEnvelope, tools: &HashSet<String>) -> Arc<dyn LiveFrameSink> {
        self.streams.producer(&self.scope, envelope, tools)
    }
    async fn approve(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        match self.resolve(request).await {
            Ok(decision) => decision,
            Err(error) => {
                tracing::warn!("rf approval ledger refused a call");
                ToolApprovalDecision::Denied {
                    cause: crate::tools::ApprovalDenialCause::LedgerRefused,
                    reason: crate::security::redaction::redacted_error_report(&error),
                }
            }
        }
    }
}
impl RunApprovalResolver {
    async fn resolve(&self, request: ToolApprovalRequest) -> anyhow::Result<ToolApprovalDecision> {
        let ledger = request
            .context
            .approval_ledger
            .as_ref()
            .context("rf approval ledger unavailable")?;
        let keyring = crate::acl::approval_grant::WitnessKeyring::global()?;
        let port = ApprovalLedgerPort {
            memory: &ledger.memory,
            state_key: &ledger.state_key,
            workspace: &ledger.workspace,
            keyring,
        };
        let entry = ledger
            .catalog
            .entry_for_public_name(&request.descriptor.public_name)
            .context("approval capability unavailable")?;
        anyhow::ensure!(
            entry.root == request.descriptor.backend_name,
            "approval backend changed"
        );
        anyhow::ensure!((1..=86400).contains(&self.wait_secs), "invalid approval wait");
        let mut arguments = request.command.arguments.clone();
        crate::security::redaction::redact_json_value(&mut arguments);
        let mut summary = crate::security::redact(&format!(
            "{} {}",
            request.descriptor.public_name,
            serde_json::to_string(&arguments)?
        ));
        let mut end = summary.len().min(512);
        while !summary.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        summary.truncate(end);
        let text = |key| {
            self.scope
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .context("approval root scope missing")
        };
        let pending = port
            .record_pending_rf(
                &request.context.envelope,
                entry,
                &request.command.arguments,
                RfPending {
                    tool: request.descriptor.public_name.clone(),
                    summary,
                    root_session: text("session")?,
                    root_run_id: text("run_id")?,
                    agent: text("agent")?,
                    incarnation: text("incarnation")?,
                    wait_secs: self.wait_secs,
                },
            )
            .await?;
        let id = pending.authority().call_id();
        let saved = ledger
            .memory
            .find_message_event_by_idempotency_key(&ledger.workspace, &format!("rf-decision:{id}"))
            .await?;
        let decision = if let Some(saved) = saved {
            let value: Value =
                serde_json::from_str(saved.raw_payload_json.as_deref().context("approval decision missing")?)?;
            serde_json::from_value::<Decision>(value.get("decision").cloned().context("approval decision missing")?)?
        } else {
            let (outcome, mut rx) = watch::channel(None);
            let proposed = Arc::new(Entry {
                ledger: ledger.clone(),
                pending: pending.clone(),
                claimed: AtomicBool::new(false),
                outcome,
                cancel: self.cancellation.child_token(),
            });
            let (entry, fresh) = {
                let mut entries = self.approvals.entries.lock();
                match entries.entry(id.clone()) {
                    std::collections::hash_map::Entry::Occupied(old) => (Arc::clone(old.get()), false),
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(Arc::clone(&proposed));
                        (proposed, true)
                    }
                }
            };
            if fresh {
                let waiter = Arc::clone(&entry);
                tokio::spawn(async move {
                    let seconds = (waiter.pending.expires_at() - chrono::Utc::now())
                        .to_std()
                        .unwrap_or_default();
                    let selected = tokio::select! {
                        biased;
                        ()=waiter.cancel.cancelled()=>Some(Decision::Cancelled),
                        ()=tokio::time::sleep(seconds)=>Some(Decision::Expired),
                        _=rx.changed()=>None,
                    };
                    if let Some(decision) = selected {
                        if waiter.finish(decision).await.is_err() {
                            tracing::warn!("rf approval terminal write failed");
                        }
                    }
                });
            }
            let _pause = crate::agent::idle::pause_for_approval();
            let mut rx = entry.outcome.subscribe();
            let decided = loop {
                let observed = *rx.borrow_and_update();
                if let Some(value) = observed {
                    break value;
                }
                rx.changed().await.context("approval wait closed")?;
            };
            self.approvals.entries.lock().remove(&id);
            decided
        };
        if decision == Decision::Cancelled {
            return Ok(ToolApprovalDecision::Cancelled {
                reason: "approval cancelled".to_owned(),
            });
        }
        if decision != Decision::Approved {
            return Ok(ToolApprovalDecision::Denied {
                cause: if decision == Decision::Expired {
                    crate::tools::ApprovalDenialCause::NoAnswer
                } else {
                    crate::tools::ApprovalDenialCause::UserRejected
                },
                reason: format!("approval {}", decision.as_str()),
            });
        }
        let grant = port
            .claim_if_approved(
                &request.context.envelope.resolved_owner_id(),
                &request.context.envelope.session_key,
                pending.authority(),
            )
            .await?
            .context("approved call grant unavailable")?;
        Ok(ToolApprovalDecision::Approved {
            runtime_approval_granted: true,
            runtime_grant: Some(serde_json::to_value(
                crate::security::policy::ApprovalGrant::from_verified_v2(
                    &request.descriptor.public_name,
                    "rf_run_approval",
                    grant,
                ),
            )?),
        })
    }
}

pub(crate) async fn decide(
    state: &super::AppState,
    owner: &str,
    session: &str,
    id: &str,
    body: Value,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse};
    let error = |status, code, message| super::api::capabilities::coded_error(status, code, message);
    let decision = match body
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.get("decision"))
        .and_then(Value::as_str)
    {
        Some("approved") => Decision::Approved,
        Some("denied") => Decision::Denied,
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "field_invalid",
                "decision must be approved or denied",
            );
        }
    };
    let entry = state.rf_approvals.entries.lock().get(id).cloned();
    let Some(entry) = entry else {
        return recorded(state, owner, session, id, decision).await;
    };
    let Some(rf) = entry.pending.rf.as_ref() else {
        return error(StatusCode::NOT_FOUND, "approval_unknown", "approval unknown");
    };
    if entry.pending.subject().owner_id != owner || rf.root_session != session {
        return error(StatusCode::CONFLICT, "approval_run_mismatch", "approval scope differs");
    }
    let selected = if entry.cancel.is_cancelled() {
        Decision::Cancelled
    } else if entry.pending.expires_at() <= chrono::Utc::now() {
        Decision::Expired
    } else {
        decision
    };
    match entry.finish(selected).await {
        Ok(Decision::Expired) => error(StatusCode::GONE, "approval_expired", "approval expired"),
        Ok(actual) if actual == decision => {
            axum::Json(json!({"approval_id":id,"run_id":entry.pending.authority().run_id,"decision":actual.as_str()}))
                .into_response()
        }
        Ok(_) => error(StatusCode::CONFLICT, "approval_decided", "approval already decided"),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "approval could not be committed",
        ),
    }
}
async fn recorded(
    state: &super::AppState,
    owner: &str,
    session: &str,
    id: &str,
    decision: Decision,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse};
    let error = |status, code, message| super::api::capabilities::coded_error(status, code, message);
    let Ok(authority) = RunCallAuthority::parse_call_id(id) else {
        return error(StatusCode::NOT_FOUND, "approval_unknown", "approval unknown");
    };
    let workspace = state.config.load_full().workspace_dir.to_string_lossy().into_owned();
    let load = async {
        let pending = state
            .mem
            .find_message_event_by_idempotency_key(&workspace, &authority.pending_key())
            .await?;
        let resolved = state
            .mem
            .find_message_event_by_idempotency_key(&workspace, &format!("rf-decision:{}", authority.call_id()))
            .await?;
        Ok::<_, anyhow::Error>((pending, resolved))
    }
    .await;
    let (pending, resolved) = match load {
        Ok((Some(pending), resolved)) => (pending, resolved),
        Ok((None, _)) => return error(StatusCode::NOT_FOUND, "approval_unknown", "approval unknown"),
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
                "approval lookup failed",
            );
        }
    };
    let raw: Value = pending
        .raw_payload_json
        .as_deref()
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default();
    if pending.owner_id.as_deref() != Some(owner)
        || raw.pointer("/record/rf/root_session").and_then(Value::as_str) != Some(session)
    {
        return error(StatusCode::CONFLICT, "approval_run_mismatch", "approval scope differs");
    }
    let value: Value = resolved
        .and_then(|e| e.raw_payload_json)
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default();
    match value.get("decision").and_then(Value::as_str) {
        Some("expired") => error(StatusCode::GONE, "approval_expired", "approval expired"),
        Some(actual) if actual == decision.as_str() => {
            axum::Json(json!({"approval_id":id,"run_id":authority.run_id,"decision":actual})).into_response()
        }
        Some(_) => error(StatusCode::CONFLICT, "approval_decided", "approval already decided"),
        None => error(
            StatusCode::CONFLICT,
            "approval_run_mismatch",
            "approval run is no longer live",
        ),
    }
}
