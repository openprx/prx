//! Accounted no-tool assessment calls for the existing turn owner.
//! Borrows request provenance, provider tracing and physical accounting;
//! it creates no execution, approval, publication or budget authority.
use crate::agent::turn_contracts::{ToolLoopCancelled, ToolLoopRuntimeAdapter, ToolLoopTrace};
use crate::hooks::{HookEvent, HookManager};
use crate::llm::route_decision::ProviderUsageAccumulator;
use crate::memory::{MemoryFabric, MessageEventScope};
use crate::observability::{Observer, ObserverEvent};
use crate::providers::{ChatMessage, ChatRequest, Provider};
use sha2::{Digest, Sha256};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

pub(super) struct AssessmentCall<'a> {
    pub provider: &'a dyn Provider,
    pub provider_name: &'a str,
    pub model: &'a str,
    pub observer: &'a dyn Observer,
    pub hooks: &'a HookManager,
    pub runtime_adapter: &'a ToolLoopRuntimeAdapter,
    pub execution_trace: Option<&'a crate::tools::capability::execution_trace::ExecutionTrace>,
    pub parameter_capture: &'a crate::providers::parameter_capture::Capture,
    pub cancellation_token: Option<&'a CancellationToken>,
    pub last_turn_trace: &'a mut ToolLoopTrace,
    pub usage_accumulator: &'a mut ProviderUsageAccumulator,
    pub any_turn_had_fallback: &'a mut bool,
}

/// Persist rejection metadata before returning or repairing; never log model text.
pub(super) async fn record_rejected_review(
    fabric: Option<&MemoryFabric>,
    scope: Option<&MessageEventScope>,
    response: &crate::providers::ChatResponse,
    diagnostics: Option<serde_json::Value>,
    repair_attempted: bool,
) -> anyhow::Result<()> {
    if let (Some(fabric), Some(scope)) = (fabric, scope) {
        fabric
            .record_runtime_event(
                scope.clone(),
                "turn.completion_review.rejected",
                "Completion review rejected before delivery".to_string(),
                Some(
                    serde_json::json!({
                        "response_sha256":hex::encode(Sha256::digest(response.text_or_empty().as_bytes())),
                        "response_bytes":response.text_or_empty().len(),
                        "tool_calls":response.tool_calls.len(),
                        "parsed":diagnostics.is_some(), "constraint_provenance":diagnostics,
                        "repair_attempted":repair_attempted,
                    })
                    .to_string(),
                ),
            )
            .await?;
    }
    Ok(())
}

impl AssessmentCall<'_> {
    pub async fn chat(
        self,
        assessment_messages: &[ChatMessage],
        purpose: &str,
    ) -> anyhow::Result<crate::providers::traits::ChatTrace> {
        let Self {
            provider,
            provider_name,
            model,
            observer,
            hooks,
            runtime_adapter,
            execution_trace,
            parameter_capture,
            cancellation_token,
            last_turn_trace,
            usage_accumulator,
            any_turn_had_fallback,
        } = self;
        let observation = execution_trace.map(|trace| trace.begin_provider()).transpose()?;
        if let Some(retained) = &runtime_adapter.provider_trace {
            retained.begin();
        }
        let assessment_started = Instant::now();
        observer.record_event(&ObserverEvent::LlmRequest {
            provider: provider_name.to_string(),
            model: model.to_string(),
            messages_count: assessment_messages.len(),
        });
        hooks
            .emit(
                HookEvent::LlmRequest,
                serde_json::json!({
                    "provider":provider_name, "model":model, "purpose":purpose,
                    "messages_count":assessment_messages.len(), "tool_count":0,
                }),
            )
            .await;
        let assessment_future = parameter_capture.scope(crate::runtime::experiment_stage::chat_traced(
            provider,
            provider_name,
            runtime_adapter
                .tool_execution_context
                .envelope
                .run_id
                .as_deref()
                .unwrap_or(""),
            ChatRequest {
                messages: &assessment_messages,
                tools: None,
            },
            model,
            0.0,
        ));
        let assessment_result = if let Some(token) = cancellation_token {
            tokio::select! {
                () = token.cancelled() => {
                    if let Some(observation) = observation { observation.cancelled(); }
                    if let Some(retained) = &runtime_adapter.provider_trace { retained.failed(); }
                    return Err(ToolLoopCancelled.into());
                }
                result = assessment_future => result,
            }
        } else {
            assessment_future.await
        };
        if let Some(observation) = observation {
            observation.finish(assessment_result.as_ref().ok().map(|trace| trace.attempts.as_slice()));
        }
        crate::agent::idle::beat(crate::agent::idle::ProgressKind::ProviderResponse);
        let duration = assessment_started.elapsed();
        let error_message = assessment_result.as_ref().err().map(ToString::to_string);
        observer.record_event(&ObserverEvent::LlmResponse {
            provider: provider_name.to_string(),
            model: model.to_string(),
            duration,
            success: assessment_result.is_ok(),
            error_message: error_message.clone(),
        });
        hooks.emit(HookEvent::LlmResponse, serde_json::json!({
    "provider":provider_name, "model":model, "purpose":purpose,
    "duration_ms":duration.as_millis(), "success":assessment_result.is_ok(), "error_message":error_message,
})).await;
        if assessment_result.is_err()
            && let Some(retained) = &runtime_adapter.provider_trace
        {
            retained.failed();
        }
        let trace = assessment_result?;
        usage_accumulator.record(trace.tokens_used.clone());
        *any_turn_had_fallback |=
            trace.attempts.len() > 1 || trace.final_provider != provider_name || trace.final_model != model;
        last_turn_trace
            .prepared_parameters
            .extend(parameter_capture.snapshot_from(last_turn_trace.prepared_parameters.len()));
        if let Some(retained) = &runtime_adapter.provider_trace {
            retained.completed(
                last_turn_trace,
                usage_accumulator.finish(),
                *any_turn_had_fallback,
                trace.tokens_used.is_reported()
                    && trace.tokens_used.has_any_tokens()
                    && !trace
                        .attempts
                        .iter()
                        .any(|attempt| attempt.status == crate::llm::route_decision::AttemptStatus::Failed),
            );
        }

        Ok(trace)
    }
}

