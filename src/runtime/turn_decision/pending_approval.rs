//! Authenticated pending approval metadata. No caller-supplied operation or
//! digest can create a record; issuance requires the service's gated context.
pub(super) mod collection;
mod h2;
mod sealed_call;
use super::{TurnDecisionContext, TurnDecisionService, TurnExecutionDirective};
use crate::acl::approval_grant::{ApprovalGrantV2, DecisionApprovalBinding, RiskLevel, Subject, WitnessKeyring};
use crate::causal_tree::ids::DecisionId;
use crate::causal_tree::store::DecisionScope;
use crate::decision::KeyDomain;
use crate::memory::MessageEvent;
use crate::runtime::turn_source::TurnSourceProof;
pub(crate) use crate::security::approval_ledger::pending::{
    DecisionApprovalReceipt, DecisionApprovalView, DecisionRevocationReceipt, PendingApprovalPage,
    VerifiedPendingDecisionApproval,
};
use crate::security::approval_ledger::pending::{PendingRecord, VerifiedDecisionApproval, scope_from_source};
use crate::security::approval_ledger::{ApprovalLedger, ApprovalRuntime};
use anyhow::Context;
use chrono::Utc;
use serde::Deserialize;
use std::sync::Arc;

/// A durable pending or declined collection result. Only writer/readback can construct it.
pub(super) struct CollectedApprovalResult {
    event: MessageEvent,
}
impl CollectedApprovalResult {
    pub(super) const fn event(&self) -> &MessageEvent {
        &self.event
    }
}

impl TurnDecisionService {
    async fn verify_pending_console_file_operation(
        &self,
        pending: &VerifiedPendingDecisionApproval,
    ) -> anyhow::Result<()> {
        if pending.source().source != crate::memory::MessageEventSource::Console
            || pending.record().sealed_call.is_none()
        {
            return Ok(());
        }
        let call = self.open_pending_call(pending)?;
        if !matches!(call.name.as_str(), "file_write" | "file_edit") {
            return Ok(());
        }
        let path = call
            .arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .context("pending Console file path missing")?;
        let filesystem = self
            .pending_console_filesystem(pending.source())
            .await?
            .context("pending Console filesystem unavailable")?;
        let (operation, risk) = self.scoped_file_operation(Some(&filesystem), &call.name, path).await?;
        anyhow::ensure!(
            pending.record().op_ids.as_slice() == [operation] && pending.record().approval_risk()? == risk,
            "pending Console file namespace or operation changed"
        );
        Ok(())
    }

    pub(crate) async fn inspect_pending_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<DecisionApprovalView> {
        self.approval_ledger()
            .inspect_pending_decision_approval(owner, session, decision_id)
            .await
    }

