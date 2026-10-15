use crate::agent::turn_contracts::{ApprovalDecision, ApprovalResolver, ScopeContext, ToolLoopRuntimeAdapter};
use crate::approval::{ApprovalManager, ApprovalRequest, ApprovalResponse};
use crate::memory::Memory;
use crate::memory::MemoryVisibility;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::security::SecurityPolicy;
use crate::security::policy::ApprovalGrant;
use crate::tools::Tool;
use crate::util::truncate_with_ellipsis;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Returns `true` when `name` belongs to the set of tools that **modify**
/// state (filesystem, shell, git, memory writes, background sessions, etc.)
/// and therefore must be intercepted when the chat session is in
/// [`ChatMode::Plan`].
///
/// Unknown tool names are conservatively treated as writes — better to
/// surface "[plan mode] would call X" than accidentally execute an opaque
/// MCP/composio tool whose side-effects we can't classify.
pub(super) fn is_write_tool(name: &str) -> bool {
    crate::causal_tree::capability_policy::plan_intercepts_tool(name)
}

/// Truncate a JSON value to a short preview string suitable for synthesized
/// "would call" messages in plan mode. Keeps the message readable even when
/// the original arguments are large (file contents, shell commands, …).
pub(super) fn preview_tool_arguments(args: &serde_json::Value) -> String {
    let raw = args.to_string();
    truncate_with_ellipsis(&raw, 160)
}

use crate::security::approval_operation::{GrantOp, grant_op_for_call};

/// Build a signed v2 ApprovalGrant for a freshly user-approved tool call,
/// binding it to the calling principal, the operation (exact or `{tool}:{verb}:*`
/// glob), and a short single-use validity window. Shell/cron commands stay on the
/// v1 command-hash path so persisted runner grants (cron/xin, executed in a later
/// process with the scheduler — not the chat principal — as caller) keep working.
pub(crate) fn runtime_approval_grant_for_call(
    tool_name: &str,
    args: &serde_json::Value,
    scope_ctx: Option<&ScopeContext<'_>>,
) -> Option<ApprovalGrant> {
    use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, OpIdMatch, Subject, WitnessKeyring};

    // The legacy callback has no canonical file target resolver. Preserve its
    // refusal for file_edit; only the exact decision-call producer can bind it.
    if tool_name == "file_edit" {
        return None;
    }

    let (operation, op_id_match, risk) = match grant_op_for_call(tool_name, args)? {
        GrantOp::Command(command) => {
            // Legacy v1 command-hash binding (cron/runner compatible).
            return Some(ApprovalGrant::for_command(
                tool_name,
                &command,
                "approval_manager",
                None,
            ));
        }
        GrantOp::ResourceExact(operation, risk) => (operation, OpIdMatch::Exact, risk),
        GrantOp::ResourceGlob(pattern, risk) => (pattern, OpIdMatch::GlobPattern(String::new()), risk),
    };

    // Without a trusted runtime scope we cannot bind a subject. Rather than fall
    // back to an unbound grant, deny by issuing nothing for the glob case (which
    // has no precise hash); for the exact case keep a v1 exact-hash grant so
    // legacy precise flows still work.
    let Some(scope) = scope_ctx else {
        return match op_id_match {
            OpIdMatch::Exact => Some(ApprovalGrant::for_resource_operation(
                tool_name,
                &operation,
                "approval_manager",
                None,
            )),
            OpIdMatch::GlobPattern(_) => None,
        };
    };

    let workspace_id = scope.policy.workspace_dir.to_string_lossy().to_string();
    let principal = crate::memory::principal::OwnerPrincipal::new(
        workspace_id.as_str(),
        scope.channel,
        scope.sender,
        scope.chat_id,
        vec![crate::memory::principal::Role::Anonymous],
    );

    let keyring = match WitnessKeyring::global() {
        Ok(keyring) => keyring,
        Err(error) => {
            tracing::error!("witness keyring unavailable; cannot issue v2 grant: {error}");
            // Exact ops can still use a precise v1 hash; glob ops cannot, so deny.
            return match op_id_match {
                OpIdMatch::Exact => Some(ApprovalGrant::for_resource_operation(
                    tool_name,
                    &operation,
                    "approval_manager",
                    None,
                )),
                OpIdMatch::GlobPattern(_) => None,
            };
        }
    };

    // Consume `principal` (not used afterwards) to avoid cloning its strings.
    let subject = Subject {
        agent_id: format!("prx:agent:{}", scope.channel),
        principal_id: principal.principal_id,
        owner_id: principal.owner_id,
        workspace_id,
        session_key: Some(scope.chat_id.to_string()),
    };

    // For a glob grant the stored capability.op_id IS the pattern; for an exact
    // grant it is the operation string.
    let match_mode = match &op_id_match {
        OpIdMatch::Exact => OpIdMatch::Exact,
        OpIdMatch::GlobPattern(_) => OpIdMatch::GlobPattern(operation.clone()),
    };

    let issue = (|| -> anyhow::Result<ApprovalGrant> {
        // Fully signed, short-window, single-use grant bound to `subject`. We
        // verify our own signature in-process before trusting it (defense in
        // depth); the gate re-verifies against the keyring regardless of the
        // in-process `v2_verified` hint.
        let grant = ApprovalGrantV2::issue_one_shot_match(
            keyring,
            subject,
            IssuerAuthority::HumanReview,
            operation.clone(),
            match_mode,
            risk,
        )?;
        crate::acl::approval_grant::verify_grant(keyring, &grant)?;
        Ok(ApprovalGrant::from_verified_v2(tool_name, "approval_manager", grant))
    })();

    match issue {
        Ok(grant) => Some(grant),
        Err(error) => {
            tracing::error!(tool = tool_name, "failed to issue v2 approval grant: {error}");
            // Exact ops can degrade to a precise v1 hash; glob ops cannot be
            // expressed in v1, so deny rather than wildcard-approve.
            match op_id_match {
                OpIdMatch::Exact => Some(ApprovalGrant::for_resource_operation(
                    tool_name,
                    &operation,
                    "approval_manager",
                    None,
                )),
                OpIdMatch::GlobPattern(_) => None,
            }
        }
    }
}

