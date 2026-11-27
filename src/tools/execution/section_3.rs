impl ToolExecutionService {
    /// Run the fixed descriptor -> effect -> policy -> approval -> preparation ->
    /// execute -> audit pipeline and return a typed terminal outcome.
    pub async fn execute(
        &self,
        command: ToolExecutionCommand,
        context: ToolExecutionContext,
        cancellation: Option<CancellationToken>,
    ) -> ToolExecutionOutcome {
        let pending = context.execution_trace.as_ref().map(|trace| {
            let binding = context.admitted_binding.as_ref();
            // Decision-free model turns still observe the identity of the
            // concrete registered tool. An unknown name remains unbound;
            // identifying a call never grants dispatch or marks it selected.
            let observed = self.current_descriptor(&command.name).and_then(|descriptor| {
                context
                    .turn_surface
                    .as_ref()
                    .and_then(|surface| surface.observed_capability_id(&command.name))
                    .map(str::to_owned)
                    .or_else(|| {
                        super::capability::catalog::descriptor_identity(&descriptor)
                            .ok()
                            .map(|id| id.as_str().to_owned())
                    })
            });
            trace.begin(
                &command.operation_id,
                binding.map(|binding| binding.capability_id()).or(observed.as_deref()),
                binding.is_some_and(|binding| binding.is_selected()),
            )
        });
        // `execute_inner` owns the whole descriptor, policy, approval,
        // preparation and backend pipeline. Its state machine is large enough
        // that embedding it in each caller's async frame can exhaust a default
        // tokio worker stack when a tool starts another agent loop. Keep the
        // canonical pipeline on the heap so every caller carries only a pointer
        // while it is pending.
        let mut outcome = Box::pin(self.execute_inner(command, context, cancellation)).await;
        if let Some(stage) = crate::runtime::experiment_stage::current() {
            if let Err(error) = stage.account_output(outcome.model_content.len()) {
                outcome.status = ToolExecutionStatus::Failed;
                outcome.error = Some(error.to_string());
                outcome.model_content = error.to_string();
            }
        }
        if let Some(pending) = pending {
            pending.finish_with_error(outcome.status, outcome.error.as_deref());
        }
        outcome
    }

    async fn execute_inner(
        &self,
        mut command: ToolExecutionCommand,
        context: ToolExecutionContext,
        cancellation: Option<CancellationToken>,
    ) -> ToolExecutionOutcome {
        let started = Instant::now();
        // Every production tool call passes through here, which makes this the
        // one place that has to register for the runtime work registry to be
        // complete. The call gets its own kill handle, derived from the turn
        // token so turn-level cancellation still reaches it, but cancellable on
        // its own so an operator can end one stuck call without taking the
        // whole turn down with it.
        let cancellation = Some(
            cancellation
                .as_ref()
                .map_or_else(CancellationToken::new, CancellationToken::child_token),
        );
        let work = crate::runtime::registry::register_tool_call(
            &command.name,
            context.envelope.task_id.as_deref(),
            cancellation.clone(),
        );
        // A tool call starting is progress for the turn's idle detector, and
        // this is the one place every production tool call passes through.
        crate::agent::idle::beat(crate::agent::idle::ProgressKind::ToolStart);
        let input_sha256 = format!("{:x}", Sha256::digest(command.arguments.to_string().as_bytes()));
        if let Err((status, error)) = admission::admit(self, &context, &command.name) {
            return self
                .finish(
                    &command,
                    &context,
                    input_sha256,
                    None,
                    None,
                    status,
                    format!("Error: {error}"),
                    None,
                    Some(error),
                    None,
                    started,
                )
                .with_refusal_cause(if status == ToolExecutionStatus::StaleCapability {
                    ToolCallRefusalReason::StaleCapability
                } else {
                    ToolCallRefusalReason::PolicyDenied
                });
        }
        let Some(resolved) = self.resolve(&command.name) else {
            let available = self
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.public_name)
                .collect::<Vec<_>>();
            let error = format!(
                "unknown tool '{}'; available tools: {}",
                command.name,
                available.join(", ")
            );
            return self
                .finish(
                    &command,
                    &context,
                    input_sha256,
                    None,
                    None,
                    ToolExecutionStatus::UnknownTool,
                    format!("Error: {error}"),
                    None,
                    Some(error),
                    None,
                    started,
                )
                .with_refusal_cause(ToolCallRefusalReason::UnknownTool);
        };
        if let Err((status, error)) = admission::recheck_resolved(&context, &command.name) {
            return self
                .finish(
                    &command,
                    &context,
                    input_sha256,
                    None,
                    None,
                    status,
                    format!("Error: {error}"),
                    None,
                    Some(error),
                    None,
                    started,
                )
                .with_refusal_cause(ToolCallRefusalReason::PolicyDenied);
        }
        let descriptor = resolved.descriptor;
        if let Some(stage) = crate::runtime::experiment_stage::current() {
            if let Err(error) = stage.claim_tool(&descriptor.backend_name) {
                let error = error.to_string();
                return self
                    .finish(
                        &command,
                        &context,
                        input_sha256,
                        None,
                        None,
                        ToolExecutionStatus::Failed,
                        error.clone(),
                        None,
                        Some(error),
                        None,
                        started,
                    )
                    .with_refusal_cause(ToolCallRefusalReason::PolicyDenied);
            }
        }

        if let Some(binding) = &context.admitted_binding {
            let verified = (context.execution_trace.as_ref().is_some_and(|trace| !trace.is_sealed()))
                .then_some(binding)
                .ok_or(super::capability::execution_binding::ToolExecutionError::StaleCapability)
                .and_then(|binding| binding.verify_admitted_descriptor(&context.envelope, &descriptor));
            if let Err(error) = verified {
                let error = error.to_string();
                return self
                    .finish(
                        &command,
                        &context,
                        input_sha256,
                        Some(descriptor),
                        None,
                        ToolExecutionStatus::StaleCapability,
                        format!("Error: {error}"),
                        None,
                        Some(error),
                        None,
                        started,
                    )
                    .with_refusal_cause(ToolCallRefusalReason::StaleCapability);
            }
        }
        // V02-R9 / OQ-21: an owner-isolated origin runs only tools that enforce
        // per-owner isolation. Keyed on the envelope rather than a channel name,
        // so every isolated origin inherits it, and decided before policy so no
        // autonomy level can widen it.
        let private_filesystem = context
            .owner_filesystem
            .as_ref()
            .filter(|scope| {
                (context.controlled_case().is_some()
                    || (context.envelope.owner_isolated
                        && context.envelope.source == crate::runtime::envelope::RuntimeSource::Console))
                    && (if context.controlled_case().is_some() {
                        scope.matches(&context.envelope.workspace_id, &context.envelope.resolved_owner_id())
                    } else {
                        context
                            .envelope
                            .owner_id
                            .as_deref()
                            .is_some_and(|owner| scope.matches(&context.envelope.workspace_id, owner))
                    })
                    && descriptor.adapter == ToolAdapterKind::Native
                    && descriptor.backend_name == descriptor.public_name
                    && matches!(
                        descriptor.public_name.as_str(),
                        "file_read" | "file_write" | "file_edit"
                    )
                    && resolved.backend.supports_owner_filesystem()
            })
            .cloned();
        if context.envelope.owner_isolated && !admits_owner_isolated_origin(&descriptor) && private_filesystem.is_none()
        {
            let error = owner_isolation_refusal(&descriptor.public_name);
            return self
                .finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    None,
                    ToolExecutionStatus::Denied,
                    format!("Error: {error}"),
                    None,
                    Some(error),
                    None,
                    started,
                )
                .with_refusal_cause(ToolCallRefusalReason::OwnerIsolation);
        }
        // Structural validation runs before approval on purpose: a call that
        // cannot legally execute must never reach an approval prompt, a
        // reservation, or a backend. This is the one production gate for the
        // tool contract, and every entry point funnels through here — including
        // the gateway MCP server, which ran outside it until 0.8.126. The
        // contract enforced is the one the caller was shown: see
        // `ToolExecutionContext::schema_dialect`.
        // Absent-value normalisation (null, blank or empty optional fields) runs
        // before the contract check, so such calls are not refused.
        resolved.backend.normalize_arguments(&mut command.arguments);
        let contract = enforce_command_contract(&mut command.arguments, &descriptor, context.schema_dialect);
        if let Err(error) = contract {
            return self
                .finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    None,
                    ToolExecutionStatus::InvalidArguments,
                    format!("Error: {error}"),
                    None,
                    Some(error),
                    None,
                    started,
                )
                .with_refusal_cause(ToolCallRefusalReason::InvalidArguments);
        }

        let h2_invocation = match async {
            use crate::runtime::h2_entry::{EntryAction, EntryInvocation};
            let requested = command.name == "skills_manage"
                && command
                    .arguments
                    .get("action")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|name| name.starts_with("lifecycle_experiment_"));
            if let Some(origin) = context.h2_origin() {
                origin.ensure_live()?;
                anyhow::ensure!(
                    origin.profile().permits_tool(&command.name),
                    "tool is outside the fixed controlled profile"
                );
                if command.name == "skills_manage" {
                    EntryAction::parse(
                        command
                            .arguments
                            .get("action")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default(),
                    )?;
                }
            }
            if !requested {
                return Ok::<_, anyhow::Error>(None);
            }
            let origin = context
                .h2_origin()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("experiment action has no actual controlled source"))?;
            let admission = context
                .execution_admission
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("experiment action has no admitted descriptor"))?;
            let verified = self
                .verify_admitted_call(&context.envelope, admission, &descriptor, &command)
                .map_err(|_| anyhow::anyhow!("experiment action has no admitted descriptor"))?;
            // The initial pending call still needs to be recorded first. Full
            // autonomy requests no approval and no review (R51); the
            // admitted-descriptor verification above still applies.
            let full = context.child_scope.as_ref().map_or_else(
                || self.policy.autonomy_level(),
                |scope| scope.gate_inputs().0.authority.level,
            ) == crate::security::AutonomyLevel::Full;
            if !full && crate::security::approval_operation::requires_explicit_grant(&command.name, &command.arguments)
            {
                return Ok(None);
            }
            Ok(Some(EntryInvocation::from_verified(origin, &verified, full).await?))
        }
        .await
        {
            Ok(invocation) => invocation,
            Err(error) => {
                let error = error.to_string();
                return self
                    .finish(
                        &command,
                        &context,
                        input_sha256,
                        Some(descriptor),
                        None,
                        ToolExecutionStatus::Denied,
                        format!("Error: {error}"),
                        None,
                        Some(error),
                        None,
                        started,
                    )
                    .with_refusal_cause(ToolCallRefusalReason::PolicyDenied);
            }
        };

        // A child runs at its sealed scope level: its parent's level, or a
        // named agent's own configured level (verified against the seal).
        // Without a child scope the call runs at the service's level.
        // A turn opened with a call context runs at that context's level.
        let effective_level = context.call.as_ref().map_or_else(
            || {
                context.child_scope.as_ref().map_or_else(
                    || self.policy.autonomy_level(),
                    |scope| scope.gate_inputs().0.authority.level,
                )
            },
            |call| call.authority.level,
        );
        let full = effective_level == crate::security::AutonomyLevel::Full;
        let decision = admission::child_autonomy(
            &context,
            &descriptor,
            self.policy.decide_at(&descriptor, &context, effective_level),
            effective_level,
        );
        // Ordinary calls honor the execution policy, including Full's Allow.
        // Only the verified controlled H2 ingress retains its durable exact
        // grant protocol, and only below Full: Full has no permission limits
        // (R51). Deny always remains Deny.
        let decision = if decision != ToolExecutionDecision::Deny
            && !full
            && context.h2_origin().is_some()
            && crate::security::approval_operation::requires_explicit_grant(&descriptor.public_name, &command.arguments)
        {
            ToolExecutionDecision::Ask
        } else {
            decision
        };
        if decision == ToolExecutionDecision::Deny {
            let error = format!(
                "tool '{}' is not permitted under the current execution policy",
                descriptor.public_name
            );
            self.record_call_refusal(
                &command,
                &context,
                &input_sha256,
                refusal_record::ToolCallRefusalReason::PolicyDenied,
                &error,
            )
            .await;
            return self
                .finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    ToolExecutionStatus::Denied,
                    format!("Error: {error}"),
                    None,
                    Some(error),
                    None,
                    started,
                )
                .with_refusal_cause(ToolCallRefusalReason::PolicyDenied);
        }
        // Below Full, a path the tool's own policy refuses never reaches an
        // approval prompt or a reservation (the structural-validation intent
        // above). The backend re-checks at execute time with the same
        // function. A private owner filesystem has its own resolver.
        if effective_level != crate::security::AutonomyLevel::Full && private_filesystem.is_none() {
            if let Err(refusal) = crate::context::call::scope(
                context.call.clone(),
                resolved
                    .backend
                    .path_precheck(&descriptor.public_name, &command.arguments),
            )
            .await
            {
                // The audit error and the durable row stay payload-free; only
                // the model-visible result and the operator notice name the path.
                let reason = refusal_record::ToolCallRefusalReason::from(refusal.reason());
                self.record_call_refusal(&command, &context, &input_sha256, reason, refusal.message())
                    .await;
                let error = format!("call refused before approval: {}", reason.as_str());
                let model_content = format!(
                    "Error: {}. The {} call did not run.",
                    refusal.message(),
                    descriptor.public_name
                );
                return self
                    .finish(
                        &command,
                        &context,
                        input_sha256,
                        Some(descriptor),
                        Some(decision),
                        ToolExecutionStatus::Denied,
                        model_content,
                        None,
                        Some(error),
                        None,
                        started,
                    )
                    .with_refusal_cause(reason);
            }
        }

        let mut runtime_approval_granted = false;
        let mut runtime_grant = None;
        let approval_run = context.envelope.run_id.clone().unwrap_or_default();
        let approval_channel = context.envelope.channel.clone().unwrap_or_default();
        let channel_gate =
            crate::agent::channel_approval::lookup_for_call(&approval_run, context.envelope.parent_run_id.as_deref());
        let prx_operator_ask = decision == ToolExecutionDecision::Ask;
        let channel_policy_ask =
            channel_gate.as_ref().is_some_and(|gate| gate.force_ask) && decision == ToolExecutionDecision::Allow;
        // The call's effective level decides, not Main's configured level.
        let decision = if channel_policy_ask && !full {
            ToolExecutionDecision::Ask
        } else {
            decision
        };
        if channel_policy_ask && full {
            runtime_approval_granted = true;
        }
        if decision == ToolExecutionDecision::Ask && context.pending_approval_denied {
            let reason = "approval is not available for this agent".to_string();
            self.record_approval_resolution(
                &command,
                &context,
                &input_sha256,
                ApprovalVerdict::Denied,
                Some(ApprovalDenialCause::NoResolver),
                Some(&reason),
            )
            .await;
            let model_content = approval_denied_model_content(&reason, &descriptor.public_name);
            let mut outcome = self.finish(
                &command,
                &context,
                input_sha256,
                Some(descriptor),
                Some(decision),
                ToolExecutionStatus::ApprovalDenied,
                model_content,
                None,
                Some(reason),
                None,
                started,
            );
            outcome.approval_denial_cause = Some(ApprovalDenialCause::NoResolver);
            return outcome;
        }
        // A supervised bridge owns its approval UI even when PRX also asks.
        // A bridge that does not force approval keeps the durable PRX route.
        if decision == ToolExecutionDecision::Ask
            && channel_gate
                .as_ref()
                .is_some_and(|gate| !prx_operator_ask || gate.force_ask)
        {
            let approval_resolution: std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Option<crate::agent::turn_contracts::ApprovalDecision>>
                        + Send
                        + '_,
                >,
            > = Box::pin(crate::agent::channel_approval::resolve_tool(
                &approval_run,
                &descriptor.public_name,
                &command.arguments,
                &approval_channel,
            ));
            crate::hooks::notify(
                crate::hooks::HookEvent::ApprovalRequested,
                serde_json::json!({"tool": descriptor.public_name, "call_id": command.operation_id}),
            );
            let resolved = approval_resolution.await;
            crate::hooks::notify(
                crate::hooks::HookEvent::ApprovalDecided,
                serde_json::json!({
                    "tool": descriptor.public_name,
                    "call_id": command.operation_id,
                    "decision": if matches!(
                        resolved,
                        Some(
                            crate::agent::turn_contracts::ApprovalDecision::Grant
                                | crate::agent::turn_contracts::ApprovalDecision::GrantForSession
                        )
                    ) {
                        "approved"
                    } else {
                        "denied"
                    },
                }),
            );
            if !matches!(
                resolved,
                Some(
                    crate::agent::turn_contracts::ApprovalDecision::Grant
                        | crate::agent::turn_contracts::ApprovalDecision::GrantForSession
                )
            ) {
                let reason = APPROVAL_DENIED_CHANNEL.to_string();
                self.record_approval_resolution(
                    &command,
                    &context,
                    &input_sha256,
                    ApprovalVerdict::Denied,
                    Some(ApprovalDenialCause::ChannelDenied),
                    Some(&reason),
                )
                .await;
                let model_content = approval_denied_model_content(&reason, &descriptor.public_name);
                let mut outcome = self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    ToolExecutionStatus::ApprovalDenied,
                    model_content,
                    None,
                    Some(reason),
                    None,
                    started,
                );
                outcome.approval_denial_cause = Some(ApprovalDenialCause::ChannelDenied);
                return outcome;
            }
            self.record_approval_resolution(&command, &context, &input_sha256, ApprovalVerdict::Approved, None, None)
                .await;
            runtime_approval_granted = true;
            if prx_operator_ask {
                runtime_grant = self.policy.approved_call_grant(&command, &context);
            }
        } else if decision == ToolExecutionDecision::Ask {
            let (approval, ledger_route) = self
                .resolve_ordinary_approval(ToolApprovalRequest {
                    command: command.clone(),
                    descriptor: descriptor.clone(),
                    context: context.clone(),
                })
                .await;
            match approval {
                ToolApprovalDecision::Approved {
                    runtime_approval_granted: approved,
                    runtime_grant: grant,
                } => {
                    self.record_approval_resolution(
                        &command,
                        &context,
                        &input_sha256,
                        ApprovalVerdict::Approved,
                        None,
                        None,
                    )
                    .await;
                    runtime_approval_granted = approved;
                    runtime_grant = grant;
                }
                ToolApprovalDecision::Denied { cause, reason } => {
                    self.record_approval_resolution(
                        &command,
                        &context,
                        &input_sha256,
                        ApprovalVerdict::Denied,
                        Some(cause),
                        Some(&reason),
                    )
                    .await;
                    let model_content = approval_denied_model_content(&reason, &descriptor.public_name);
                    let mut outcome = self.finish(
                        &command,
                        &context,
                        input_sha256,
                        Some(descriptor),
                        Some(decision),
                        ToolExecutionStatus::ApprovalDenied,
                        model_content,
                        None,
                        Some(reason),
                        None,
                        started,
                    );
                    outcome.approval_denial_cause = Some(cause);
                    return outcome;
                }
                ToolApprovalDecision::Cancelled { reason } => {
                    // An approval-ledger route that is still awaiting a grant
                    // stays pending: it writes no resolution row.
                    if !ledger_route {
                        self.record_approval_resolution(
                            &command,
                            &context,
                            &input_sha256,
                            ApprovalVerdict::Cancelled,
                            None,
                            Some(&reason),
                        )
                        .await;
                    }
                    return self.finish(
                        &command,
                        &context,
                        input_sha256,
                        Some(descriptor),
                        Some(decision),
                        if ledger_route {
                            ToolExecutionStatus::AwaitingApproval
                        } else {
                            ToolExecutionStatus::Cancelled
                        },
                        format!("Error: {reason}"),
                        None,
                        Some(reason),
                        None,
                        started,
                    );
                }
            }
        }

        let preparation = match self.preparation.prepare(&descriptor, &command, &context).await {
            Ok(permit) => permit,
            Err(error) => {
                return self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    ToolExecutionStatus::PreparationDenied,
                    format!("Error: preparation denied tool execution: {error}"),
                    None,
                    Some(error),
                    None,
                    started,
                );
            }
        };

        let arguments = match normalize_arguments(&command.arguments, &context, runtime_approval_granted, runtime_grant)
        {
            Ok(arguments) => arguments,
            Err(error) => {
                return self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    ToolExecutionStatus::InvalidArguments,
                    format!("Error: {error}"),
                    None,
                    Some(error),
                    Some(preparation),
                    started,
                );
            }
        };
        if cancellation.as_ref().is_some_and(CancellationToken::is_cancelled) {
            return self.finish(
                &command,
                &context,
                input_sha256,
                Some(descriptor),
                Some(decision),
                ToolExecutionStatus::Cancelled,
                "Error: tool execution cancelled".to_string(),
                None,
                Some("tool execution cancelled before reservation".to_string()),
                Some(preparation),
                started,
            );
        }

        let exact_call = context
            .execution_trace
            .as_ref()
            .and_then(|trace| trace.begin_exact_call(&command, &context, &input_sha256));
        let idempotency_lease = match self
            .prepare_idempotent_execution(
                &command,
                &context,
                &input_sha256,
                &descriptor,
                decision,
                &preparation,
                started,
            )
            .await
        {
            Ok(lease) => {
                if let Some(observation) = &exact_call {
                    observation.reserved(lease.as_ref().map(|lease| lease.reservation.attempt));
                }
                lease
            }
            Err(outcome) => {
                if let Some(observation) = exact_call {
                    observation.refused(outcome.status, outcome.replayed);
                }
                return *outcome;
            }
        };
        #[cfg(test)]
        if let Some(gate) = self
            .post_reservation_gate
            .as_ref()
            .filter(|gate| gate.armed.swap(false, AtomicOrdering::AcqRel))
        {
            gate.entered.notify_one();
            gate.release.notified().await;
        }

        // A scheduled claim can expire while approval, preparation or the
        // idempotency reservation awaits. Validate its live authority before
        // constructing/polling the backend future. The guard may read SQLite,
        // so it runs on the blocking pool, never inside the schema verifier.
        if let Some(authority_guard) = context.envelope.authority_guard.clone() {
            let validate = async {
                crate::runtime::blocking::spawn_blocking(move || authority_guard.validate())
                    .await
                    .is_ok_and(|result| result.unwrap_or(false))
            };
            let current = if let Some(token) = cancellation.as_ref() {
                tokio::select! {
                    biased;
                    () = token.cancelled() => false,
                    current = validate => current,
                }
            } else {
                validate.await
            };
            if !current {
                let cancelled = cancellation.as_ref().is_some_and(CancellationToken::is_cancelled);
                let status = if cancelled {
                    ToolExecutionStatus::Cancelled
                } else {
                    ToolExecutionStatus::StaleCapability
                };
                let error = if cancelled {
                    "tool execution cancelled during runtime authority validation"
                } else {
                    "runtime execution authority is expired or unavailable"
                };
                let abandonment = self
                    .abandon_idempotent_execution(idempotency_lease.as_ref(), &context)
                    .await;
                let (status, error) = match abandonment {
                    Ok(()) => (status, error.to_owned()),
                    Err(error) => (ToolExecutionStatus::Indeterminate, error),
                };
                if let Some(observation) = exact_call {
                    observation.complete(status);
                }
                return self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    status,
                    format!("Error: {error}"),
                    None,
                    Some(error),
                    Some(preparation),
                    started,
                );
            }
        }

        // Approval, preparation, reservation and claim may have awaited while the
        // live registry changed. Recheck before the backend future is polled.
        if let Err((status, error)) = admission::admit_child(self, &context, &command.name, true)
            .and_then(|()| admission::admit_called_tokens(self, &context, &command.name))
            .and_then(|()| admission::admit_called_spend(self, &context, &command.name))
        {
            let abandonment = self
                .abandon_idempotent_execution(idempotency_lease.as_ref(), &context)
                .await;
            let (status, error) = match abandonment {
                Ok(()) => (status, error),
                Err(error) => (ToolExecutionStatus::Indeterminate, error),
            };
            if let Some(observation) = exact_call {
                observation.complete(status);
            }
            return self.finish(
                &command,
                &context,
                input_sha256,
                Some(descriptor),
                Some(decision),
                status,
                format!("Error: {error}"),
                None,
                Some(error),
                Some(preparation),
                started,
            );
        }
        if let Some(binding) = &context.admitted_binding {
            let verified = (context.execution_trace.as_ref().is_some_and(|trace| !trace.is_sealed()))
                .then_some(binding)
                .ok_or(super::capability::execution_binding::ToolExecutionError::StaleCapability)
                .and_then(|binding| binding.verify_admitted_descriptor(&context.envelope, &descriptor));
            if let Err(error) = verified {
                let abandonment = self
                    .abandon_idempotent_execution(idempotency_lease.as_ref(), &context)
                    .await;
                let (status, error) = match abandonment {
                    Ok(()) => (ToolExecutionStatus::StaleCapability, error.to_string()),
                    Err(error) => (ToolExecutionStatus::Indeterminate, error),
                };
                if let Some(observation) = exact_call {
                    observation.complete(status);
                }
                return self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    status,
                    format!("Error: {error}"),
                    None,
                    Some(error),
                    Some(preparation),
                    started,
                );
            }
        }
        // The resolved Arc is immutable for the in-flight call even if a
        // dynamic catalog refresh changes future descriptor snapshots.
        let backend = resolved.backend;
        let child_tool_name = match command.name.as_str() {
            "delegate" => Some("delegate"),
            "sessions_spawn" => Some("sessions_spawn"),
            "sessions_send" => Some("sessions_send"),
            "mcp_call" => Some("mcp_call"),
            _ => None,
        };
        let child_call_context = child_tool_name.map(|tool_name| ChildCallContext {
            tool_name,
            context: context.clone(),
        });
        // Scoped so any child process the tool spawns registers with this call
        // as its parent, which is what makes a cascading kill possible.
        let backend_cancellation = cancellation.clone();
        let backend_h2_invocation = h2_invocation.clone();
        let exact_call_ref = exact_call.as_ref();
        let public_name = command.name.as_str();
        let execution_parameters = descriptor.execution_parameters.clone();
        let schema_dialect = context.schema_dialect;
        let call_context = context.call.clone();
        let live_frames = context
            .live_turn
            .as_ref()
            .map(|live| live.0.frames(&context.envelope, &std::collections::HashSet::new()));
        let operation_id = &command.operation_id;
        let tool_future = crate::runtime::registry::scope_current(
            work.id(),
            CHILD_CALL_CONTEXT.scope(
                child_call_context,
                super::capability::search_tool::with_search_authority(
                    if command.name == "capability_search" {
                        context.capability_search.clone()
                    } else {
                        None
                    },
                    super::capability::execution_binding::scope_dispatch_binding(
                        if backend.root_name() == "mcp_call" {
                            context.admitted_binding.as_ref()
                        } else {
                            None
                        },
                        async move {
                            // Emit only at backend entry, after approval and all
                            // admission gates, without changing legacy event sinks.
                            if let Some(live) = live_frames {
                                live.emit(&crate::agent::turn_contracts::ToolLoopEvent::ToolStarted {
                                    tool_call_id: operation_id.clone(),
                                    name: public_name.to_owned(),
                                    args: String::new(),
                                });
                            }
                            if let Some(observation) = exact_call_ref {
                                observation.backend_started();
                            }
                            super::owner_filesystem_dispatch::scope(
                                private_filesystem,
                                crate::tools::schema::with_execution_contract(
                                    execution_parameters,
                                    schema_dialect,
                                    crate::runtime::h2_entry::scope_invocation(
                                        backend_h2_invocation.map(|invocation| {
                                            (invocation, backend_cancellation.clone().unwrap_or_default())
                                        }),
                                        crate::context::call::scope(
                                            call_context,
                                            super::effective_autonomy::scope(
                                                effective_level,
                                                backend.invoke_observed(public_name, arguments, backend_cancellation),
                                            ),
                                        ),
                                    ),
                                ),
                            )
                            .await
                        },
                    ),
                ),
            ),
        );
        let tool_future = async {
            if let Some(invocation) = &h2_invocation {
                invocation.ensure_live()?;
            }
            tool_future.await
        };
        let mut cancelled_before_invoke = false;
        let raw_result = if let (Some(token), Some(_)) = (cancellation.clone(), idempotency_lease.as_ref()) {
            // The outer cancellation race owns only the ReservedNotStarted
            // phase. The wrapper marks the handoff immediately before the
            // backend's first poll. Once that boundary is crossed we retain
            // ownership and drive the backend's cooperative cancellation to a
            // terminal result instead of dropping an indeterminate side
            // effect.
            let backend_started = Arc::new(AtomicBool::new(false));
            tokio::pin!(tool_future);
            let started_for_poll = Arc::clone(&backend_started);
            let tracked_future = std::future::poll_fn(move |context| {
                started_for_poll.store(true, AtomicOrdering::Release);
                tool_future.as_mut().poll(context)
            });
            tokio::pin!(tracked_future);
            tokio::select! {
                biased;
                () = token.cancelled() => {
                    if backend_started.load(AtomicOrdering::Acquire) {
                        Some(tracked_future.as_mut().await)
                    } else {
                        cancelled_before_invoke = true;
                        None
                    }
                },
                result = tracked_future.as_mut() => Some(result),
            }
        } else if let Some(token) = cancellation {
            tokio::select! {
                biased;
                () = token.cancelled() => None,
                result = tool_future => Some(result),
            }
        } else {
            Some(tool_future.await)
        };

        let abandonment_error = if cancelled_before_invoke {
            self.abandon_idempotent_execution(idempotency_lease.as_ref(), &context)
                .await
                .err()
        } else {
            None
        };
        let (raw_result, process) = match raw_result {
            Some(Ok(observed)) => (Some(Ok(observed.result)), observed.process),
            Some(Err(error)) => (Some(Err(error)), None),
            None => (None, None),
        };
        let mut outcome = match (raw_result, abandonment_error) {
            (None, Some(error)) => self.finish(
                &command,
                &context,
                input_sha256,
                Some(descriptor),
                Some(decision),
                ToolExecutionStatus::Indeterminate,
                format!("Error: {error}"),
                None,
                Some(error),
                Some(preparation),
                started,
            ),
            (None, None) => self.finish(
                &command,
                &context,
                input_sha256,
                Some(descriptor),
                Some(decision),
                ToolExecutionStatus::Cancelled,
                "Error: tool execution cancelled".to_string(),
                None,
                Some("tool execution cancelled".to_string()),
                Some(preparation),
                started,
            ),
            (Some(Ok(result)), _)
                if is_tool_cancelled_result(&result)
                    || process
                        .as_ref()
                        .is_some_and(|process| process.termination == super::traits::ProcessTermination::Cancelled) =>
            {
                self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    ToolExecutionStatus::Cancelled,
                    "Error: tool execution cancelled".to_string(),
                    Some(result),
                    Some("tool execution cancelled".to_string()),
                    Some(preparation),
                    started,
                )
            }
            (Some(Ok(result)), _) => {
                let (status, model_content, error) = if result.success {
                    let model_content = match result.error.as_deref().filter(|error| !error.is_empty()) {
                        Some(diagnostics) => format!("Output:\n{}\n\nDiagnostics:\n{diagnostics}", result.output),
                        None => result.output.clone(),
                    };
                    (ToolExecutionStatus::Succeeded, model_content, None)
                } else {
                    let error = process
                        .as_ref()
                        .map(|process| {
                            process
                                .diagnostic
                                .clone()
                                .unwrap_or_else(|| result.error.clone().unwrap_or_else(|| process.summary()))
                        })
                        .unwrap_or_else(|| result.error.clone().unwrap_or_else(|| result.output.clone()));
                    let hint = super::error_hints::recovery_hint(&command.name, &error);
                    let mut model_content = if hint.is_empty() {
                        format!("Error: {error}")
                    } else {
                        format!("Error: {error}\n{hint}")
                    };
                    // `error` is a separate diagnostic channel, not a replacement
                    // for output produced before failure. Keep the Error prefix
                    // used by existing history consumers and avoid duplicating
                    // output when it was the fallback error above.
                    if result.error.is_some() && !result.output.is_empty() {
                        model_content.push_str(&format!("\n\nOutput:\n{}", result.output));
                    }
                    (ToolExecutionStatus::Failed, model_content, Some(error))
                };
                self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    status,
                    model_content,
                    Some(result),
                    error,
                    Some(preparation),
                    started,
                )
            }
            (Some(Err(error)), _) => {
                // The trace alone classifies the full source chain; the outcome and
                // everything built from it keep the plain message.
                if let Some(trace) = context.execution_trace.as_ref() {
                    trace.note_error_detail(&command.operation_id, format!("{error:#}"));
                }
                let status = if error
                    .downcast_ref::<crate::tools::schema::ToolArgumentError>()
                    .is_some()
                {
                    ToolExecutionStatus::InvalidArguments
                } else {
                    ToolExecutionStatus::Failed
                };
                let error = error.to_string();
                let hint = super::error_hints::recovery_hint(&command.name, &error);
                let model_content = if hint.is_empty() {
                    format!("Error executing {}: {error}", command.name)
                } else {
                    format!("Error executing {}: {error}\n{hint}", command.name)
                };
                self.finish(
                    &command,
                    &context,
                    input_sha256,
                    Some(descriptor),
                    Some(decision),
                    status,
                    model_content,
                    None,
                    Some(error),
                    Some(preparation),
                    started,
                )
            }
        };
        outcome.process = process;
        if let (Some(process), Some(result)) = (&outcome.process, &outcome.result) {
            outcome.model_content = process.render(result);
            if !result.success {
                let hint =
                    super::error_hints::recovery_hint(&command.name, outcome.error.as_deref().unwrap_or_default());
                if !hint.is_empty() {
                    outcome.model_content.push('\n');
                    outcome.model_content.push_str(&hint);
                }
            }
        }
        if let Some(observation) = exact_call {
            observation.complete(outcome.status);
        }
        if !cancelled_before_invoke {
            self.commit_idempotent_execution(idempotency_lease.as_ref(), &context, &outcome)
                .await;
        }
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        command: &ToolExecutionCommand,
        context: &ToolExecutionContext,
        input_sha256: String,
        descriptor: Option<ToolDescriptor>,
        decision: Option<ToolExecutionDecision>,
        status: ToolExecutionStatus,
        model_content: String,
        result: Option<ToolResult>,
        error: Option<String>,
        preparation: Option<ToolExecutionPermit>,
        started: Instant,
    ) -> ToolExecutionOutcome {
        // ... and a tool call producing its terminal outcome is progress too.
        crate::agent::idle::beat(crate::agent::idle::ProgressKind::ToolEnd);
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.audit.record(ToolExecutionAuditRecord {
            operation_id: command.operation_id.clone(),
            capability: command.name.clone(),
            backend_name: descriptor.as_ref().map(|value| value.backend_name.clone()),
            adapter: descriptor.as_ref().map(|value| value.adapter),
            effect: descriptor.as_ref().map(|value| value.effect),
            decision,
            status,
            preparation_strategy: preparation.as_ref().map(|value| value.strategy.clone()),
            input_sha256,
            source: context.envelope.source.as_str().to_string(),
            workspace_id: context.envelope.workspace_id.clone(),
            session_key: context.envelope.session_key.clone(),
            run_id: context.envelope.run_id.clone(),
            parent_run_id: context.envelope.parent_run_id.clone(),
            task_id: context.envelope.task_id.clone(),
            error: error.clone(),
            duration_ms,
        });
        ToolExecutionOutcome {
            operation_id: command.operation_id.clone(),
            descriptor,
            decision,
            status,
            model_content,
            result,
            process: None,
            error,
            preparation,
            duration_ms,
            replayed: false,
            approval_denial_cause: None,
            refusal_cause: None,
        }
    }
}

