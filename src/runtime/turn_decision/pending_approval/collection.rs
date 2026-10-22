//! Single-use collection provenance. This module never constructs dispatch authority.
use super::{PendingRecord, TurnDecisionContext, TurnDecisionService};
use crate::causal_tree::branch::CapabilityRef;
use crate::runtime::turn_source::{OwnedTurnSourceProof, TurnSourceProof};
use crate::tools::{ToolExecutionCommand, ToolSpec};
use anyhow::Context;
use std::sync::{Arc, atomic::Ordering};
use tokio_util::sync::CancellationToken;

/// The model proposed a call that needs no approval of its own (a read or a
/// non-grant-bound `skills_manage` action) while the turn was collecting a
/// grant-bound one. Nothing was recorded or executed; the message is written
/// for the owner and the turn ends as an ordinary failed terminal with it.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct CollectionDeclined(pub(crate) String);

pub(crate) struct ApprovalCollectionLease {
    service: Arc<TurnDecisionService>,
    context: Arc<TurnDecisionContext>,
    source: OwnedTurnSourceProof,
    capability: CapabilityRef,
    descriptor: crate::tools::execution::ToolDescriptor,
}

impl TurnDecisionService {
    pub(crate) fn begin_approval_collection(
        self: &Arc<Self>,
        source: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
    ) -> anyhow::Result<ApprovalCollectionLease> {
        let capability = self.validate_approval_collection(source, context)?.capability().clone();
        let entries = self.catalog.resolve(std::slice::from_ref(&capability))?;
        let entry = entries.first().context("collection capability missing")?;
        anyhow::ensure!(
            crate::causal_tree::candidate_generator::collectable_approval_entry(entry),
            "unsupported collection tool"
        );
        let descriptor = self
            .execution
            .current_descriptor(&entry.public_name)
            .context("collection descriptor missing")?;
        self.check_live_parity()?;
        context
            .approval_collection_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("approval collection already consumed"))?;
        Ok(ApprovalCollectionLease {
            service: Arc::clone(self),
            context: Arc::clone(context),
            source: OwnedTurnSourceProof::capture(source),
            capability,
            descriptor,
        })
    }
}

impl ApprovalCollectionLease {
    pub(crate) fn parameter_capture(&self) -> crate::providers::parameter_capture::Capture {
        self.context.capture_main_parameters(true)
    }