pub(super) struct AgentToolApprovalStrategy {
    pub(super) manager: Option<Arc<ApprovalManager>>,
    pub(super) resolver: Option<Arc<dyn ApprovalResolver>>,
    pub(super) channel: String,
    pub(super) policy: Arc<SecurityPolicy>,
    pub(super) cancellation: Option<CancellationToken>,
}

impl AgentToolApprovalStrategy {
    fn runtime_grant(&self, request: &crate::tools::ToolApprovalRequest) -> Option<serde_json::Value> {
        let envelope = &request.context.envelope;
        let scope = ScopeContext {
            policy: self.policy.as_ref(),
            sender: envelope.sender.as_deref().unwrap_or("unknown"),
            channel: envelope.channel.as_deref().unwrap_or("unknown"),
            chat_type: &request.context.chat_type,
            chat_id: &request.context.chat_id,
            session_key: Some(&envelope.session_key),
            owner_id: envelope.owner_id.as_deref(),
            owner_isolated: envelope.owner_isolated,
            topic_id: envelope.topic_id.as_deref(),
            task_id: envelope.task_id.as_deref(),
            source_message_event_id: envelope.source_message_event_id.as_deref(),
            config_generation_id: envelope.config_generation_id,
            config_source_revision: envelope.config_source_revision.as_deref(),
        };
        runtime_approval_grant_for_call(
            &request.descriptor.public_name,
            &request.command.arguments,
            Some(&scope),
        )
        .and_then(|grant| serde_json::to_value(grant).ok())
    }
}

#[async_trait::async_trait]
impl crate::tools::ApprovalStrategy for AgentToolApprovalStrategy {
    async fn resolve(&self, request: crate::tools::ToolApprovalRequest) -> crate::tools::ToolApprovalDecision {
        if request.decision_call.is_some() {
            // A generic tool approval must not erase the pinned decision/call
            // scope. The durable decision resume adapter owns this transition.
            return crate::tools::ToolApprovalDecision::Denied {
                reason: "decision-bound approval requires durable review and resume".to_owned(),
            };
        }
        let Some(manager) = self.manager.as_ref() else {
            return crate::tools::ToolApprovalDecision::Denied {
                reason: "approval required but no approval resolver is available".to_string(),
            };
        };
        let tool_name = request.descriptor.public_name.as_str();
        if manager.is_session_allowlisted(tool_name) {
            return crate::tools::ToolApprovalDecision::Approved {
                runtime_approval_granted: true,
                runtime_grant: self.runtime_grant(&request),
            };
        }

        let approval_request = ApprovalRequest {
            tool_name: tool_name.to_string(),
            arguments: request.command.arguments.clone(),
        };
        let (decision, grant) = if let Some(resolver) = self.resolver.as_ref() {
            let resolved = if let Some(token) = self.cancellation.as_ref() {
                tokio::select! {
                    biased;
                    () = token.cancelled() => None,
                    decision = resolver.resolve(&approval_request, &self.channel) => Some(decision),
                }
            } else {
                Some(resolver.resolve(&approval_request, &self.channel).await)
            };
            match resolved {
                Some(ApprovalDecision::Grant) => (ApprovalResponse::Yes, true),
                Some(ApprovalDecision::Deny) | None => (ApprovalResponse::No, false),
            }
        } else if self.channel == "cli" {
            (manager.prompt_cli(&approval_request), true)
        } else {
            (ApprovalResponse::No, false)
        };
        manager.record_decision(tool_name, &request.command.arguments, decision, &self.channel);
        if decision == ApprovalResponse::No {
            return crate::tools::ToolApprovalDecision::Denied {
                reason: "Denied by user".to_string(),
            };
        }
        crate::tools::ToolApprovalDecision::Approved {
            runtime_approval_granted: grant,
            runtime_grant: grant.then(|| self.runtime_grant(&request)).flatten(),
        }
    }
}