fn tool_execution_event_input(
    context: &ToolExecutionContext,
    idempotency_key: &str,
    event_type: &str,
    content: String,
    raw_payload_json: String,
) -> MessageEventInput {
    MessageEventInput {
        event_id: None,
        idempotency_key: Some(idempotency_key.to_string()),
        workspace_id: context.envelope.workspace_id.clone(),
        owner_id: context.envelope.owner_id.clone(),
        source: context.envelope.source.as_str().into(),
        channel: context.envelope.channel.clone(),
        session_key: Some(context.envelope.session_key.clone()),
        agent_partition: context.envelope.agent_partition.clone(),
        parent_session_key: context.envelope.parent_session_key.clone(),
        run_id: context.envelope.run_id.clone(),
        parent_run_id: context.envelope.parent_run_id.clone(),
        agent_id: context.envelope.agent_id.clone(),
        persona_id: context.envelope.persona_id.clone(),
        sender: context.envelope.sender.clone(),
        recipient: context.envelope.recipient.clone(),
        role: "event".to_string(),
        event_type: event_type.to_string(),
        subject: None,
        goal_id: None,
        causation_event_id: context.envelope.source_message_event_id.clone(),
        correlation_id: context.envelope.run_id.clone(),
        attempt_id: context.envelope.attempt_id.clone(),
        lease_epoch: context.envelope.lease_epoch,
        config_generation_id: context.envelope.config_generation_id,
        config_source_revision: context.envelope.config_source_revision.clone(),
        content,
        raw_payload_json: Some(raw_payload_json),
        visibility: context.envelope.visibility.clone(),
    }
}