    /// Pending approvals for one owner, from the same ledger `inspect` reads.
    ///
    /// Foreign rows are not returned. A row that fails inspection, whose
    /// authenticated review denies the call, or whose operations are all
    /// already resolved, is omitted. An approved call that is still waiting
    /// to resume stays listed. `next_cursor` is the last durable id examined
    /// when the page stopped before the ledger ended.
    pub(crate) async fn list_pending_decision_approvals(
        &self,
        owner: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<PendingApprovalPage> {
        self.approval_ledger()
            .list_pending_decision_approvals(owner, after_id, limit)
            .await
    }

    pub(super) async fn record_pending_decision_approval(
        &self,
        proof: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
    ) -> anyhow::Result<()> {
        self.resolve_agent_model(proof)?;
        self.check_live_parity()?;
        let TurnExecutionDirective::RequestApproval { approval_scope } = &context.directive else {
            anyhow::bail!("decision has no pending approval scope");
        };
        anyhow::ensure!(
            context.source_actor_matches(Some(proof.policy_channel()), Some(proof.policy_sender()))
                && approval_scope.decision_id == context.decision_id()
                && approval_scope.owner == proof.owner_id()
                && approval_scope.workspace == proof.workspace_id()
                && approval_scope.session.as_deref() == Some(proof.session_key())
                && approval_scope.catalog_generation == *self.catalog.generation()
                && approval_scope.single_use
                && !approval_scope.op_ids.is_empty()
                && !approval_scope.capability_refs.is_empty()
                && approval_scope.expires_at > Utc::now(),
            "pending approval differs from its issued decision scope"
        );
        let fresh = crate::causal_tree::directive::directive_from_selection(
            &context.engine,
            &crate::causal_tree::directive::DirectiveContext {
                current_generation: *self.catalog.generation(),
                approval_expires_at: approval_scope.expires_at,
                discovery_limit: 1,
            },
        )?;
        anyhow::ensure!(
            matches!(fresh, TurnExecutionDirective::RequestApproval { approval_scope: fresh } if fresh == *approval_scope),
            "pending approval does not match the gated selection"
        );
        self.catalog.resolve(&approval_scope.capability_refs)?;
        let link = self.settlement_ref(Arc::clone(context))?;
        link.validate_durable_chain().await?;
        let source_id = proof.source_event_id().context("pending approval source missing")?;
        let source = self
            .memory
            .find_owned_message_event(proof.workspace_id(), proof.owner_id(), source_id)
            .await?
            .context("pending approval source is not durable")?;
        anyhow::ensure!(
            source.role == "user"
                && source.event_type == "message.created"
                && source.session_key.as_deref() == Some(proof.session_key())
                && source.run_id.as_deref() == Some(proof.run_id())
                && crate::runtime::turn_source::subject_task_id(source.subject.as_ref()) == proof.task_id()
                && context.source_actor_matches(source.channel.as_deref(), source.sender.as_deref())
                && crate::runtime::turn_source::recorded_input_matches(proof.surface(), &source, proof.routing_input()),
            "pending approval source differs from admitted input"
        );
        let store_workspace = self.store.workspace_id().context("approval decision store missing")?;
        let decision_scope = DecisionScope::Owner {
            workspace_id: Arc::clone(&store_workspace),
            owner_id: Arc::from(proof.owner_id()),
        };
        let mut decision_digests = Vec::with_capacity(context.hop_ids().len());
        for id in context.hop_ids() {
            let row = self
                .store
                .reader()
                .replay_record(&decision_scope, id)?
                .context("pending approval decision is not durable")?;
            decision_digests.push((id.to_string(), row.digest));
        }
        let principal = crate::memory::principal::OwnerPrincipal::new(
            proof.workspace_id(),
            proof.policy_channel(),
            proof.policy_sender(),
            proof.session_key(),
            vec![crate::memory::principal::Role::Anonymous],
        );
        let record = PendingRecord {
            h2_origin: proof.h2_entry_origin().map(|origin| Box::new(origin.record().clone())),
            subject: Subject {
                agent_id: format!("prx:agent:{}", proof.policy_channel()),
                principal_id: principal.principal_id,
                owner_id: proof.owner_id().to_owned(),
                workspace_id: proof.workspace_id().to_owned(),
                session_key: Some(proof.session_key().to_owned()),
            },
            binding: DecisionApprovalBinding {
                decision_id: context.decision_id().to_string(),
                catalog_generation: approval_scope.catalog_generation.0,
                call_digest: approval_scope
                    .call_digest
                    .context("pending approval has no proven call digest")?,
            },
            source_event_id: source.event_id.clone(),
            source_mac: self
                .state_key
                .hmac(KeyDomain::ApprovalState, &[b"source", &serde_json::to_vec(&source)?]),
            run_id: proof.run_id().to_owned(),
            decision_digests,
            policy_version: self.policy_version.to_string(),
            capabilities: approval_scope
                .capability_refs
                .iter()
                .map(|reference| {
                    (
                        reference.id.as_str().to_owned(),
                        reference.fingerprint.0,
                        reference.generation.0,
                    )
                })
                .collect(),
            op_ids: approval_scope.op_ids.iter().map(|op| op.as_str().to_owned()).collect(),
            effect: approval_scope.effect.as_str().to_owned(),
            risk: None,
            schema_dialect: None,
            sealed_call: None,
            expires_at: proof.h2_entry_origin().map_or(approval_scope.expires_at, |origin| {
                approval_scope.expires_at.min(origin.record().authority_expires_at)
            }),
        };
        let expires_at = record.expires_at;
        self.persist_pending_record(record, &source).await?;
        if let Some(owner) = context.recursive_allowance.get() {
            owner.retain_pending(expires_at);
        }
        Ok(())
    }

    async fn persist_pending_record(&self, record: PendingRecord, source: &MessageEvent) -> anyhow::Result<()> {
        self.persist_pending_record_receipt(record, source).await.map(|_| ())
    }

    async fn persist_collection_pending_record(
        &self,
        context: Arc<TurnDecisionContext>,
        record: PendingRecord,
        source: &MessageEvent,
    ) -> anyhow::Result<()> {
        let proof = context
            .approval_collection_proof()
            .context("collection proof missing")?;
        anyhow::ensure!(
            context.service_id == self.service_id
                && context
                    .approval_collection_started
                    .load(std::sync::atomic::Ordering::Acquire)
                && record.binding.decision_id == context.decision_id().to_string()
                && record.capabilities.as_slice()
                    == [(
                        proof.capability().id.as_str().to_owned(),
                        proof.capability().fingerprint.0,
                        proof.capability().generation.0
                    )]
                && record.subject.workspace_id == source.workspace_id
                && source.owner_id.as_deref() == Some(record.subject.owner_id.as_str())
                && record.subject.session_key == source.session_key
                && source.run_id.as_deref() == Some(record.run_id.as_str())
                && record.source_event_id == source.event_id
                && record.sealed_call.is_some()
                && !record.op_ids.is_empty(),
            "collection pending commitment mismatch"
        );
        let expires_at = record.expires_at;
        let event = self.persist_pending_record_receipt(record, source).await?;
        if let Some(owner) = context.recursive_allowance.get() {
            owner.retain_pending(expires_at);
        }
        context
            .collected_approval_result
            .set(Box::new(CollectedApprovalResult { event }))
            .map_err(|_| anyhow::anyhow!("collection pending receipt already installed"))
    }

    async fn persist_pending_record_receipt(
        &self,
        record: PendingRecord,
        source: &MessageEvent,
    ) -> anyhow::Result<MessageEvent> {
        self.approval_ledger()
            .persist_pending_record_receipt(record, source)
            .await
    }

    /// Only the authenticated owner can resolve this pending record. Review is
    /// one durable transition for the complete operation set; retries restore
    /// the exact signed grant batch instead of issuing new grants.
    pub(crate) async fn review_pending_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
        approved: bool,
    ) -> anyhow::Result<DecisionApprovalReceipt> {
        self.approval_ledger()
            .review_pending_decision_approval(owner, session, decision_id, approved)
            .await
    }
    async fn review_pending_with_local(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
        approved: bool,
        local_owner: Option<crate::runtime::h2_entry::LocalOwnerReviewStamp>,
    ) -> anyhow::Result<DecisionApprovalReceipt> {
        self.approval_ledger()
            .review_pending_with_local(owner, session, decision_id, approved, local_owner)
            .await
    }