pub(super) fn agent_tool_execution_context(
    scope: Option<&ScopeContext<'_>>,
    channel_name: &str,
    unscoped_policy: Option<Arc<SecurityPolicy>>,
) -> (Arc<SecurityPolicy>, crate::tools::ToolExecutionContext) {
    // CRON-ORIGIN: a scheduled run whose job was not created by the local
    // operator acts as its creator, not as the `prx agent` CLI below.
    // Its calls are decided by the configured policy for that creator.
    if scope.is_none()
        && let Some(scheduled) = crate::tools::execution::scheduled_job_tool_scope()
    {
        return (scheduled.policy, scheduled.context);
    }
    let policy = scope.map_or_else(
        || unscoped_policy.unwrap_or_else(|| Arc::new(SecurityPolicy::default())),
        |scope| Arc::new(scope.policy.clone()),
    );
    let workspace_id = policy.workspace_dir.to_string_lossy().to_string();
    let session_key = scope.map_or_else(
        || format!("agent:{}", Uuid::now_v7()),
        |scope| scope.chat_id.to_string(),
    );
    let mut envelope = RuntimeEnvelope::chat_terminal(workspace_id, session_key, MemoryVisibility::Workspace)
        .with_sender(scope.map_or("agent", |scope| scope.sender))
        .with_channel(scope.map_or(channel_name, |scope| scope.channel));
    if let Some(scope) = scope {
        if let Some(owner_id) = scope.owner_id {
            envelope = envelope.with_owner_id(owner_id);
        }
        if let Some(topic_id) = scope.topic_id {
            envelope = envelope.with_topic_id(topic_id);
        }
        if let Some(task_id) = scope.task_id {
            envelope = envelope.with_task_id(task_id);
        }
        if let Some(event_id) = scope.source_message_event_id {
            envelope = envelope.with_source_message_event_id(event_id);
        }
    }
    let chat_type = scope.map_or("private", |scope| scope.chat_type);
    let chat_id = scope.map_or_else(|| "agent:local".to_string(), |scope| scope.chat_id.to_string());
    (
        Arc::clone(&policy),
        crate::tools::ToolExecutionContext::new(envelope, chat_type).with_chat_id(chat_id),
    )
}

/// Build the same execution adapter for the shared turn loop and its CLI
/// entrypoint. An explicit policy is used only for an unscoped local CLI turn;
/// scheduled creator authority and a scoped caller keep precedence.
pub(super) struct AgentToolRuntimeInputs<'a> {
    pub scope: Option<&'a ScopeContext<'a>>,
    pub channel_name: &'a str,
    pub tools_registry: Arc<Vec<Box<dyn Tool>>>,
    pub approval: Option<Arc<ApprovalManager>>,
    pub approval_resolver: Option<Arc<dyn ApprovalResolver>>,
    pub cancellation: Option<CancellationToken>,
    pub ledger: Option<Arc<dyn Memory>>,
    pub unscoped_policy: Option<Arc<SecurityPolicy>>,
}

pub(super) fn build_agent_tool_runtime_adapter(inputs: AgentToolRuntimeInputs<'_>) -> ToolLoopRuntimeAdapter {
    let (policy, tool_execution_context) =
        agent_tool_execution_context(inputs.scope, inputs.channel_name, inputs.unscoped_policy);
    let approval_strategy = AgentToolApprovalStrategy {
        manager: inputs.approval,
        resolver: inputs.approval_resolver,
        channel: inputs.channel_name.to_string(),
        policy: Arc::clone(&policy),
        cancellation: inputs.cancellation,
    };
    let mut service = crate::tools::ToolExecutionService::from_shared_boxed_registry(
        inputs.tools_registry,
        Arc::new(crate::tools::SecurityEffectPolicy::new(policy)),
        Arc::new(approval_strategy),
        Arc::new(crate::tools::AdapterOwnedPreparation),
        Arc::new(crate::tools::TracingToolExecutionAudit),
    );
    if let Some(ledger) = inputs.ledger {
        service = service.with_idempotency_memory(ledger);
    }
    ToolLoopRuntimeAdapter {
        provider_trace: None,
        events: None,
        stream_provider: false,
        tool_execution_service: Some(Arc::new(service)),
        tool_execution_context,
        allowed_tool_names: None,
    }
}

#[path = "turn_tests/execution_policy.rs"]
mod tests;