/// Tool-argument name prefixes reserved for the runtime.
///
/// Every argument under one of these prefixes is *runtime-only*: it is written
/// by the runtime immediately before a tool executes and it carries trust —
/// the caller scope (`_zc_scope`, `_zc_scope_trusted`, `_prx_scope_trusted`),
/// the approval grant (`_zc_approval_granted`, `_zc_approval_grant`) and the
/// principal binding (`_zc_principal`). None of it may ever survive from an
/// externally supplied payload (model output, an MCP gateway HTTP body, a
/// third-party tool result) into tool execution, and none of it may leak back
/// out to a third-party MCP server.
///
/// Stripping is by prefix on purpose. A per-key blocklist fails *open*: adding
/// a new `_zc_*` key and forgetting one of the injection sites silently accepts
/// a forged one. With a prefix the same mistake fails *closed* — the runtime
/// key simply goes missing, which surfaces immediately as a denied or
/// untrusted call instead of an authorization bypass.
pub const RUNTIME_ONLY_ARG_PREFIXES: [&str; 2] = ["_zc_", "_prx_"];

/// Tools that enforce per-owner isolation for an owner-isolated origin
/// (V02-R9 / OQ-21).
///
/// An owner-isolated context — today an externally authenticated MCP caller —
/// may execute only these. Stateful tools rebuild their principal from the service-authored
/// `_zc_scope` and applies [`scope_requires_owner_match`] (or the run registry's
/// owner scope) to every row it reads or deletes. Every other tool is refused:
/// autonomy approval is not consulted at `AutonomyLevel::Full`, so the operator's
/// `mcp_server.exposed_tools` list would otherwise be the only boundary between
/// an external caller and a mutating tool that does not scope its effect by
/// owner. A tool joins this list only once it is owner-aware or reads no private
/// runtime state. Web search is stateless; its provider key and quota are shared.
///
/// `session_status` and `sessions_send` are owner-aware for in-process turns
/// but the gateway does not register them, so they stay off the list until it
/// does: an exposed entry is then refused with 403 before any identity binding
/// is written, instead of passing this gate and answering 404 afterwards.
pub use crate::tools::capability::policy::OWNER_ISOLATION_AWARE_TOOLS;