/// Persist the existing turn owner's assessment before it can permit dispatch.
pub(super) async fn record_outcome(
    fabric: Option<&MemoryFabric>,
    scope: Option<&MessageEventScope>,
    assessment: &super::completion_review::ReplyAssessment,
    trace: &crate::providers::traits::ChatTrace,
    phase: &str,
) -> anyhow::Result<()> {
    use super::completion_review::ReplyAssessment;
    let observed = matches!(assessment, ReplyAssessment::Completion(_));
    if let (Some(fabric), Some(scope)) = (fabric, scope) {
        fabric
            .record_runtime_event(
                scope.clone(),
                if observed {
                    "turn.completion_review.assessed"
                } else {
                    "turn.execution_requirement.assessed"
                },
                "Chat reply assessed".to_string(),
                Some(
                    serde_json::json!({
                        "assessment":format!("{assessment:?}"),
                        "requirement":match assessment {
                            ReplyAssessment::Execution(requirement) => Some(format!("{requirement:?}")),
                            ReplyAssessment::Completion(_) => None,
                        },
                        "provider":crate::providers::redact_endpoint_userinfo(&trace.final_provider),
                        "model":trace.final_model,
                        "tool_results_observed":observed,
                        "phase":phase,
                    })
                    .to_string(),
                ),
            )
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Memory, MemoryPrincipal, MemoryVisibility, SqliteMemory};
    use std::sync::Arc;

    #[tokio::test]
    async fn rejected_reviews_persist_source_scope_without_model_content() {
        let directory = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(directory.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), "review-fixture");
        let scope = MessageEventScope::new("chat", MemoryVisibility::Private)
            .with_channel("terminal")
            .with_owner_id("owner:review-fixture:terminal:local-user")
            .with_sender("local-user")
            .with_session_key("chat:review");
        let response = crate::providers::ChatResponse {
            text: Some("private-model-content".into()),
            tool_calls: vec![],
            reasoning_content: None,
        };
        for (diagnostics, repaired) in [(None, false), (Some(serde_json::json!({"constraint_count":1})), true)] {
            record_rejected_review(Some(&fabric), Some(&scope), &response, diagnostics, repaired)
                .await
                .unwrap();
        }
        let events = memory
            .list_message_events_since(
                &MemoryPrincipal {
                    workspace_id: "review-fixture".into(),
                    owner_id: Some("owner:review-fixture:terminal:local-user".into()),
                    require_owner_match: true,
                    agent_id: None,
                    persona_id: None,
                    session_key: Some("chat:review".into()),
                    legacy_session_key: None,
                    channel: Some("terminal".into()),
                    sender: Some("local-user".into()),
                },
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(events.len(), 2);
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event.event_type, "turn.completion_review.rejected");
            assert_eq!(event.session_key.as_deref(), Some("chat:review"));
            let raw = event.raw_payload_json.as_ref().unwrap();
            assert!(!raw.contains("private-model-content"));
            let payload: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                payload.get("repair_attempted").and_then(serde_json::Value::as_bool),
                Some(index == 1)
            );
            assert_eq!(
                payload.get("parsed").and_then(serde_json::Value::as_bool),
                Some(index == 1)
            );
            assert_eq!(
                payload.get("response_bytes").and_then(serde_json::Value::as_u64),
                Some(21)
            );
        }
    }
}