    /// Read-only preparation: authenticate original pending provenance, the
    /// winning human review, and every separately issued grant. No use is
    /// consumed here; a later atomic resolution must still win before dispatch.
    pub(super) async fn load_reviewed_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<VerifiedDecisionApproval> {
        self.approval_ledger()
            .load_reviewed_decision_approval(owner, session, decision_id)
            .await
    }

    /// Read-only ingress preflight for a previously reviewed operation. This
    /// returns no grant or execution authority; dispatch must independently
    /// revalidate and win the durable atomic one-use resolution.
    pub(crate) async fn validate_pending_decision_resume(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<()> {
        self.approval_ledger()
            .validate_pending_decision_resume(owner, session, decision_id)
            .await
    }

    /// Revocation reads authenticated metadata even when a resolution already
    /// exists. The durable writer still requires that resolution to be the
    /// matching revocation; it never turns an existing consumption into one.
    pub(crate) async fn revoke_pending_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<DecisionRevocationReceipt> {
        self.approval_ledger()
            .revoke_pending_decision_approval(owner, session, decision_id)
            .await
    }

    pub(crate) async fn load_pending_decision_approval(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
    ) -> anyhow::Result<VerifiedPendingDecisionApproval> {
        self.approval_ledger()
            .load_pending_decision_approval(owner, session, decision_id)
            .await
    }
}

/// The execution adapter owns only a reference to the issued turn. It cannot
/// supply a new decision, operation set, actor or source event.
pub(super) struct ExecutionApprovalRecorder {
    pub(super) h2_origin: Option<Arc<crate::runtime::h2_entry::VerifiedEntryOrigin>>,
    pub(super) service: Arc<TurnDecisionService>,
    pub(super) context: Arc<TurnDecisionContext>,
}

#[async_trait::async_trait]
impl crate::tools::capability::execution_binding::DecisionApprovalRecorder for ExecutionApprovalRecorder {
    async fn file_operation(&self, tool: &str, path: &str) -> anyhow::Result<(String, RiskLevel)> {
        use std::sync::atomic::Ordering;
        anyhow::ensure!(
            matches!(tool, "file_write" | "file_edit")
                && self.context.service_id == self.service.service_id
                && !self.context.execution_closed.load(Ordering::Acquire),
            "file approval scope is unavailable"
        );
        let entry = self
            .service
            .catalog
            .entry_for_public_name(tool)
            .context("file capability missing")?;
        anyhow::ensure!(
            self.context
                .execution_refs
                .iter()
                .any(|reference| reference.id == entry.id
                    && reference.fingerprint == entry.fingerprint
                    && reference.generation == entry.generation)
                || self.context.discovery_admission(tool).is_some_and(|admission| {
                    admission.reference.id == entry.id
                        && admission.reference.fingerprint == entry.fingerprint
                        && admission.reference.generation == entry.generation
                }),
            "file capability was not selected or admitted from discovery"
        );
        self.service.check_live_parity()?;
        let operation = self
            .service
            .scoped_file_operation(self.context.owner_filesystem.as_ref(), tool, path)
            .await?;
        self.service.check_live_parity()?;
        anyhow::ensure!(
            !self.context.execution_closed.load(Ordering::Acquire),
            "file approval turn closed"
        );
        Ok(operation)
    }
    async fn record(
        &self,
        call: &crate::tools::capability::execution_binding::VerifiedDecisionCall,
    ) -> anyhow::Result<()> {
        let Some((record, source)) = self
            .service
            .prepare_pending_execution_call(&self.context, call, self.h2_origin.as_deref())
            .await?
        else {
            return Ok(());
        };
        let expires_at = record.expires_at;
        self.service
            .approval_ledger()
            .persist_pending_record_receipt(record, &source)
            .await?;
        if let Some(owner) = self.context.recursive_allowance.get() {
            owner.retain_pending(expires_at);
        }
        Ok(())
    }
}

impl TurnDecisionService {
    async fn prepare_pending_execution_call(
        &self,
        context: &Arc<TurnDecisionContext>,
        call: &crate::tools::capability::execution_binding::VerifiedDecisionCall,
        h2_origin: Option<&crate::runtime::h2_entry::VerifiedEntryOrigin>,
    ) -> anyhow::Result<Option<(PendingRecord, MessageEvent)>> {
        use std::sync::atomic::Ordering;
        let trusted = context.engine.state.trusted.parts();
        let scope = call.scope();
        let resolved_owner = scope.resolved_owner_id();
        anyhow::ensure!(
            !context.execution_closed.load(Ordering::Acquire)
                && context.execution_trace.get().is_some_and(|trace| !trace.is_sealed())
                && call.binding().decision_id == context.decision_id().to_string()
                && call.binding().catalog_generation == self.catalog.generation().0
                && (context.execution_refs.contains(call.capability())
                    || context.has_discovery_admission(call.capability()))
                && trusted.workspace_id.as_ref() == scope.workspace_id
                && trusted.owner_id.as_deref() == Some(resolved_owner.as_str())
                && trusted.session_key.as_deref() == Some(scope.session_key.as_str())
                && trusted.run_id.as_deref() == scope.run_id.as_deref()
                && trusted.task_id.as_deref() == scope.task_id.as_deref()
                && trusted.source_event_id.as_deref() == scope.source_message_event_id.as_deref()
                && context.source_actor_matches(scope.channel.as_deref(), scope.sender.as_deref()),
            "approval call differs from its active selected decision"
        );
        self.check_live_parity()?;
        self.settlement_ref(Arc::clone(context))?
            .validate_durable_chain()
            .await?;
        let entries = self.catalog.resolve(std::slice::from_ref(call.capability()))?;
        let entry = entries.first().context("approval capability missing")?;
        anyhow::ensure!(!call.operation().is_empty(), "approval operation scope missing");
        let source_id = scope
            .source_message_event_id
            .as_deref()
            .context("approval source missing")?;
        let owner = resolved_owner.as_str();
        let source = self
            .memory
            .find_owned_message_event(&scope.workspace_id, owner, source_id)
            .await?
            .context("approval source is not durable")?;
        anyhow::ensure!(
            source.role == "user"
                && source.event_type == "message.created"
                && source.session_key.as_deref() == Some(scope.session_key.as_str())
                && source.run_id == scope.run_id
                && crate::runtime::turn_source::subject_task_id(source.subject.as_ref()) == scope.task_id.as_deref()
                && source.config_generation_id == scope.config_generation_id
                && source.config_source_revision == scope.config_source_revision
                && context.source_actor_matches(source.channel.as_deref(), source.sender.as_deref())
                && crate::runtime::turn_source::recorded_input_matches(
                    trusted.surface,
                    &source,
                    context.engine.state.routing_input.as_str()
                ),
            "approval call source differs from admitted input"
        );
        let key = format!("decision-approval:{}:pending", context.decision_id());
        if self
            .memory
            .find_message_event_by_idempotency_key(&scope.workspace_id, &key)
            .await?
            .is_some()
        {
            let previous = self
                .load_pending_decision_approval(owner, &scope.session_key, context.decision_id())
                .await?;
            anyhow::ensure!(
                &previous.record().binding == call.binding(),
                "decision already awaits a different call"
            );
            return Ok(None);
        }
        let decision_scope = DecisionScope::Owner {
            workspace_id: self.store.workspace_id().context("approval store missing")?,
            owner_id: Arc::from(owner),
        };
        let mut decision_digests = Vec::with_capacity(context.hop_ids().len());
        for id in context.hop_ids() {
            let row = self
                .store
                .reader()
                .replay_record(&decision_scope, id)?
                .context("approval decision is not durable")?;
            decision_digests.push((id.to_string(), row.digest));
        }
        let channel = scope.channel.as_deref().context("approval channel missing")?;
        let sender = scope.sender.as_deref().context("approval sender missing")?;
        let principal = crate::memory::principal::OwnerPrincipal::new(
            &scope.workspace_id,
            channel,
            sender,
            &scope.session_key,
            vec![crate::memory::principal::Role::Anonymous],
        );
        let mut record = PendingRecord {
            h2_origin: h2_origin.map(|origin| Box::new(origin.record().clone())),
            subject: Subject {
                agent_id: format!("prx:agent:{channel}"),
                principal_id: principal.principal_id,
                owner_id: owner.to_owned(),
                workspace_id: scope.workspace_id.clone(),
                session_key: Some(scope.session_key.clone()),
            },
            binding: call.binding().clone(),
            source_event_id: source.event_id.clone(),
            source_mac: self
                .state_key
                .hmac(KeyDomain::ApprovalState, &[b"source", &serde_json::to_vec(&source)?]),
            run_id: scope.run_id.clone().context("approval run missing")?,
            decision_digests,
            policy_version: self.policy_version.to_string(),
            capabilities: vec![(
                call.capability().id.as_str().to_owned(),
                call.capability().fingerprint.0,
                call.capability().generation.0,
            )],
            op_ids: vec![call.operation().to_owned()],
            effect: entry.effect.as_str().to_owned(),
            risk: Some(call.risk()),
            schema_dialect: Some(call.schema_dialect()),
            sealed_call: None,
            expires_at: h2_origin.map_or_else(
                || Utc::now() + chrono::Duration::minutes(5),
                |origin| {
                    origin
                        .record()
                        .authority_expires_at
                        .min(Utc::now() + chrono::Duration::minutes(5))
                },
            ),
        };
        record.sealed_call = Some(self.seal_pending_call(&record, call.command())?);
        Ok(Some((record, source)))
    }
}

/// Source-authenticated, normalized original call, ready for a new constrained
/// resume decision. This is deliberately not an executor permit.
pub(super) struct PreparedDecisionResume {
    approval: VerifiedDecisionApproval,
    command: crate::tools::ToolExecutionCommand,
}

impl TurnDecisionService {
    pub(super) async fn prepare_decision_resume(
        &self,
        owner: &str,
        session: &str,
        decision_id: DecisionId,
        mut command: crate::tools::ToolExecutionCommand,
    ) -> anyhow::Result<PreparedDecisionResume> {
        use crate::security::approval_operation::{GrantOp, grant_op_for_call};
        use crate::tools::capability::execution_binding::{CallDigestSource, decision_call_digest};
        let approval = self
            .load_reviewed_decision_approval(owner, session, decision_id)
            .await?;
        let record = approval.pending().record();
        let source = approval.pending().source();

        let entry = self
            .catalog
            .entry_for_public_name(&command.name)
            .context("resume tool unavailable")?;
        anyhow::ensure!(
            record.capabilities.len() == 1
                && record
                    .capabilities
                    .first()
                    .is_some_and(|(id, schema, generation)| id == entry.id.as_str()
                        && *schema == entry.fingerprint.0
                        && *generation == entry.generation.0),
            "resume capability differs from pending call"
        );
        let descriptor = self
            .execution
            .current_descriptor(&command.name)
            .context("resume descriptor unavailable")?;
        anyhow::ensure!(
            crate::tools::capability::fingerprint::tool_spec_fingerprint(
                &descriptor.public_name,
                &descriptor.description,
                &descriptor.parameters
            ) == entry.fingerprint,
            "resume tool schema changed"
        );
        let dialect = record
            .schema_dialect
            .context("pending approval lacks actual-call schema provenance")?;
        crate::tools::execution::enforce_command_contract(&mut command.arguments, &descriptor, dialect)
            .map_err(anyhow::Error::msg)?;
        if record.sealed_call.is_some() {
            let stored = self.open_pending_call(approval.pending())?;
            anyhow::ensure!(
                stored.name == command.name && stored.arguments == command.arguments,
                "resume differs from the persisted original call"
            );
        }
        let (operation, risk) = match grant_op_for_call(&command.name, &command.arguments) {
            Some(GrantOp::Command(operation)) => (operation, RiskLevel::High),
            Some(GrantOp::ResourceExact(operation, risk)) => (operation, risk),
            Some(GrantOp::ResourceGlob(_, _)) => match command.name.as_str() {
                "message_send" => {
                    self.execution
                        .resolve_approval_operation(&command.name, &command.arguments)
                        .await?
                }
                "memory_store" => (
                    crate::security::approval_operation::memory_write_operation(None),
                    RiskLevel::Medium,
                ),
                "memory_forget" => {
                    let key = command
                        .arguments
                        .get("key")
                        .and_then(serde_json::Value::as_str)
                        .context("resume key missing")?;
                    (
                        crate::security::approval_operation::memory_delete_operation(None, key),
                        RiskLevel::High,
                    )
                }
                "file_write" | "file_edit" => {
                    let path = command
                        .arguments
                        .get("path")
                        .and_then(serde_json::Value::as_str)
                        .context("resume path missing")?;
                    let filesystem = self.pending_console_filesystem(source).await?;
                    self.scoped_file_operation(filesystem.as_ref(), &command.name, path)
                        .await?
                }
                _ => anyhow::bail!("resume operation is not resolved exactly"),
            },
            None => match entry.op_ids.as_slice() {
                [operation] => (operation.as_str().to_owned(), RiskLevel::High),
                _ => anyhow::bail!("resume operation is unavailable"),
            },
        };
        anyhow::ensure!(
            record.op_ids.as_slice() == [operation.as_str()] && risk == record.approval_risk()?,
            "resume operation or risk changed"
        );
        let capability = crate::causal_tree::branch::CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        let digest = decision_call_digest(
            &record.binding.decision_id,
            &capability,
            CallDigestSource {
                workspace: &record.subject.workspace_id,
                owner,
                session,
                run: source.run_id.as_deref(),
                event: Some(&source.event_id),
                channel: source.channel.as_deref(),
                sender: source.sender.as_deref(),
                generation: source.config_generation_id,
                revision: source.config_source_revision.as_deref(),
            },
            &command.arguments,
            &operation,
            risk,
        )?;
        anyhow::ensure!(
            digest == record.binding.call_digest,
            "resume arguments differ from approved call"
        );
        self.check_live_parity()?;
        let now = Utc::now();
        anyhow::ensure!(
            record.expires_at > now && approval.grants().iter().all(|grant| grant.is_time_valid(now)),
            "resume approval expired during preparation"
        );
        // The request supplies only the effect, never retry/ledger identity.
        command.operation_id = format!("decision-resume:{decision_id}");
        command.idempotency_key = Some(format!("decision-resume:{decision_id}"));
        Ok(PreparedDecisionResume { approval, command })
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
struct StoredDecisionResumeRequest {
    decision_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExplicitDecisionResumeRequest {
    decision_id: String,
    tool: String,
    arguments: serde_json::Value,
}

/// Fresh admitted request plus the authenticated original call. No execution
/// permission is exposed: re-decision and final durable claim still follow.
pub(super) struct AdmittedDecisionResume {
    prepared: PreparedDecisionResume,
    source: MessageEvent,
    receipt: MessageEvent,
    claimed: std::sync::OnceLock<MessageEvent>,
}

/// A successful durable claim, retained privately until the dedicated backend
/// adapter can install its exact-call scope. It cannot be cloned or serialized.
pub(super) struct ClaimedDecisionResume {
    command: crate::tools::ToolExecutionCommand,
    grant: ApprovalGrantV2,
    execution: super::approval::ResumeClaimScope,
    operation: String,
    service: Arc<TurnDecisionService>,
    context: Arc<TurnDecisionContext>,
}

pub(super) struct ExecutionResumeAdapter {
    service: Arc<TurnDecisionService>,
    context: Arc<TurnDecisionContext>,
    proof: crate::runtime::turn_source::OwnedTurnSourceProof,
    envelope: crate::runtime::envelope::RuntimeEnvelope,
}

impl ExecutionResumeAdapter {
    pub(super) fn new(
        service: Arc<TurnDecisionService>,
        context: Arc<TurnDecisionContext>,
        proof: &dyn TurnSourceProof,
        envelope: crate::runtime::envelope::RuntimeEnvelope,
    ) -> Self {
        Self {
            service,
            context,
            proof: crate::runtime::turn_source::OwnedTurnSourceProof::capture(proof),
            envelope,
        }
    }
}

#[async_trait::async_trait]
impl crate::tools::capability::execution_binding::DecisionResumeAdapter for ExecutionResumeAdapter {
    fn verify(
        &self,
        command: &crate::tools::ToolExecutionCommand,
        execution_context: &crate::tools::ToolExecutionContext,
    ) -> anyhow::Result<crate::security::policy::ApprovalGrant> {
        use std::sync::atomic::Ordering;
        let resume = self.context.resume.as_ref().context("resume admission missing")?;
        let (prepared, dialect) = resume.command()?;
        anyhow::ensure!(
            *command == *prepared
                && execution_context.envelope == self.envelope
                && execution_context.schema_dialect == dialect
                && !self.context.execution_closed.load(Ordering::Acquire)
                && self
                    .context
                    .execution_trace
                    .get()
                    .is_some_and(|trace| !trace.is_sealed()),
            "resume call or execution scope changed"
        );
        let selected = self.service.agent_execution_specs(&self.proof, &self.context)?;
        anyhow::ensure!(
            matches!(self.context.directive, TurnExecutionDirective::Execute { .. })
                && selected.specs.len() == 1
                && selected.specs.first().is_some_and(|spec| spec.name == command.name),
            "resume call was not selected"
        );
        let [grant] = resume.prepared.approval.grants() else {
            anyhow::bail!("resume requires one exact grant");
        };
        // This wrapper is only injection metadata. The downstream verifier
        // still requires the independently successful durable claim scope.
        Ok(crate::security::policy::ApprovalGrant::from_verified_v2(
            command.name.clone(),
            "PRXOperator",
            grant.clone(),
        ))
    }

    async fn claim(
        &self,
        command: &crate::tools::ToolExecutionCommand,
        execution_context: &crate::tools::ToolExecutionContext,
    ) -> anyhow::Result<Box<dyn crate::security::decision_approval::DecisionGrantAuthority>> {
        self.verify(command, execution_context)?;
        let claimed = self.claim_decision_resume(command).await?;
        Ok(Box::new(claimed))
    }
}

impl crate::security::decision_approval::DecisionGrantAuthority for ClaimedDecisionResume {
    fn verify(
        &self,
        grant: &ApprovalGrantV2,
        tool: &str,
        operation: &str,
        risk: RiskLevel,
        caller_principal: Option<&str>,
    ) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        let verify = || -> anyhow::Result<()> {
            anyhow::ensure!(
                *grant == self.grant
                    && tool == self.command.name
                    && operation == self.operation
                    && caller_principal == Some(self.grant.subject.principal_id.as_str())
                    && self.context.service_id == self.service.service_id
                    && self.execution.decision_id == self.context.decision_id().to_string()
                    && !self.context.execution_closed.load(Ordering::Acquire)
                    && self
                        .context
                        .execution_trace
                        .get()
                        .is_some_and(|trace| !trace.is_sealed()),
                "decision grant differs from active claimed call"
            );
            self.service.check_live_parity()?;
            let binding = self
                .grant
                .decision_binding
                .as_ref()
                .context("claimed binding missing")?;
            anyhow::ensure!(
                binding.catalog_generation == self.service.catalog.generation().0,
                "claimed decision catalog changed"
            );
            grant.verify_for_decision(
                WitnessKeyring::global()?,
                &self.grant.subject,
                binding,
                operation,
                risk,
                Utc::now(),
            )?;
            Ok(())
        };
        verify().map_err(|error| error.to_string())
    }
}

impl ExecutionResumeAdapter {
    /// Final claim for the prepared call, never for model-generated replacement
    /// arguments. The caller must place this after admission/idempotency checks
    /// and immediately before the scoped backend dispatch. A later failure burns
    /// the grant; it does not restore approval for an uncertain execution.
    async fn claim_decision_resume(
        &self,
        command: &crate::tools::ToolExecutionCommand,
    ) -> anyhow::Result<ClaimedDecisionResume> {
        let service = &self.service;
        let proof: &dyn TurnSourceProof = &self.proof;
        let context = &self.context;
        use std::sync::atomic::Ordering;
        let resume = context.resume.as_ref().context("resume admission missing")?;
        let ensure_active = || -> anyhow::Result<()> {
            anyhow::ensure!(
                !context.execution_closed.load(Ordering::Acquire)
                    && context.execution_trace.get().is_some_and(|trace| !trace.is_sealed()),
                "resume execution is closed"
            );
            let selected = service.agent_execution_specs(proof, context)?;
            anyhow::ensure!(
                matches!(context.directive, TurnExecutionDirective::Execute { .. })
                    && selected.specs.len() == 1
                    && selected.specs.first().is_some_and(|spec| spec.name == command.name)
                    && *command == resume.prepared.command,
                "resume dispatch differs from the prepared selected call"
            );
            Ok(())
        };
        ensure_active()?;
        // Reload the original pending/review/issued records and resolve the
        // operation again. This also checks that no resolution already exists.
        let prepared = service
            .prepare_decision_resume(
                proof.owner_id(),
                proof.session_key(),
                DecisionId::parse(&resume.prepared.approval.pending().record().binding.decision_id)?,
                command.clone(),
            )
            .await?;
        anyhow::ensure!(
            prepared.command == *command
                && prepared.approval.grants() == resume.prepared.approval.grants()
                && serde_json::to_value(prepared.approval.pending().record())?
                    == serde_json::to_value(resume.prepared.approval.pending().record())?,
            "resume approval changed after admission"
        );
        // Admission was authenticated earlier, but do not rely on an in-memory
        // source or intent if its durable record is now missing or different.
        for expected in [&resume.source, &resume.receipt] {
            let actual = service
                .memory
                .find_owned_message_event(proof.workspace_id(), proof.owner_id(), &expected.event_id)
                .await?
                .context("resume provenance is no longer durable")?;
            anyhow::ensure!(
                serde_json::to_value(&actual)? == serde_json::to_value(expected)?,
                "resume durable provenance changed"
            );
        }
        service
            .settlement_ref(Arc::clone(context))?
            .validate_durable_chain()
            .await?;
        ensure_active()?;
        let pending = prepared.approval.pending();
        let [grant] = prepared.approval.grants() else {
            anyhow::bail!("resume dispatch requires one exact grant");
        };
        let [operation] = pending.record().op_ids.as_slice() else {
            anyhow::bail!("resume dispatch requires one exact operation");
        };
        let execution = super::approval::ResumeClaimScope {
            decision_id: context.decision_id().to_string(),
            source_event_id: resume.source.event_id.clone(),
            run_id: proof.run_id().to_owned(),
        };
        anyhow::ensure!(
            execution.decision_id != pending.record().binding.decision_id,
            "resume must consume under a fresh decision"
        );
        let receipt = service
            .approval_ledger()
            .claim_decision_grant(
                scope_from_source(pending.source()),
                grant,
                &super::approval::DecisionGrantScope {
                    subject: &pending.record().subject,
                    binding: &pending.record().binding,
                    op_id: operation,
                    risk: pending.record().approval_risk()?,
                },
                WitnessKeyring::global()?,
                execution.clone(),
            )
            .await?;
        resume
            .claimed
            .set(receipt)
            .map_err(|_| anyhow::anyhow!("resume claim already attached"))?;
        // The shared atomic resolution wins against both concurrent resume and
        // revoke. Never roll it back when parity/expiry/closure fails here.
        ensure_active()?;
        Ok(ClaimedDecisionResume {
            command: prepared.command,
            grant: grant.clone(),
            execution,
            operation: operation.clone(),
            service: Arc::clone(service),
            context: Arc::clone(context),
        })
    }
}

impl TurnDecisionService {
    pub(super) async fn admit_decision_resume(
        &self,
        proof: &dyn TurnSourceProof,
    ) -> anyhow::Result<AdmittedDecisionResume> {
        self.resolve_agent_model(proof)?;
        self.check_live_parity()?;
        // Parse the source-owned routing input rather than a second command
        // argument that could silently select a different approved operation.
        let request: DecisionResumeRequest =
            serde_json::from_str(proof.routing_input()).context("resume requires an explicit source-bound request")?;
        let (decision_id, command) = match request {
            DecisionResumeRequest::Stored(request) => {
                let id = DecisionId::parse(&request.decision_id)?;
                let pending = self
                    .load_pending_decision_approval(proof.owner_id(), proof.session_key(), id)
                    .await?;
                (id, self.open_pending_call(&pending)?)
            }
            DecisionResumeRequest::Explicit(request) => (
                DecisionId::parse(&request.decision_id)?,
                crate::tools::ToolExecutionCommand::new(request.tool, request.arguments),
            ),
        };
        let prepared = self
            .prepare_decision_resume(proof.owner_id(), proof.session_key(), decision_id, command)
            .await?;
        let original = prepared.approval.pending();
        let source_id = proof.source_event_id().context("resume source missing")?;
        anyhow::ensure!(
            source_id != original.record().source_event_id
                && proof.run_id() != original.record().run_id
                && proof.workspace_id() == original.record().subject.workspace_id
                && original.source().channel.as_deref() == Some(proof.policy_channel())
                && original.source().sender.as_deref() == Some(proof.policy_sender()),
            "resume requires a fresh request from the original actor scope"
        );
        let source = self
            .memory
            .find_owned_message_event(proof.workspace_id(), proof.owner_id(), source_id)
            .await?
            .context("resume input is not durable")?;
        anyhow::ensure!(
            source.id > 0
                && source.event_type == "message.created"
                && source.role == "user"
                && source.session_key.as_deref() == Some(proof.session_key())
                && source.run_id.as_deref() == Some(proof.run_id())
                && crate::runtime::turn_source::subject_task_id(source.subject.as_ref()) == proof.task_id()
                && source.channel.as_deref() == Some(proof.policy_channel())
                && source.sender.as_deref() == Some(proof.policy_sender())
                && source.config_generation_id == Some(proof.config_generation_id())
                && source.config_source_revision.as_deref() == Some(proof.config_source_revision())
                && crate::runtime::turn_source::recorded_input_matches(proof.surface(), &source, proof.routing_input()),
            "resume input differs from admitted source"
        );
        let durable = self
            .approval_ledger()
            .record_decision_resume(
                crate::security::approval_ledger::ResumeRequestScope {
                    decision_id,
                    workspace: proof.workspace_id(),
                    owner: proof.owner_id(),
                    source_id,
                    run_id: proof.run_id(),
                },
                &source,
                &original.record().binding,
                prepared.approval.grants(),
            )
            .await?;
        let now = Utc::now();
        anyhow::ensure!(
            original.record().expires_at > now
                && prepared.approval.grants().iter().all(|grant| grant.is_time_valid(now)),
            "resume approval expired during admission"
        );
        Ok(AdmittedDecisionResume {
            prepared,
            source,
            receipt: durable,
            claimed: std::sync::OnceLock::new(),
        })
    }
}

pub(super) struct ResumeGateEvidence {
    /// Authenticated original source content, still untrusted decision data.
    pub(super) original_request: Arc<str>,
    pub(super) candidate_reference: crate::causal_tree::branch::CapabilityRef,
    pub(super) calls: std::collections::BTreeMap<crate::causal_tree::branch::CandidateId, [u8; 32]>,
    pub(super) approvals:
        std::collections::BTreeMap<[u8; 32], std::collections::BTreeSet<crate::causal_tree::branch::CapabilityRef>>,
    pub(super) grants: Vec<Arc<str>>,
    pub(super) causation: Arc<str>,
}

impl AdmittedDecisionResume {
    pub(super) fn original_run(&self) -> &str {
        &self.prepared.approval.pending().record().run_id
    }

    pub(super) fn claimed_event(&self) -> Option<&MessageEvent> {
        self.claimed.get()
    }
    pub(super) fn command(
        &self,
    ) -> anyhow::Result<(
        &crate::tools::ToolExecutionCommand,
        crate::tools::schema::ToolSchemaDialect,
    )> {
        Ok((
            &self.prepared.command,
            self.prepared
                .approval
                .pending()
                .record()
                .schema_dialect
                .context("resume dialect missing")?,
        ))
    }

    pub(super) fn gate_evidence(
        &self,
        service: &TurnDecisionService,
        proof: &dyn TurnSourceProof,
    ) -> anyhow::Result<ResumeGateEvidence> {
        use crate::causal_tree::branch::{CandidateId, CapabilityRef, CausalActionKind, Ns8};
        use std::collections::{BTreeMap, BTreeSet};
        let pending = self.prepared.approval.pending().record();
        anyhow::ensure!(
            self.source.event_id == proof.source_event_id().context("resume source missing")?
                && self.source.run_id.as_deref() == Some(proof.run_id())
                && crate::runtime::turn_source::subject_task_id(self.source.subject.as_ref()) == proof.task_id()
                && pending.subject.owner_id == proof.owner_id()
                && pending.subject.workspace_id == proof.workspace_id()
                && pending.subject.session_key.as_deref() == Some(proof.session_key())
                && pending.policy_version == service.policy_version.as_ref()
                && pending.binding.catalog_generation == service.catalog.generation().0
                && pending.expires_at > Utc::now(),
            "resume gate scope changed or expired"
        );
        service.resolve_agent_model(proof)?;
        service.check_live_parity()?;
        let entry = service
            .catalog
            .entry_for_public_name(&self.prepared.command.name)
            .context("resume capability missing")?;
        anyhow::ensure!(
            pending.capabilities.len() == 1
                && pending
                    .capabilities
                    .first()
                    .is_some_and(|(id, schema, generation)| id == entry.id.as_str()
                        && *schema == entry.fingerprint.0
                        && *generation == entry.generation.0),
            "resume gate capability changed"
        );
        let keyring = WitnessKeyring::global()?;
        anyhow::ensure!(
            self.prepared.approval.grants().len() == pending.op_ids.len() && !pending.op_ids.is_empty(),
            "resume gate grants incomplete"
        );
        for (grant, op) in self.prepared.approval.grants().iter().zip(&pending.op_ids) {
            grant.verify_for_decision(
                keyring,
                &pending.subject,
                &pending.binding,
                op,
                pending.approval_risk()?,
                Utc::now(),
            )?;
        }
        let digest = pending.binding.call_digest;
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        Ok(ResumeGateEvidence {
            original_request: Arc::from(self.prepared.approval.pending().source().content.as_str()),
            candidate_reference: reference.clone(),
            calls: BTreeMap::from([
                (CandidateId::new(CausalActionKind::UseTool, entry.family, None), digest),
                (
                    CandidateId::new(
                        CausalActionKind::UseTool,
                        entry.family,
                        Some(Ns8::of(&service.state_key, &entry.namespace)),
                    ),
                    digest,
                ),
            ]),
            approvals: BTreeMap::from([(digest, BTreeSet::from([reference]))]),
            grants: self
                .prepared
                .approval
                .grants()
                .iter()
                .map(|grant| Arc::from(grant.grant_id.as_str()))
                .collect(),
            causation: Arc::from(self.receipt.event_id.as_str()),
        })
    }
}

impl TurnDecisionService {
    pub(super) fn approval_ledger(&self) -> ApprovalLedger<'_> {
        ApprovalLedger {
            memory: &self.memory,
            state_key: &self.state_key,
            store: &self.store,
            workspace: self.generation.effective.workspace_dir.to_string_lossy().into_owned(),
            catalog_generation: self.catalog.generation().0,
            policy_version: &self.policy_version,
            enabled: self.recordable && self.durable_key && self.generation.effective.causal_tree.enabled,
            port: self,
        }
    }
}

#[async_trait::async_trait]
impl ApprovalRuntime for TurnDecisionService {
    fn check_live_parity(&self) -> anyhow::Result<()> {
        Ok(Self::check_live_parity(self)?)
    }
    async fn verify_pending_console_file_operation(
        &self,
        pending: &VerifiedPendingDecisionApproval,
    ) -> anyhow::Result<()> {
        Self::verify_pending_console_file_operation(self, pending).await
    }
    fn open_pending_call(
        &self,
        pending: &VerifiedPendingDecisionApproval,
    ) -> anyhow::Result<crate::tools::ToolExecutionCommand> {
        Self::open_pending_call(self, pending)
    }
    fn verify_h2_review_requirement(
        &self,
        pending: &VerifiedPendingDecisionApproval,
        approved: bool,
        stamp: Option<&crate::runtime::h2_entry::LocalOwnerReviewStamp>,
    ) -> anyhow::Result<()> {
        Self::verify_h2_review_requirement(self, pending, approved, stamp)
    }

    fn retire_pending(&self, record: &PendingRecord) {
        self.recursive_allowances.retire_pending(
            &record.subject.workspace_id,
            &record.subject.owner_id,
            record.subject.session_key.as_deref().unwrap_or_default(),
            &record.run_id,
        );
    }
}