/// Whether `name` is on [`OWNER_ISOLATION_AWARE_TOOLS`].
#[must_use]
pub fn is_owner_isolation_aware_tool(name: &str) -> bool {
    OWNER_ISOLATION_AWARE_TOOLS.contains(&name)
}

/// Whether a resolved tool may run for an owner-isolated origin.
///
/// The public name must be on the list **and** be served by the backend of the
/// same name, so a dynamic MCP, WASM or skill alias that happens to publish a
/// listed name from another backend is refused: it is not the audited tool.
#[must_use]
pub fn admits_owner_isolated_origin(descriptor: &ToolDescriptor) -> bool {
    crate::tools::capability::policy::admits_owner_isolated_identity(&descriptor.public_name, &descriptor.backend_name)
}

/// The refusal given to an owner-isolated caller for a tool that does not
/// enforce per-owner isolation.
#[must_use]
pub fn owner_isolation_refusal(tool_name: &str) -> String {
    format!("tool '{tool_name}' is not available to an owner-isolated caller: it does not enforce per-owner isolation")
}

/// Key inside the service-authored `_zc_scope` that marks the turn's origin as
/// owner-isolated (C03 / V02-R4).
///
/// Written only when [`crate::runtime::envelope::RuntimeEnvelope::owner_isolated`]
/// is set, which today means an externally authenticated MCP caller. Tools that
/// rebuild a [`crate::memory::MemoryPrincipal`] from the scope read it back with
/// [`scope_requires_owner_match`].
pub const SCOPE_OWNER_ISOLATED_KEY: &str = "owner_isolated";

