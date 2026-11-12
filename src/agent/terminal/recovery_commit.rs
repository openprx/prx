//! AUTH-02-3a/3b terminal commit: claim v3, authenticated outbox, cost projection.

use super::oversized_reply::{ReplyReference, compact_reply};
use super::replay::PreparedReplay;
use super::{
    TurnTerminalCommit, TurnTerminalPayload, TurnTerminalReceipt, TurnTerminalStatus, ensure_runtime_authority,
    redact_persisted_json, replay_requires_journal, skill_awaiting_approval, skill_observations, skill_task_key,
    terminal_semantic_scope, verify_terminal_replay, verify_terminal_replay_journal,
};
use crate::causal_tree::outcome::{OutcomeCorrelator, TerminalFacts};
use crate::causal_tree::store::Enqueue;
use crate::config::schema::CostConfig;
use crate::cost::types::CostSettlement;
use crate::decision::KeyDomain;
use crate::llm::route_decision::{MeteredTokenUsageRecord, UsagePrice, usage_cost_resolution};
use crate::memory::fabric::MemoryFabric;
use crate::memory::recovery_authority::{
    RecoveryChain, RecoveryCostIntent, RecoveryCostRow, RecoveryCostState, RecoveryEvidence, RecoveryMacDomain,
    RecoveryPricing, RecoveryPublication, RecoveryTerminalCommit, RecoveryTerminalOutcome,
};
use crate::memory::{Memory, MessageEventInput, MessageEventScope};
use crate::runtime::ingress_recovery::{SettledTerminalFacts, SkillObservation};
use anyhow::Context;
use std::path::Path;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[cfg(test)]
thread_local! {
    static BEFORE_INLINE_PROJECTION: std::cell::RefCell<Option<Box<dyn FnOnce() + Send>>> = const { std::cell::RefCell::new(None) };
    static SKIP_INLINE_PROJECTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CRASH_BEFORE_ACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
struct ProjectionAckGuard(bool);
#[cfg(test)]
impl Drop for ProjectionAckGuard {
    fn drop(&mut self) {
        CRASH_BEFORE_ACK.with(|cell| cell.set(self.0));
    }
}

#[cfg(test)]
pub(crate) fn skip_inline_projection(skip: bool) {
    SKIP_INLINE_PROJECTION.with(|cell| cell.set(skip));
}

#[cfg(test)]
pub(crate) fn crash_before_ack(crash: bool) {
    CRASH_BEFORE_ACK.with(|cell| cell.set(crash));
}

#[cfg(test)]
pub(crate) fn before_inline_projection(hook: Box<dyn FnOnce() + Send>) {
    BEFORE_INLINE_PROJECTION.with(|cell| *cell.borrow_mut() = Some(hook));
}

pub(super) struct ReplyAndUsage {
    pub(super) reply_reference: Option<ReplyReference>,
    pub(super) usage_settlement: Option<MeteredTokenUsageRecord>,
}

pub(super) async fn finalize_recovery_terminal(
    fabric: &MemoryFabric,
    commit: TurnTerminalCommit,
    cost_config: &CostConfig,
    workspace_dir: &Path,
    stop_after_marker: bool,
    mut execution_trace: Option<crate::tools::capability::execution_trace::ExecutionTraceSnapshot>,
    mut prepared_replay: Option<PreparedReplay>,
    reply_and_usage: ReplyAndUsage,
) -> anyhow::Result<TurnTerminalReceipt> {
    let ReplyAndUsage {
        reply_reference: original_reference,
        usage_settlement,
    } = reply_and_usage;
    ensure_runtime_authority(&commit.scope, "projection").await?;
    if let Some(outcome) = commit.provider_outcome.as_ref() {
        crate::llm::route_decision::record_provider_outcome_events(fabric, commit.scope.clone(), outcome).await?;
    }
    ensure_runtime_authority(&commit.scope, "terminal-marker").await?;

    let memory = fabric.memory();
    let existing_claim = memory
        .find_message_event_by_idempotency_key(
            fabric.workspace_id(),
            &format!("turn:{}:decision-claim", commit.terminal_id),
        )
        .await?;
    let existing_payload = existing_claim
        .as_ref()
        .map(|event| {
            serde_json::from_str::<serde_json::Value>(
                event
                    .raw_payload_json
                    .as_deref()
                    .context("recovery terminal claim has no payload")?,
            )
            .context("recovery terminal claim payload is invalid")
        })
        .transpose()?;
    let recorded = commit.decision.is_recorded();

    if !recorded {
        anyhow::ensure!(
            commit
                .scope
                .causation_event_id
                .as_deref()
                .is_some_and(|id| !id.is_empty()),
            "unrecorded terminal claim requires causation_event_id"
        );
    }

    let prepared_parameters = commit.decision.context.prepared_main_parameters();
    let redacted_usage = usage_settlement
        .as_ref()
        .map(crate::security::redaction::redact_json_structure)
        .transpose()?;
    let usage_json = serde_json::to_vec(&redacted_usage)?;
    let mac_memory = std::sync::Arc::clone(&memory);
    let mac_commit = commit.clone();
    let mac_workspace = fabric.workspace_id().to_owned();
    let mac_trace = execution_trace.clone();
    let mac_parameters = prepared_parameters.clone();
    let (key_id, semantic, reply_reference, semantic_bytes, semantic_mac_v3, usage_digest) =
        crate::runtime::blocking::spawn_blocking(move || {
            let repository = mac_memory
                .recovery_repository()
                .context("recovery repository unavailable")?;
            let active_id = repository.activate_recovery_authority()?;
            let key_id = match existing_payload.as_ref() {
                Some(payload) => payload
                    .get("recovery_key_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .context("recovery terminal claim key is missing")?,
                None => active_id,
            };
            let (semantic, reply_reference) = claim_semantic(
                &mac_commit,
                &mac_workspace,
                recorded,
                mac_trace.as_ref(),
                &mac_parameters,
                &key_id,
                original_reference,
            )?;
            let semantic_bytes = serde_json::to_vec(&semantic)?;
            let mut claim_preimage = semantic_bytes.clone();
            claim_preimage.extend_from_slice(key_id.as_bytes());
            if let Some(payload) = existing_payload.as_ref() {
                let bytes = hex::decode(
                    payload
                        .get("semantic_mac_v3")
                        .and_then(serde_json::Value::as_str)
                        .context("recovery terminal claim mac is missing")?,
                )?;
                let tag = crate::memory::recovery_authority::RecoveryTag {
                    key_id: key_id.clone(),
                    tag: bytes
                        .as_slice()
                        .try_into()
                        .context("recovery terminal claim mac length")?,
                };
                repository
                    .verify_recovery_mac(&tag, RecoveryMacDomain::ClaimV3, &claim_preimage)
                    .context("recovery terminal commit conflict")?;
            }
            let claim = hex::encode(
                repository
                    .recovery_mac(Some(&key_id), RecoveryMacDomain::ClaimV3, &claim_preimage)?
                    .tag,
            );
            let usage = hex::encode(
                repository
                    .recovery_mac(Some(&key_id), RecoveryMacDomain::UsageDigestV1, &usage_json)?
                    .tag,
            );
            Ok::<_, anyhow::Error>((key_id, semantic, reply_reference, semantic_bytes, claim, usage))
        })
        .await??;
    let semantic_mac_v2 = if recorded {
        let recorded_link = commit
            .decision
            .recorded
            .as_ref()
            .context("recorded decision is missing")?;
        Some(hex::encode(recorded_link.key.hmac(
            KeyDomain::TerminalClaim,
            &[b"terminal-claim-v2", &semantic_bytes],
        )))
    } else {
        None
    };

    let claim_id = terminal_event_id(
        memory.as_ref(),
        fabric.workspace_id(),
        &commit.terminal_id,
        "decision-claim",
    )
    .await?;
    let assistant_id = if commit.history.is_some() && commit.status == TurnTerminalStatus::Completed {
        Some(terminal_event_id(memory.as_ref(), fabric.workspace_id(), &commit.terminal_id, "assistant").await?)
    } else {
        None
    };
    let usage_id = if usage_settlement.is_some() {
        Some(terminal_event_id(memory.as_ref(), fabric.workspace_id(), &commit.terminal_id, "usage").await?)
    } else {
        None
    };
    let final_id = terminal_event_id(memory.as_ref(), fabric.workspace_id(), &commit.terminal_id, "final").await?;

    let claim_payload = {
        let mut payload = serde_json::json!({
            "terminal_id": commit.terminal_id,
            "semantic_mac_v3": semantic_mac_v3,
            "recovery_key_id": key_id,
        });
        if recorded {
            payload.as_object_mut().context("claim payload")?.insert(
                "decision_id".into(),
                serde_json::Value::String(commit.decision.decision_id.to_string()),
            );
        }
        if let Some(mac) = semantic_mac_v2 {
            payload
                .as_object_mut()
                .context("claim payload")?
                .insert("semantic_mac_v2".into(), serde_json::Value::String(mac));
        }
        if reply_reference.is_some() {
            payload
                .as_object_mut()
                .context("claim payload")?
                .insert("semantic".into(), semantic.clone());
        }
        payload.to_string()
    };

    let pricing = recovery_pricing(usage_settlement.as_ref(), cost_config);
    let cost_settlement = match pricing {
        RecoveryPricing::Metered => Some(CostSettlement::ProjectionPending),
        RecoveryPricing::Subscription => usage_settlement.as_ref().map(|usage| CostSettlement::Subscription {
            provider: usage.provider.clone(),
            model: usage.model.clone(),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        }),
        RecoveryPricing::Unknown => usage_settlement.is_some().then_some(CostSettlement::UnknownPricing),
    };

    let decision_id = recorded.then(|| commit.decision.decision_id.to_string());
    let payload = TurnTerminalPayload {
        terminal_id: &commit.terminal_id,
        decision_id: decision_id.as_deref(),
        model_catalog_revision: commit
            .decision
            .context
            .model_catalog()
            .map(|catalog| catalog.revision.as_str()),
        model_catalog_content_sha256: commit
            .decision
            .context
            .model_catalog()
            .map(crate::providers::catalog::ReleaseCatalog::content_sha256),
        status: commit.status,
        history: &commit.history,
        history_scope: commit.history_scope.as_ref().map(terminal_semantic_scope),
        execution_trace: execution_trace.as_ref(),
        assistant_event_id: assistant_id.as_deref(),
        provider_outcome: &commit.provider_outcome,
        prepared_parameters: &prepared_parameters,
        usage_settlement: &usage_settlement,
        cost_settlement: &cost_settlement,
        telemetry: &commit.telemetry,
        delivery_intent: &commit.delivery_intent,
        attempt_id: commit.scope.attempt_id.as_deref(),
        lease_epoch: commit.scope.lease_epoch,
    };
    let mut payload_value = redact_persisted_json(serde_json::to_value(&payload)?);
    if let (Some(reference), Some(history)) = (&reply_reference, &commit.history) {
        compact_reply(&mut payload_value, &history.assistant_content);
        payload_value
            .as_object_mut()
            .context("terminal payload")?
            .insert("replay_unavailable".into(), serde_json::to_value(reference)?);
    }

    let mut events = Vec::new();
    events.push(runtime_event(
        fabric,
        &commit.scope,
        "turn.decision.claimed",
        format!("terminal_id={}", commit.terminal_id),
        Some(claim_payload),
        format!("turn:{}:decision-claim", commit.terminal_id),
        Some(claim_id.clone()),
    ));
    if let (Some(history), Some(id)) = (commit.history.as_ref(), assistant_id.as_ref()) {
        if commit.status == TurnTerminalStatus::Completed {
            let scope = commit.history_scope.clone().unwrap_or_else(|| commit.scope.clone());
            let assistant = assistant_event(
                fabric,
                scope,
                history.assistant_content.clone(),
                format!("turn:{}:assistant", commit.terminal_id),
                id.clone(),
            );
            events.push(assistant);
        }
    }
    if let (Some(usage), Some(id)) = (usage_settlement.as_ref(), usage_id.as_ref()) {
        events.push(runtime_event(
            fabric,
            &commit.scope,
            "usage.settled",
            format!("settlement_id={}", commit.terminal_id),
            Some(serde_json::to_string(usage)?),
            format!("turn:{}:usage", commit.terminal_id),
            Some(id.clone()),
        ));
    }
    events.push(runtime_event(
        fabric,
        &commit.scope,
        "turn.finalized",
        format!("terminal_id={} status={:?}", commit.terminal_id, commit.status),
        Some(serde_json::to_string(&payload_value)?),
        format!("turn:{}:final", commit.terminal_id),
        Some(final_id.clone()),
    ));

    let status = match commit.status {
        TurnTerminalStatus::Completed => "completed",
        TurnTerminalStatus::Silent => "silent",
        TurnTerminalStatus::Failed => "failed",
        TurnTerminalStatus::Cancelled => "cancelled",
    };
    let surface = commit
        .decision
        .context
        .audit_hops()
        .last()
        .map(|hop| hop.state.trusted.parts().surface.as_str().to_owned())
        .context("terminal decision has no surface")?;
    let parent_authority_id = commit
        .decision
        .parent_grant
        .as_ref()
        .map(|grant| grant.scope().gate_inputs().0.parent_authority_id.to_string());
    let (skill_status, awaiting_approval, verified) = recovered_skill(&commit, execution_trace.as_ref());
    let settled = SettledTerminalFacts {
        v: 1,
        terminal_id: commit.terminal_id.clone(),
        claim_event_id: claim_id.clone(),
        assistant_event_id: assistant_id.clone(),
        usage_event_id: usage_id.clone(),
        final_event_id: final_id.clone(),
        semantic_mac_v3,
        status: status.to_owned(),
        surface,
        parent_authority_id,
        pricing: pricing.as_str().to_owned(),
        usage_digest,
        key_id: key_id.clone(),
        skill_observation: SkillObservation {
            status: skill_status,
            awaiting_approval,
            verified,
        },
    };
    let publication_facts = serde_json::to_vec(&settled)?;
    let owner_id = commit.scope.owner_id.clone().context("terminal owner is missing")?;
    let chain = RecoveryChain {
        workspace_id: fabric.workspace_id().to_owned(),
        owner_id: owner_id.clone(),
        source: commit.scope.source.as_str().to_owned(),
        run_id: commit
            .scope
            .run_id
            .clone()
            .unwrap_or_else(|| commit.terminal_id.clone()),
    };
    let outbox_facts = serde_json::to_vec(&serde_json::json!({
        "v": 1,
        "terminal_id": commit.terminal_id,
        "status": status,
        "pricing": pricing.as_str(),
        "usage": redacted_usage,
        "usage_event_id": usage_id,
        "final_event_id": final_id,
    }))?;
    let terminal_commit = RecoveryTerminalCommit {
        events,
        ingress: None,
        publication: RecoveryPublication {
            chain: chain.clone(),
            input: None,
            terminal: Some(RecoveryEvidence {
                event_id: final_id.clone(),
                record: Vec::new(),
            }),
            facts: publication_facts,
        },
        outbox: RecoveryCostIntent {
            pricing,
            facts: outbox_facts,
        },
    };

    let deadline = Instant::now() + Duration::from_secs(15);
    let outcome = if let Some(prepared) = prepared_replay.take() {
        let recorded_link = commit
            .decision
            .recorded
            .as_ref()
            .context("recorded decision is missing")?;
        let key = recorded_link.key.clone();
        crate::memory::sqlite::with_after_prefix_async(
            Box::new(move |prefix, final_input| {
                let claim = prefix
                    .iter()
                    .find(|event| event.event_type == "turn.decision.claimed")
                    .context("recovery terminal claim is missing")?;
                let assistant = prefix.iter().find(|event| event.role == "assistant");
                let mut prepared = prepared;
                prepared.bind_claim(&key, claim)?;
                let mut payload: serde_json::Value = serde_json::from_str(
                    final_input
                        .raw_payload_json
                        .as_deref()
                        .context("terminal marker has no payload")?,
                )?;
                prepared.seal(&key, &mut payload, assistant)?;
                final_input.raw_payload_json = Some(serde_json::to_string(&payload)?);
                Ok(())
            }),
            memory.commit_recovery_terminal(terminal_commit, deadline),
        )
        .await?
    } else {
        memory.commit_recovery_terminal(terminal_commit, deadline).await?
    };

    let events = match outcome {
        RecoveryTerminalOutcome::Committed { events, .. } | RecoveryTerminalOutcome::Replayed { events, .. } => events,
        RecoveryTerminalOutcome::Conflict => anyhow::bail!("recovery terminal commit conflict"),
    };

    if pricing == RecoveryPricing::Metered {
        let skip_inline = {
            #[cfg(test)]
            {
                SKIP_INLINE_PROJECTION.with(std::cell::Cell::get)
            }
            #[cfg(not(test))]
            {
                false
            }
        };
        if !skip_inline {
            let memory = fabric.memory();
            let workspace_id = fabric.workspace_id().to_owned();
            let workspace_dir = workspace_dir.to_path_buf();
            let cost_config = cost_config.clone();
            let settlement_id = commit.terminal_id.clone();
            #[cfg(test)]
            let faults = crate::cost::tracker::take_recovery_projection_faults();
            #[cfg(test)]
            let crash_ack = CRASH_BEFORE_ACK.with(std::cell::Cell::get);
            #[cfg(test)]
            let before_projection = BEFORE_INLINE_PROJECTION.with(|cell| cell.borrow_mut().take());
            crate::runtime::blocking::spawn_blocking(move || {
                #[cfg(test)]
                let _faults = faults.install();
                #[cfg(test)]
                let _ack = ProjectionAckGuard(CRASH_BEFORE_ACK.with(|cell| cell.replace(crash_ack)));
                let repository = memory
                    .recovery_repository()
                    .context("recovery repository unavailable")?;
                #[cfg(test)]
                if let Some(hook) = before_projection { hook(); }
                // The commit inserted the row; an authenticated finish can remove it from the retryable set.
                find_pending_cost(repository, &workspace_id, &settlement_id)?.map_or_else(
                    || Ok(()),
                    |row| project_committed_usage(memory.as_ref(), &workspace_id, &workspace_dir, &cost_config, &row),
                )
            })
            .await
            .context("recovery cost projection worker failed")
            .and_then(|result| result)
            .unwrap_or_else(|_error| tracing::warn!(settlement_id = %commit.terminal_id, "recovery cost projection deferred to the restart scan"));
        }
    }

    let terminal_event = events
        .iter()
        .find(|event| event.event_type == "turn.finalized")
        .cloned()
        .context("recovery terminal marker is missing")?;
    let assistant_event = events.iter().find(|event| event.role == "assistant").cloned();

    let mut journal_selected = None;
    if recorded {
        let recorded_link = commit
            .decision
            .recorded
            .as_ref()
            .context("recorded decision is missing")?;
        let owner = terminal_event
            .owner_id
            .as_deref()
            .context("terminal owner is missing")?;
        let journal = replay_requires_journal(&recorded_link.key, &terminal_event, None)?;
        let verified_replay = if journal {
            verify_terminal_replay_journal(
                fabric.memory().as_ref(),
                workspace_dir,
                &recorded_link.key,
                &recorded_link.store_workspace_id,
                fabric.workspace_id(),
                owner,
                &commit.terminal_id,
                &terminal_event,
            )
            .await?
        } else {
            verify_terminal_replay(
                fabric.memory().as_ref(),
                &recorded_link.reader,
                &recorded_link.key,
                &recorded_link.store_workspace_id,
                fabric.workspace_id(),
                owner,
                &commit.terminal_id,
                &terminal_event,
            )
            .await?
        };
        anyhow::ensure!(
            journal || verified_replay.source().source != crate::memory::MessageEventSource::SessionWorker,
            "worker segment requires launch provenance for legacy selection"
        );
        journal_selected = Some(journal);
        recorded_link
            .child_outcome_hook
            .persist(&verified_replay, &terminal_event, None, None)
            .await?;
    }

    let awaiting = skill_awaiting_approval(&commit.decision, execution_trace.as_ref());
    let observations = skill_observations(
        &commit.decision,
        execution_trace.as_ref(),
        commit.provider_outcome.as_ref(),
    );
    if !awaiting {
        commit.decision.context.retire_recursive_allowance();
    }
    super::release_skill_lifecycle(
        workspace_dir,
        commit.scope.owner_id.clone(),
        skill_task_key(&commit.scope),
        &commit.terminal_id,
        commit.status,
        awaiting,
        commit.scope.parent_run_id.is_none(),
        observations,
    )
    .await;
    let receipt = TurnTerminalReceipt {
        terminal_id: commit.terminal_id.clone(),
        terminal_event,
        assistant_event,
        usage_settlement,
        cost_settlement,
        delivery_intent: commit.delivery_intent.clone(),
    };
    if stop_after_marker {
        return Ok(receipt);
    }
    if recorded {
        let recorded_link = commit
            .decision
            .recorded
            .as_ref()
            .context("recorded decision is missing")?;
        let facts = TerminalFacts::from_receipt(&commit.decision.context, &receipt)?;
        let queued =
            OutcomeCorrelator::settle(&recorded_link.writer, &commit.decision.context, &facts)? == Enqueue::Queued;
        if journal_selected == Some(true) {
            if !queued {
                tracing::warn!("decision outcome projection could not be queued after terminal commit");
            }
        } else {
            anyhow::ensure!(queued, "decision outcome could not be queued after terminal commit");
        }
    }
    let _ = execution_trace.take();
    Ok(receipt)
}

fn recovered_skill(
    commit: &TurnTerminalCommit,
    trace: Option<&crate::tools::capability::execution_trace::ExecutionTraceSnapshot>,
) -> (TurnTerminalStatus, bool, bool) {
    if !commit.decision.is_recorded() {
        return (commit.status, false, false);
    }
    let facts = TerminalFacts::from_live(
        commit.terminal_id.clone(),
        commit.status,
        commit.provider_outcome.clone(),
        trace.cloned(),
    );
    let kind = match commit.decision.context.directive() {
        crate::causal_tree::directive::TurnExecutionDirective::RequestApproval { .. } => "request_approval",
        _ => "",
    };
    facts.recovered_signals(kind)
}

fn recovery_pricing(usage: Option<&MeteredTokenUsageRecord>, cost_config: &CostConfig) -> RecoveryPricing {
    let Some(usage) = usage else {
        return RecoveryPricing::Unknown;
    };
    match usage_cost_resolution(
        &usage.provider,
        &usage.model,
        usage.prompt_tokens,
        usage.completion_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens,
        cost_config,
    ) {
        UsagePrice::Priced(_) => RecoveryPricing::Metered,
        UsagePrice::Subscription => RecoveryPricing::Subscription,
        UsagePrice::Missing | UsagePrice::Invalid => RecoveryPricing::Unknown,
    }
}

fn claim_semantic(
    commit: &TurnTerminalCommit,
    fabric_workspace: &str,
    recorded: bool,
    execution_trace: Option<&crate::tools::capability::execution_trace::ExecutionTraceSnapshot>,
    prepared_parameters: &[crate::providers::catalog::parameters::ResolvedParameters],
    key_id: &str,
    original_reference: Option<ReplyReference>,
) -> anyhow::Result<(serde_json::Value, Option<ReplyReference>)> {
    let mut claim_outcome = commit.provider_outcome.clone();
    if let Some(outcome) = claim_outcome.as_mut() {
        outcome.redact_provider_labels();
    }
    let semantic = if recorded {
        let trusted = commit
            .decision
            .context
            .audit_hops()
            .last()
            .context("decision has no final hop")?
            .state
            .trusted
            .parts();
        redact_persisted_json(serde_json::json!({
            "version": 2,
            "terminal_id": commit.terminal_id,
            "decision_id": commit.decision.decision_id.to_string(),
            "execution_trace": execution_trace,
            "decision_chain": commit.decision.context.hop_ids().iter().map(ToString::to_string).collect::<Vec<_>>(),
            "workspace_id": trusted.workspace_id,
            "source_event_id": trusted.source_event_id,
            "catalog_generation": trusted.catalog_generation.to_hex(),
            "model_catalog_revision": commit.decision.context.model_catalog().map(|catalog| catalog.revision.as_str()),
            "model_catalog_content_sha256": commit.decision.context.model_catalog().map(crate::providers::catalog::ReleaseCatalog::content_sha256),
            "policy_version": trusted.policy_version,
            "status": commit.status,
            "scope": terminal_semantic_scope(&commit.scope),
            "history": commit.history,
            "history_scope": commit.history_scope.as_ref().map(terminal_semantic_scope),
            "provider_outcome": claim_outcome,
            "telemetry": commit.telemetry,
            "delivery_intent": commit.delivery_intent,
        }))
    } else {
        redact_persisted_json(serde_json::json!({
            "version": 2,
            "terminal_id": commit.terminal_id,
            "decision_id": serde_json::Value::Null,
            "execution_trace": serde_json::Value::Null,
            "decision_chain": serde_json::Value::Null,
            "workspace_id": fabric_workspace,
            "source_event_id": commit.scope.causation_event_id,
            "catalog_generation": serde_json::Value::Null,
            "model_catalog_revision": serde_json::Value::Null,
            "model_catalog_content_sha256": serde_json::Value::Null,
            "policy_version": serde_json::Value::Null,
            "status": commit.status,
            "scope": terminal_semantic_scope(&commit.scope),
            "history": commit.history,
            "history_scope": commit.history_scope.as_ref().map(terminal_semantic_scope),
            "provider_outcome": claim_outcome,
            "telemetry": commit.telemetry,
            "delivery_intent": commit.delivery_intent,
        }))
    };
    let mut semantic = semantic;
    if !prepared_parameters.is_empty() {
        semantic
            .as_object_mut()
            .context("terminal claim is not an object")?
            .insert(
                "prepared_parameters".to_owned(),
                redact_persisted_json(serde_json::to_value(prepared_parameters)?),
            );
    }
    let mut reference = None;
    let bound = crate::memory::recovery_authority::MAX_SERIALIZED_RECOVERY_FACTS;
    if serde_json::to_vec(&semantic)?.len().saturating_add(key_id.len()) > bound {
        if let Some(history) = commit
            .history
            .as_ref()
            .filter(|history| !history.assistant_content.is_empty())
        {
            let model = &commit
                .decision
                .context
                .audit_hops()
                .last()
                .context("decision has no final hop")?
                .state
                .trusted
                .parts()
                .resolved_model_id;
            let reply = match original_reference {
                Some(reference) => reference,
                None => ReplyReference::new(&history.assistant_content, model)?,
            };
            compact_reply(&mut semantic, &history.assistant_content);
            semantic
                .as_object_mut()
                .context("claim semantic object")?
                .insert("replay_unavailable".into(), serde_json::to_value(&reply)?);
            reference = Some(reply);
        }
        anyhow::ensure!(
            serde_json::to_vec(&semantic)?.len().saturating_add(key_id.len()) <= bound,
            "oversized recovery MAC input"
        );
    }
    Ok((semantic, reference))
}

#[cfg(test)]
pub(crate) async fn capture_parameters_for_test(draft: &mut super::TurnTerminalDraft) {
    draft
        .decision
        .as_ref()
        .expect("test: issued decision")
        .context
        .capture_main_parameters(true)
        .scope(async {
            crate::providers::parameter_capture::record(crate::providers::catalog::parameters::ResolvedParameters {
                model: "prx-local-model".into(),
                catalog_revision: "test-revision".into(),
                values: Default::default(),
            });
        })
        .await;
    let parameters = draft
        .decision
        .as_ref()
        .expect("test: issued decision")
        .context
        .prepared_main_parameters();
    draft
        .provider_outcome
        .as_mut()
        .expect("test: provider outcome")
        .prepared_parameters = parameters;
}

#[cfg(test)]
pub(crate) fn claim_semantic_for_test(
    commit: &super::TurnTerminalCommit,
    fabric_workspace: &str,
    recorded: bool,
    key_id: &str,
) -> anyhow::Result<serde_json::Value> {
    let prepared_parameters = commit.decision.context.prepared_main_parameters();
    let trace = recorded.then(|| commit.decision.context.terminal_execution_trace());
    Ok(claim_semantic(
        commit,
        fabric_workspace,
        recorded,
        trace.as_ref(),
        &prepared_parameters,
        key_id,
        None,
    )?
    .0)
}

fn runtime_event(
    fabric: &MemoryFabric,
    scope: &MessageEventScope,
    event_type: &str,
    content: String,
    raw_payload_json: Option<String>,
    idempotency_key: String,
    event_id: Option<String>,
) -> MessageEventInput {
    let payload = raw_payload_json.unwrap_or_else(|| serde_json::json!({ "event_type": event_type }).to_string());
    event_input(
        fabric,
        scope.clone(),
        "event",
        event_type,
        format!("{event_type} {content}"),
        Some(idempotency_key),
        Some(payload),
        event_id,
    )
}

fn assistant_event(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    content: String,
    idempotency_key: String,
    event_id: String,
) -> MessageEventInput {
    event_input(
        fabric,
        scope,
        "assistant",
        "message.created",
        content,
        Some(idempotency_key),
        None,
        Some(event_id),
    )
}

fn event_input(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    role: &str,
    event_type: &str,
    content: String,
    idempotency_key: Option<String>,
    raw_payload_json: Option<String>,
    event_id: Option<String>,
) -> MessageEventInput {
    MessageEventInput {
        agent_partition: scope.agent_partition,
        event_id,
        idempotency_key,
        workspace_id: fabric.workspace_id().to_owned(),
        owner_id: scope.owner_id,
        source: scope.source,
        channel: scope.channel,
        session_key: scope.session_key,
        parent_session_key: scope.parent_session_key,
        run_id: scope.run_id,
        parent_run_id: scope.parent_run_id,
        agent_id: scope.agent_id,
        persona_id: scope.persona_id,
        sender: scope.sender.map(crate::memory::fabric::redact_provider_label),
        recipient: scope.recipient.map(crate::memory::fabric::redact_provider_label),
        role: role.to_owned(),
        event_type: event_type.to_owned(),
        subject: scope.subject,
        goal_id: scope.goal_id,
        causation_event_id: scope.causation_event_id,
        correlation_id: scope.correlation_id,
        attempt_id: scope.attempt_id,
        lease_epoch: scope.lease_epoch,
        config_generation_id: scope.config_generation_id,
        config_source_revision: scope.config_source_revision,
        content,
        raw_payload_json,
        visibility: scope.visibility,
    }
}

fn find_pending_cost(
    repository: &dyn crate::memory::recovery_authority::RecoveryRepository,
    workspace_id: &str,
    settlement_id: &str,
) -> anyhow::Result<Option<RecoveryCostRow>> {
    let mut after: Option<String> = None;
    loop {
        let page = repository.pending_recovery_costs(
            workspace_id,
            after.as_deref(),
            crate::memory::ingress_recovery::MAX_RECOVERY_PAGE,
        )?;
        if page.is_empty() {
            return Ok(None);
        }
        if let Some(row) = page.iter().find(|row| row.settlement_id == settlement_id).cloned() {
            return Ok(Some(row));
        }
        if page.len() < crate::memory::ingress_recovery::MAX_RECOVERY_PAGE {
            return Ok(None);
        }
        after = page.last().map(|row| row.settlement_id.clone());
    }
}

fn project_committed_usage(
    memory: &dyn Memory,
    workspace_id: &str,
    workspace_dir: &Path,
    cost_config: &CostConfig,
    row: &RecoveryCostRow,
) -> anyhow::Result<()> {
    let repository = memory
        .recovery_repository()
        .context("recovery repository unavailable")?;
    let Some(usage) = usage_from_facts(&row.facts)? else {
        return Ok(());
    };
    let tracker = match crate::cost::tracker::CostTracker::for_workspace(cost_config.clone(), workspace_dir) {
        Ok(tracker) => tracker,
        Err(error) if is_blocked(&error) => {
            repository.finish_recovery_cost(
                workspace_id,
                &row.settlement_id,
                &row.usage_digest,
                RecoveryCostState::Blocked,
            )?;
            return Ok(());
        }
        Err(_) => {
            repository.finish_recovery_cost(
                workspace_id,
                &row.settlement_id,
                &row.usage_digest,
                RecoveryCostState::Error,
            )?;
            return Ok(());
        }
    };
    let projected = tracker.project_recovery_usage(&usage);
    let crash_before_ack = {
        #[cfg(test)]
        {
            CRASH_BEFORE_ACK.with(std::cell::Cell::get)
        }
        #[cfg(not(test))]
        {
            false
        }
    };
    if crash_before_ack {
        return Ok(());
    }
    #[cfg(test)]
    if projected
        .as_ref()
        .is_err_and(|error| error.is::<crate::cost::tracker::TestCrash>())
    {
        return Ok(());
    }
    let state = match projected {
        Ok(()) => RecoveryCostState::Acknowledged,
        Err(error) if is_blocked(&error) => RecoveryCostState::Blocked,
        Err(_) => RecoveryCostState::Error,
    };
    repository.finish_recovery_cost(workspace_id, &row.settlement_id, &row.usage_digest, state)
}

fn is_blocked(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<crate::cost::tracker::LedgerBlocked>())
}

/// Restart scan: project pending/error rows. Failures never fail prepare.
pub(crate) fn scan_pending_recovery_costs(
    memory: &dyn Memory,
    workspace_id: &str,
    workspace_dir: &Path,
    cost_config: &CostConfig,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let Some(repository) = memory.recovery_repository() else {
        return Ok(());
    };
    let rows =
        match repository.pending_recovery_costs(workspace_id, None, crate::memory::ingress_recovery::MAX_RECOVERY_PAGE)
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "recovery cost restart scan could not list pending rows");
                return Ok(());
            }
        };
    if rows.is_empty() || Instant::now() >= deadline {
        return Ok(());
    }
    let tracker = match crate::cost::tracker::CostTracker::for_workspace(cost_config.clone(), workspace_dir) {
        Ok(tracker) => tracker,
        Err(error) => {
            tracing::warn!(%error, "recovery cost restart scan could not open the ledger");
            let state = if is_blocked(&error) {
                RecoveryCostState::Blocked
            } else {
                RecoveryCostState::Error
            };
            for row in rows {
                if Instant::now() >= deadline {
                    break;
                }
                if row.pricing != RecoveryPricing::Metered {
                    continue;
                }
                if let Err(finish_error) =
                    repository.finish_recovery_cost(workspace_id, &row.settlement_id, &row.usage_digest, state)
                {
                    tracing::warn!(%finish_error, settlement_id = %row.settlement_id, "recovery cost finish failed");
                }
            }
            return Ok(());
        }
    };
    for row in rows {
        if Instant::now() >= deadline {
            break;
        }
        if row.pricing != RecoveryPricing::Metered {
            continue;
        }
        let usage = match usage_from_facts(&row.facts) {
            Ok(Some(usage)) => usage,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(%error, settlement_id = %row.settlement_id, "recovery cost row facts are unreadable");
                continue;
            }
        };
        let projected = tracker.project_recovery_usage(&usage);
        #[cfg(test)]
        if projected
            .as_ref()
            .is_err_and(|error| error.is::<crate::cost::tracker::TestCrash>())
        {
            continue;
        }
        let state = match projected {
            Ok(()) => RecoveryCostState::Acknowledged,
            Err(error) if is_blocked(&error) => RecoveryCostState::Blocked,
            Err(_) => RecoveryCostState::Error,
        };
        if let Err(error) = repository.finish_recovery_cost(workspace_id, &row.settlement_id, &row.usage_digest, state)
        {
            tracing::warn!(%error, settlement_id = %row.settlement_id, "recovery cost finish failed");
        }
    }
    Ok(())
}

fn usage_from_facts(facts: &[u8]) -> anyhow::Result<Option<MeteredTokenUsageRecord>> {
    let value: serde_json::Value = serde_json::from_slice(facts)?;
    match value.get("usage") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(usage) => Ok(Some(serde_json::from_value(usage.clone())?)),
    }
}

async fn terminal_event_id(
    memory: &dyn Memory,
    workspace: &str,
    terminal: &str,
    suffix: &str,
) -> anyhow::Result<String> {
    Ok(memory
        .find_message_event_by_idempotency_key(workspace, &format!("turn:{terminal}:{suffix}"))
        .await?
        .map_or_else(|| Uuid::new_v4().to_string(), |event| event.event_id))
}