    pub(crate) fn proposal_spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.descriptor.public_name.clone(),
            description: self.descriptor.description.clone(),
            parameters: self.descriptor.parameters.clone(),
        }
    }

    /// Consume a single complete provider proposal, preserving pure effect
    /// arguments. Runtime scope is bound separately and injected only on resume.
    pub(crate) async fn record(
        self,
        call: crate::providers::ToolCall,
        dialect: crate::tools::schema::ToolSchemaDialect,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<()> {
        use crate::acl::approval_grant::{DecisionApprovalBinding, Subject};
        use crate::decision::KeyDomain;
        use crate::tools::capability::execution_binding::{CallDigestSource, decision_call_digest};
        anyhow::ensure!(!cancellation.is_cancelled(), "collection cancelled");
        self.service.validate_approval_collection(&self.source, &self.context)?;
        anyhow::ensure!(call.name == self.descriptor.public_name, "collection tool changed");
        let arguments: serde_json::Value =
            serde_json::from_str(&call.arguments).context("invalid collection arguments")?;
        let arguments = crate::tools::execution::normalize_collection_arguments(&arguments, &self.descriptor, dialect)
            .map_err(anyhow::Error::msg)?;
        let entry = self
            .service
            .catalog
            .entry_for_public_name(&call.name)
            .context("collection capability missing")?;
        let (operation, risk) = if entry.family == crate::causal_tree::branch::CapabilityTarget::Mcp {
            // An MCP tool has one fixed approval operation. Resume derives the
            // same operation and risk from the pinned entry. No owner-isolated
            // (or Console file-scoped) source reaches here: gate 1
            // `owner_isolation` blocks MCP candidates for such sources before
            // collection is ever issued, so no filesystem scope is needed.
            let [operation] = entry.op_ids.as_slice() else {
                anyhow::bail!("collection operation is not fixed");
            };
            (
                operation.as_str().to_owned(),
                crate::acl::approval_grant::RiskLevel::High,
            )
        } else if entry.collects_exact_call() {
            // The operation is the one the tool itself checks, bound to the
            // exact target in the arguments. An action with no grant-bound
            // operation needs no collected approval: decline before anything
            // is persisted so the owner gets an explanation, not a failure of
            // the collection itself.
            match crate::security::approval_operation::grant_op_for_call(&call.name, &arguments) {
                Some(crate::security::approval_operation::GrantOp::ResourceExact(operation, risk)) => (operation, risk),
                _ => {
                    let action = arguments
                        .get("action")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("the requested action");
                    return Err(CollectionDeclined(format!(
                        "skills_manage {action} needs no approval, so nothing was recorded or run. \
                         Ask for it again without naming publish, rollback or executable import."
                    ))
                    .into());
                }
            }
        } else {
            let path = arguments
                .get("path")
                .and_then(serde_json::Value::as_str)
                .context("collection file path missing")?;
            self.service
                .verify_console_filesystem_context(&self.source, &self.context)?;
            anyhow::ensure!(
                !self.source.owner_isolated() || self.context.owner_filesystem.is_some(),
                "isolated file scope unavailable"
            );
            self.service
                .scoped_file_operation(self.context.owner_filesystem.as_ref(), &call.name, path)
                .await?
        };
        anyhow::ensure!(!operation.is_empty(), "collection exact operation missing");
        self.service.validate_approval_collection(&self.source, &self.context)?;
        self.service
            .settlement_ref(Arc::clone(&self.context))?
            .validate_durable_chain()
            .await?;
        let source_id = self
            .source
            .source_event_id()
            .context("collection durable source missing")?;
        let source = self
            .service
            .memory
            .find_owned_message_event(self.source.workspace_id(), self.source.owner_id(), source_id)
            .await?
            .context("collection source is not durable")?;
        anyhow::ensure!(
            source.role == "user"
                && source.event_type == "message.created"
                && source.session_key.as_deref() == Some(self.source.session_key())
                && source.run_id.as_deref() == Some(self.source.run_id())
                && source.config_generation_id == Some(self.source.config_generation_id())
                && source.config_source_revision.as_deref() == Some(self.source.config_source_revision())
                && crate::runtime::turn_source::subject_task_id(source.subject.as_ref()) == self.source.task_id()
                && self
                    .context
                    .source_actor_matches(source.channel.as_deref(), source.sender.as_deref())
                && crate::runtime::turn_source::recorded_input_matches(
                    self.source.surface(),
                    &source,
                    self.source.routing_input()
                ),
            "collection source differs from admitted input"
        );
        let digest = decision_call_digest(
            &self.context.decision_id().to_string(),
            &self.capability,
            CallDigestSource {
                workspace: self.source.workspace_id(),
                owner: self.source.owner_id(),
                session: self.source.session_key(),
                run: Some(self.source.run_id()),
                event: Some(source_id),
                channel: source.channel.as_deref(),
                sender: source.sender.as_deref(),
                generation: source.config_generation_id,
                revision: source.config_source_revision.as_deref(),
            },
            &arguments,
            &operation,
            risk,
        )?;
        let decision_scope = crate::causal_tree::store::DecisionScope::Owner {
            workspace_id: self.service.store.workspace_id().context("approval store missing")?,
            owner_id: Arc::from(self.source.owner_id()),
        };
        let mut decision_digests = Vec::with_capacity(self.context.hop_ids().len());
        for id in self.context.hop_ids() {
            let row = self
                .service
                .store
                .reader()
                .replay_record(&decision_scope, id)?
                .context("approval decision is not durable")?;
            decision_digests.push((id.to_string(), row.digest));
        }
        let channel = source.channel.as_deref().context("approval channel missing")?;
        let sender = source.sender.as_deref().context("approval sender missing")?;
        let principal = crate::memory::principal::OwnerPrincipal::new(
            self.source.workspace_id(),
            channel,
            sender,
            self.source.session_key(),
            vec![crate::memory::principal::Role::Anonymous],
        );
        anyhow::ensure!(
            entry.op_ids.is_empty() || entry.op_ids.iter().any(|id| id.as_str() == operation),
            "collection operation differs from pinned capability"
        );
        let mut record = PendingRecord {
            subject: Subject {
                agent_id: format!("prx:agent:{channel}"),
                principal_id: principal.principal_id,
                owner_id: self.source.owner_id().to_owned(),
                workspace_id: self.source.workspace_id().to_owned(),
                session_key: Some(self.source.session_key().to_owned()),
            },
            binding: DecisionApprovalBinding {
                decision_id: self.context.decision_id().to_string(),
                catalog_generation: self.capability.generation.0,
                call_digest: digest,
            },
            source_event_id: source.event_id.clone(),
            source_mac: self
                .service
                .state_key
                .hmac(KeyDomain::ApprovalState, &[b"source", &serde_json::to_vec(&source)?]),
            run_id: self.source.run_id().to_owned(),
            decision_digests,
            policy_version: self.service.policy_version.to_string(),
            capabilities: vec![(
                self.capability.id.as_str().to_owned(),
                self.capability.fingerprint.0,
                self.capability.generation.0,
            )],
            op_ids: vec![operation],
            effect: entry.effect.as_str().to_owned(),
            risk: Some(risk),
            schema_dialect: Some(dialect),
            sealed_call: None,
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
        };
        record.sealed_call = Some(
            self.service
                .seal_pending_call(&record, &ToolExecutionCommand::new(call.name, arguments))?,
        );
        self.service.validate_approval_collection(&self.source, &self.context)?;
        anyhow::ensure!(!cancellation.is_cancelled(), "collection cancelled before persistence");
        let owner = crate::runtime::turn_cleanup::current_tracker().context("collection cleanup owner missing")?;
        let ticket = owner.register(cancellation.clone())?;
        let service = self.service;
        let context = self.context;
        tokio::spawn(async move {
            let result = service
                .persist_collection_pending_record(context, record, &source)
                .await;
            ticket.finish(result.is_ok());
            result
        })
        .await
        .context("collection persistence owner task failed")?
    }
}