/// The agent partition the runtime authored into a trusted `_zc_scope` (MA-01b3).
///
/// Read only when `_zc_scope_trusted` is literally `true`, so a model-supplied
/// `_zc_scope` never yields a partition. A malformed value reads as `None`.
#[must_use]
pub(crate) fn trusted_scope_agent_partition(args: &serde_json::Value) -> Option<crate::memory::AgentPartition> {
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !trusted {
        return None;
    }
    let raw = args.get("_zc_scope")?.get("agent_partition")?;
    match serde_json::from_value(raw.clone()) {
        Ok(partition) => Some(partition),
        Err(error) => {
            tracing::warn!(error = %error, "ignoring malformed trusted agent_partition in _zc_scope");
            None
        }
    }
}

/// The agent partition a read made for this call must stay in (MA-01h).
///
/// `Ok(None)` only when the call carries no trusted scope or its trusted scope
/// names no partition (a turn that runs under none, such as the terminal). A
/// trusted scope whose partition is present but unreadable is refused, never
/// read as unpartitioned.
///
/// # Errors
///
/// [`crate::memory::AgentPartitionUnreadable`] for an unreadable partition.
pub(crate) fn trusted_scope_read_partition(
    args: &serde_json::Value,
) -> Result<Option<crate::memory::AgentPartition>, crate::memory::AgentPartitionUnreadable> {
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !trusted {
        return Ok(None);
    }
    let Some(raw) = args.get("_zc_scope").and_then(|scope| scope.get("agent_partition")) else {
        return Ok(None);
    };
    serde_json::from_value(raw.clone()).map(Some).map_err(|error| {
        tracing::warn!(
            error_category = ?error.classify(),
            "refusing a read whose trusted agent_partition is malformed"
        );
        crate::memory::AgentPartitionUnreadable
    })
}

/// Who is creating scheduled work with this call, and at what level: the
/// call's level from the execution service's carrier (`None` off the service
/// path) and the agent from the trusted scope's partition. A trusted scope
/// without a partition is Main's; an untrusted one names nobody.
#[must_use]
pub(crate) fn creator_stamp_from_trusted_args(args: &serde_json::Value) -> crate::security::agent_level::CreatorStamp {
    use crate::memory::PartitionKey;
    use crate::security::agent_level::{CREATOR_MAIN, CreatorStamp, creator_agent_named};
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && args.get("_zc_scope").is_some_and(serde_json::Value::is_object);
    let agent = trusted.then(
        || match trusted_scope_agent_partition(args).map(|partition| partition.key) {
            Some(PartitionKey::Named { name, incarnation }) => creator_agent_named(name.as_str(), &incarnation),
            Some(PartitionKey::Main) | None => CREATOR_MAIN.to_owned(),
        },
    );
    CreatorStamp {
        level: super::effective_autonomy::current(),
        agent,
    }
}

/// Read the owner-isolation marker back out of tool arguments.
///
/// Honoured only when `_zc_scope_trusted` is set, and that whole block is
/// re-authored by [`normalize_arguments`] from the authenticated execution
/// context after [`strip_runtime_only_args`] has dropped everything the caller
/// supplied — so a caller can neither forge the marker nor clear it.
#[must_use]
pub fn scope_requires_owner_match(args: &serde_json::Value) -> bool {
    args.get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && args
            .get("_zc_scope")
            .and_then(serde_json::Value::as_object)
            .and_then(|scope| scope.get(SCOPE_OWNER_ISOLATED_KEY))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

/// Whether `key` names a runtime-only tool argument (see
/// [`RUNTIME_ONLY_ARG_PREFIXES`]).
#[must_use]
pub fn is_runtime_only_arg(key: &str) -> bool {
    RUNTIME_ONLY_ARG_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
}

/// Single source of truth for scrubbing runtime-only arguments off a tool
/// argument object.
///
/// Every site that injects trusted runtime state into tool arguments must call
/// this first, and every site that forwards arguments to an untrusted process
/// must call it last.
pub fn strip_runtime_only_args(root: &mut serde_json::Map<String, serde_json::Value>) {
    root.retain(|key, _| !is_runtime_only_arg(key));
}

/// Enforce a tool's published contract on caller-supplied arguments: validate
/// them, then make the document say what the contract means.
///
/// The descriptor's full `execution_parameters` (or public `parameters` when
/// no private contract exists) is the truth source: native,
/// skill, MCP alias and WASM plugin tools all reach this gate. Migrated built-ins
/// keep their original conditional rules here while publishing a flat schema.
/// `dialect` replays the provider's keyword lowering on the full contract:
/// Gemini strips `additionalProperties` and bounds using the same
/// [`crate::tools::schema::SchemaCleanr`] strategy as publication. Runtime-only
/// keys are removed first because the runtime authors them after validation and
/// they are intentionally absent from public schemas.
///
/// Before validation, `null` on non-required declared properties whose published
/// schema does not accept `null` is dropped (see
/// [`crate::tools::schema::CompiledToolSchema::drop_null_optional_properties`]).
///
/// On success the arguments are normalized in place — see
/// [`crate::tools::schema::CompiledToolSchema::normalize_integral_numbers`].
/// That is why this takes `&mut`: enforcement is not only a verdict.
pub(crate) fn enforce_command_contract(
    arguments: &mut serde_json::Value,
    descriptor: &ToolDescriptor,
    dialect: ToolSchemaDialect,
) -> Result<(), String> {
    let compiled = crate::tools::schema::compiled_tool_schema_for_dialect(
        &descriptor.public_name,
        dialect,
        descriptor
            .execution_parameters
            .as_deref()
            .unwrap_or(&descriptor.parameters),
    );
    if !arguments.is_object() {
        return Err("tool arguments must be a JSON object".to_string());
    }
    // A `null` on an optional property means "not sent": drop it so the
    // validator and the tool both see the key as absent. Nullability is read
    // from the schema as published (Raw), not the provider-lowered one, so the
    // same call is treated the same on every provider.
    crate::tools::schema::compiled_tool_schema_for_dialect(
        &descriptor.public_name,
        ToolSchemaDialect::Raw,
        descriptor
            .execution_parameters
            .as_deref()
            .unwrap_or(&descriptor.parameters),
    )
    .drop_null_optional_properties(arguments);
    let issues =
        crate::tools::schema::with_public_arguments(arguments, |public| compiled.validate_tool_arguments(public));
    if !issues.is_empty() {
        return Err(crate::tools::schema::describe_argument_validation_issues(
            &descriptor.public_name,
            &issues,
        ));
    }
    // The contract holds, so the document is now made to say what the contract
    // says it means: `integer` is a mathematical value in JSON Schema, and a
    // model that answers `200.0` for one is correct — but every executor reads
    // integers with `as_u64()` / `as_i64()`, which see `Float(200.0)` as absent
    // and fall back to a default. Coercing here, against the published schema,
    // is the one place that knows which fields were declared `integer`; doing
    // it in each executor would be ~20 copies of a rule none of them can state.
    compiled.normalize_integral_numbers(arguments);
    Ok(())
}

fn normalize_arguments(
    arguments: &serde_json::Value,
    context: &ToolExecutionContext,
    runtime_approval_granted: bool,
    runtime_grant: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut arguments = arguments.clone();
    let root = arguments
        .as_object_mut()
        .ok_or_else(|| "tool arguments must be a JSON object".to_string())?;

    // Structural validation against `descriptor.parameters` already ran in
    // `ToolExecutionService::execute`, before approval. Re-checking here would
    // be a second, divergent rule set.

    // Single source of truth: drop every runtime-only key an external caller
    // may have supplied before re-injecting the authoritative ones below.
    strip_runtime_only_args(root);

    let mut scope = serde_json::json!({
        "sender": context.sender(),
        "channel": context.channel(),
        "chat_type": context.chat_type,
        "chat_id": context.chat_id,
        "workspace_id": context.envelope.workspace_id,
        "session_key": context.envelope.session_key,
        "runtime_source": context.envelope.source.as_str(),
    });
    if let Some(scope) = scope.as_object_mut() {
        insert_optional(scope, "owner_id", context.envelope.owner_id.as_deref());
        // C03: only written when the origin is owner-isolated, so every other
        // turn's authored scope stays byte-identical to the pre-C03 one.
        if context.envelope.owner_isolated {
            scope.insert(SCOPE_OWNER_ISOLATED_KEY.to_string(), serde_json::Value::Bool(true));
        }
        if let Some(partition) = &context.envelope.agent_partition {
            scope.insert(
                "agent_partition".to_string(),
                serde_json::to_value(partition).map_err(|error| error.to_string())?,
            );
        }
        insert_optional(scope, "topic_id", context.envelope.topic_id.as_deref());
        insert_optional(scope, "task_id", context.envelope.task_id.as_deref());
        insert_optional(scope, "run_id", context.envelope.run_id.as_deref());
        insert_optional(scope, "parent_run_id", context.envelope.parent_run_id.as_deref());
        insert_optional(
            scope,
            "source_message_event_id",
            context.envelope.source_message_event_id.as_deref(),
        );
        if let Some(config_generation_id) = context.envelope.config_generation_id {
            scope.insert(
                "config_generation_id".to_string(),
                serde_json::Value::Number(config_generation_id.into()),
            );
        }
        insert_optional(
            scope,
            "config_source_revision",
            context.envelope.config_source_revision.as_deref(),
        );
    }
    root.insert("_zc_scope".to_string(), scope);
    root.insert("_zc_scope_trusted".to_string(), serde_json::Value::Bool(true));
    // Local-operator origin marker (T18). Unlike `_zc_scope_trusted` — which
    // asserts "the `_zc_scope` block above was written by the runtime", and is
    // therefore always true here — this one asserts "*a human at this machine's
    // terminal* drove this turn". It is the guard on destructive `gateway`
    // actions. Authored here and only here: the caller-supplied value was
    // already dropped by `strip_runtime_only_args` above, so whatever the model
    // or an HTTP body asked for is irrelevant either way.
    root.insert(
        "_prx_scope_trusted".to_string(),
        serde_json::Value::Bool(context.is_local_operator()),
    );
    // Local-root marker: the call's root principal is the local operator
    // (see `ToolExecutionContext::is_local_root`). At Full it admits the
    // destructive `gateway` actions for the operator's children and jobs.
    // Authored here and only here, after caller values were stripped.
    root.insert(
        "_prx_local_root".to_string(),
        serde_json::Value::Bool(context.is_local_root()),
    );
    root.insert(
        RUNTIME_APPROVAL_GRANTED_ARG.to_string(),
        serde_json::Value::Bool(runtime_approval_granted),
    );
    if let Some(grant) = runtime_grant {
        root.insert(RUNTIME_APPROVAL_GRANT_ARG.to_string(), grant);
    }
    Ok(arguments)
}

fn insert_optional(map: &mut serde_json::Map<String, serde_json::Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        map.insert(key.to_string(), serde_json::Value::String(value.to_string()));
    }
}
