pub async fn post_session_message(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    request: Request,
) -> impl IntoResponse {
    let principal = request.extensions().get::<super::ApiPrincipal>().cloned();
    // The runtime is pinned before the body is read, as before.
    let admitted = state.admit_turn_runtime();
    // A message to a caller-keyed session, or a body with an rf field, is an
    // rf message (AC-1b1); any other request takes the Console path below,
    // unchanged.
    let (request, unreadable) = match rf_message::classify(&state, &session_id, request).await {
        rf_message::MessageShape::Rf(parts, body) => {
            return rf_message::post_rf_message(state, session_id, parts, body, admitted).await;
        }
        rf_message::MessageShape::Console { request, unreadable } => (request, unreadable),
    };
    let Some(api_principal) = principal else {
        return json_error(StatusCode::UNAUTHORIZED, "Authenticated API principal missing");
    };
    let turn_runtime = match admitted {
        Ok(runtime) => runtime,
        Err(refusal) => return super::provider_not_ready(refusal),
    };
    if unreadable {
        // The body could not be buffered: the JSON parser's own refusal.
        return json_error(StatusCode::BAD_REQUEST, "Invalid JSON payload");
    }
    let headers = request.headers().clone();
    let owner_authenticated = super::owner_authenticated(&state, &headers);
    let async_mode = jobs::wants_async(request.uri().query(), &headers);
    let parsed = match parse_message_from_request(&state, request).await {
        Ok(parsed) => parsed,
        Err(error_response) => return error_response,
    };
    let ParsedMessage {
        message,
        agent,
        uploads,
    } = parsed;

    let selected = match select_turn_target(&state, turn_runtime, &api_principal, agent, owner_authenticated) {
        Ok(selected) => selected,
        // A refused target leaves nothing behind: dropping the guard removes
        // the files this request created, and only those.
        Err(refusal) => return refusal.into_response(),
    };
    // The guard stays alive through admission: a missing session, a failed
    // history read, closed admission or cancellation removes the files.
    submit_console_message(
        state,
        session_id,
        api_principal,
        ConsoleSubmission {
            selected,
            message,
            async_mode,
            uploads,
            owner_authenticated,
            rf: None,
        },
    )
    .await
}

/// Why `select_turn_target` refused the request. Each arm is one error type
/// the helper already returned; `IntoResponse` delegates so status, code and
/// body bytes stay unchanged.
#[derive(Debug)]
enum TargetRefusal {
    Selector(TargetError),
    Forbidden((StatusCode, Json<serde_json::Value>)),
    Unavailable(TargetUnavailable),
    RegistryMissing,
}

impl From<TargetError> for TargetRefusal {
    fn from(error: TargetError) -> Self {
        Self::Selector(error)
    }
}

impl From<(StatusCode, Json<serde_json::Value>)> for TargetRefusal {
    fn from(error: (StatusCode, Json<serde_json::Value>)) -> Self {
        Self::Forbidden(error)
    }
}

impl From<TargetUnavailable> for TargetRefusal {
    fn from(error: TargetUnavailable) -> Self {
        Self::Unavailable(error)
    }
}

impl IntoResponse for TargetRefusal {
    fn into_response(self) -> Response {
        match self {
            Self::Selector(error) => error.into_response(),
            Self::Forbidden(payload) => payload.into_response(),
            Self::Unavailable(error) => error.into_response(),
            Self::RegistryMissing => json_error(StatusCode::INTERNAL_SERVER_ERROR, "Agent registry is unavailable"),
        }
    }
}

/// Resolve the request's `agent` against the pinned runtime generation,
/// before admission, input recording or provider access.
fn select_turn_target(
    state: &AppState,
    turn_runtime: std::sync::Arc<crate::gateway::TurnRuntimeGeneration>,
    principal: &super::ApiPrincipal,
    agent: AgentField,
    owner_authenticated: bool,
) -> Result<SelectedTurn, TargetRefusal> {
    let target = TurnTarget::from_field(agent)?;
    if matches!(target, TurnTarget::Main) {
        return Ok(SelectedTurn {
            runtime: turn_runtime,
            targeted: None,
            incarnation: None,
        });
    }
    if !owner_authenticated {
        // A caller who may not post messages learns nothing about which agents exist.
        super::authorize_resource_mutation(state, "gateway_api:sessions:message", ResourceRiskLevel::Low)?;
    }
    let Some(plan) = resolve_target(&turn_runtime.config_generation.effective, &target)? else {
        return Ok(SelectedTurn {
            runtime: turn_runtime,
            targeted: None,
            incarnation: None,
        });
    };
    if !state.mem.supports_agent_scope() {
        return Err(TargetRefusal::Selector(TargetError::BackendUnsupported));
    }
    let (runtime, turn) = prepare_target(&turn_runtime, &plan)?;
    let registry = state.mem.agent_registry().ok_or(TargetRefusal::RegistryMissing)?;
    let incarnation = resolve_incarnation(state, registry, &turn_runtime, &principal.owner_id, &plan)?;
    Ok(SelectedTurn {
        runtime,
        targeted: Some(turn),
        incarnation: Some(incarnation),
    })
}

#[cfg_attr(
    test,
    expect(
        clippy::large_enum_variant,
        reason = "Test-only source variant must not add indirection to production admission"
    )
)]
enum ConsoleTurnSource {
    #[cfg(test)]
    Message(crate::gateway::turn_source::GatewayTurnProof),
    Admitted {
        proof: crate::gateway::turn_source::GatewayTurnProof,
        guard: crate::gateway::turn_source::GatewayAdmissionGuard,
        owner_authenticated: bool,
    },
}

struct ConsolePendingInput {
    state: AppState,
    runtime: std::sync::Arc<crate::gateway::TurnRuntimeGeneration>,
    principal: super::ApiPrincipal,
    envelope: RuntimeEnvelope,
    ticket: crate::runtime::turn_cleanup::CleanupTicket,
    /// Set only by the mailbox wake: the input anchors the run and is never a history row.
    continuation: bool,
}

impl ConsolePendingInput {
    async fn record(
        self,
        message: String,
        rf_admission: Option<crate::memory::RfAdmission>,
    ) -> anyhow::Result<(
        crate::gateway::turn_source::GatewayTurnProof,
        crate::gateway::turn_source::GatewayAdmissionGuard,
    )> {
        let run = self
            .envelope
            .run_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Console input run is missing"))?;
        let key = format!("console:{run}:input");
        let recording = self.runtime.config_generation.effective.memory.event_recording_config();
        let fabric = MemoryFabric::new(self.state.mem.clone(), self.envelope.workspace_id.clone())
            .with_event_recording(recording)
            .with_ingress_recovery(&self.runtime.config_generation.effective, &self.ticket)
            .await?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut ticket = Some(self.ticket);
            let mut admission = None;
            let operation = async {
                let event = match rf_admission {
                    Some(rf_admission) => {
                        fabric
                            .record_admitted_user_message(
                                self.envelope.message_scope(),
                                message.clone(),
                                Some(key.clone()),
                                rf_admission,
                            )
                            .await?
                    }
                    None => {
                        fabric
                            .record_inbound_user_message(
                                self.envelope.message_scope(),
                                message.clone(),
                                Some(key.clone()),
                                None,
                            )
                            .await?
                    }
                };
                anyhow::ensure!(
                    event.id <= 0
                        || (event.idempotency_key.as_deref() == Some(key.as_str())
                            && event.workspace_id == self.envelope.workspace_id
                            && event.owner_id.as_deref() == Some(self.principal.owner_id.as_str())),
                    "Console input identity changed during persistence"
                );
                let event_id = if event.id > 0 { event.event_id.as_str() } else { "" };
                let proof = crate::gateway::turn_source::GatewaySourceReceipt::console_proof(
                    self.state.mem.as_ref(),
                    &self.runtime,
                    &self.principal,
                    &self.envelope,
                    &message,
                    event_id,
                )
                .await?;
                anyhow::ensure!(
                    event.id <= 0
                        || proof.envelope().source_message_event_id.as_deref() == Some(event.event_id.as_str()),
                    "Console durable receipt lost its original input"
                );
                admission = Some(crate::gateway::turn_source::GatewayAdmissionGuard::new(
                    &proof,
                    fabric.clone(),
                    ticket
                        .take()
                        .ok_or_else(|| anyhow::anyhow!("Console input cleanup ticket missing"))?,
                    None,
                ));
                if recording.enabled && recording.record_user_messages {
                    if console_recordable(&self.runtime.config_generation.effective, self.state.mem.as_ref()) {
                        self.state.mem.project_console_message(&event).await?;
                    } else if !self.continuation {
                        self.state
                            .mem
                            .append_conversation_turn(
                                &self.envelope.session_key,
                                self.envelope.channel.as_deref().unwrap_or("console"),
                                self.envelope.sender.as_deref().unwrap_or("console-user"),
                                "user",
                                &message,
                                None,
                                Some(&event.event_id),
                                Some(&self.envelope.resolved_owner_id()),
                                self.envelope.agent_partition.as_ref(),
                            )
                            .await?;
                    }
                }
                Ok::<_, anyhow::Error>(proof)
            };
            let result = tokio::time::timeout(std::time::Duration::from_secs(15), operation)
                .await
                .map_err(|_| anyhow::anyhow!("Console input persistence timed out"))
                .and_then(std::convert::identity);
            let result = match result {
                Ok(proof) => admission
                    .take()
                    .map(|guard| (proof, guard))
                    .ok_or_else(|| anyhow::anyhow!("Console input owner did not issue a guard")),
                // An rf admission refused inside its transaction rolled back:
                // nothing was written, so no write is left unconfirmed.
                Err(error) if crate::memory::RfAdmissionRefused::of(&error).is_some() => {
                    if let Some(ticket) = ticket.take() {
                        ticket.finish(true);
                    }
                    Err(error)
                }
                Err(_error) if fabric.ingress_already_accepted() => {
                    if let Some(ticket) = ticket.take() {
                        ticket.finish(true);
                    }
                    Err(crate::runtime::ingress_recovery::AlreadyAccepted.into())
                }
                Err(error) => {
                    if admission.is_none() {
                        let recovery = async {
                            let event = self
                                .state
                                .mem
                                .find_owned_message_event_by_idempotency_key(
                                    &self.envelope.workspace_id,
                                    &self.principal.owner_id,
                                    &key,
                                )
                                .await?
                                .ok_or_else(|| anyhow::anyhow!("Console input commit is unconfirmed"))?;
                            anyhow::ensure!(
                                event.id > 0
                                    && !event.event_id.is_empty()
                                    && event.idempotency_key.as_deref() == Some(key.as_str())
                                    && event.workspace_id == self.envelope.workspace_id
                                    && event.owner_id.as_deref() == Some(self.principal.owner_id.as_str()),
                                "Console recovery input identity changed"
                            );
                            let proof = crate::gateway::turn_source::GatewaySourceReceipt::console_proof(
                                self.state.mem.as_ref(),
                                &self.runtime,
                                &self.principal,
                                &self.envelope,
                                &message,
                                &event.event_id,
                            )
                            .await?;
                            anyhow::ensure!(
                                proof.envelope().source_message_event_id.as_deref() == Some(event.event_id.as_str()),
                                "Console recovery input is not durable"
                            );
                            Ok::<_, anyhow::Error>(proof)
                        };
                        if let Ok(Ok(proof)) = tokio::time::timeout(std::time::Duration::from_secs(15), recovery).await
                            && let Some(ticket) = ticket.take()
                        {
                            admission = Some(crate::gateway::turn_source::GatewayAdmissionGuard::new(
                                &proof, fabric, ticket, None,
                            ));
                        }
                    }
                    if let Some(mut guard) = admission {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(15),
                            guard.finish_failure(sender.is_closed(), "Console input admission failed"),
                        )
                        .await;
                    } else if let Some(ticket) = ticket {
                        ticket.finish(false);
                    }
                    Err(error)
                }
            };
            // A disconnected HTTP request drops the guard carried by this
            // response, retaining cleanup ownership even before job launch.
            let _ = sender.send(result);
        });
        receiver
            .await
            .map_err(|_| anyhow::anyhow!("Console input owner ended without a receipt"))?
    }
}

/// What one console submission carries into admission. `uploads` rides along
/// so the files are removed unless the input or job is accepted.
struct ConsoleSubmission {
    selected: SelectedTurn,
    message: String,
    async_mode: bool,
    uploads: UploadGuard,
    /// [`super::owner_authenticated`]; when false, Main's level gates the
    /// message and the saved reply as before A4, and the turn is
    /// owner-isolated.
    owner_authenticated: bool,
    /// Set for an rf message turn: its (session, agent) slot, held until its
    /// job ends, its run and job ids and its attempt row; it also selects the
    /// `{run_id, job_id}` answer.
    rf: Option<rf_replay::RfTurn>,
}

async fn submit_console_message(
    state: AppState,
    session_id: String,
    api_principal: super::ApiPrincipal,
    submission: ConsoleSubmission,
) -> Response {
    let ConsoleSubmission {
        selected,
        message,
        async_mode,
        uploads,
        owner_authenticated,
        rf,
    } = submission;
    let SelectedTurn {
        runtime: turn_runtime,
        targeted: target,
        incarnation,
    } = selected;
    tracing::debug!(
        agent_incarnation = incarnation.as_deref(),
        "console turn target resolved"
    );
    let Some(session) = (match state.mem.get_conversation_session(&session_id).await {
        Ok(session) => session,
        Err(error) => {
            tracing::error!("Failed to read session metadata from DB: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to load session" })),
            )
                .into_response();
        }
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Session not found" })),
        )
            .into_response();
    };
    // Q10: the only place a Console turn's isolation is decided. A paired
    // bearer is the install owner and gets the terminal's tool rules at the
    // turn's own level; any other caller (pairing off) stays owner-isolated.
    // A controlled H2 launch binds only isolated Gateway sources, so it keeps
    // isolation.
    let full_target = target
        .as_ref()
        .is_some_and(|target| target.autonomy == crate::security::AutonomyLevel::Full);
    let owner_isolated = (!owner_authenticated && !full_target) || turn_runtime.publication_slot.controlled.is_some();
    let run_id = rf
        .as_ref()
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), |rf| rf.run_id.clone());
    let runtime_envelope = console_runtime_envelope_for_workspace(
        turn_runtime
            .config_generation
            .effective
            .workspace_dir
            .to_string_lossy()
            .to_string(),
        &session_id,
        &session.channel,
        &session.sender,
    )
    .with_owner_id(api_principal.owner_id.clone());
    let runtime_envelope = if owner_isolated {
        runtime_envelope.with_owner_isolation()
    } else {
        runtime_envelope
    }
    .with_run_id(run_id.clone())
    .with_config_generation(&turn_runtime.config_generation);

    if let Err(error) = recover_console_history(&state, &turn_runtime, &api_principal, &session_id).await {
        return history_recovery_failed(&error);
    }
    let mut principal = operator_history_principal(&api_principal, &runtime_envelope);
    let partition = match history_partition(
        &runtime_envelope,
        &api_principal.owner_id,
        target.as_ref(),
        incarnation.as_deref(),
    ) {
        Ok(partition) => partition,
        Err(response) => return *response,
    };
    principal.agent_partition = Some(partition.clone());
    // MA-01b3: the turn's writers stamp from the envelope; reads keep the principal.
    let runtime_envelope = runtime_envelope.with_agent_partition(partition);
    let turns = match state
        .mem
        .list_console_conversation_turns(&principal, &session_id, MAX_PAGE_LIMIT, 0)
        .await
    {
        Ok(turns) => turns,
        Err(error) => {
            tracing::error!("Failed to load conversation history from DB: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to load session history" })),
            )
                .into_response();
        }
    };

    if !owner_authenticated
        && let Err(error) =
            super::authorize_resource_mutation(&state, "gateway_api:sessions:message", ResourceRiskLevel::Low)
    {
        return error.into_response();
    }

    let input_cancellation = tokio_util::sync::CancellationToken::new();
    let ticket = match turn_runtime.publication_slot.cleanup.register(input_cancellation) {
        Ok(ticket) => ticket,
        Err(error) => {
            warn_cleanup_refused(
                &turn_runtime.publication_slot.cleanup,
                "Console input admission",
                &error,
            );
            return json_error(StatusCode::SERVICE_UNAVAILABLE, "Console input admission is closed");
        }
    };
    let mailbox_principal = api_principal.clone();
    let mailbox_envelope = runtime_envelope.clone();
    let pending = ConsolePendingInput {
        state: state.clone(),
        runtime: std::sync::Arc::clone(&turn_runtime),
        principal: api_principal,
        envelope: runtime_envelope,
        ticket,
        continuation: false,
    };
    let admission = rf.as_ref().map(|rf| rf.admission.clone());
    let (proof, mut guard) = match pending.record(message.clone(), admission).await {
        Ok(admission) => admission,
        Err(error) if crate::memory::RfAdmissionRefused::of(&error).is_some() => {
            return rf_replay::refused_admission(&state, rf, &error).await;
        }
        Err(error) if error.is::<crate::runtime::ingress_recovery::AlreadyAccepted>() => {
            return json_error(
                StatusCode::CONFLICT,
                "Input already accepted; execution was not repeated and outcome remains unconfirmed",
            );
        }
        Err(error) if error.is::<crate::memory::AgentScopeRefused>() => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": { "code": "agent_scope_refused" } })),
            )
                .into_response();
        }
        Err(error) => {
            // The chain can quote stored input (a recovery capsule decode error
            // does), so only its content-free category and the session id are logged.
            tracing::warn!(
                error_kind = console_error_kind(&error),
                session_id = %session_id,
                "Console input persistence failed before admission"
            );
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Console input persistence is unconfirmed",
            );
        }
    };
    if let Err(error) = mailbox::register(&state, mailbox_envelope, mailbox_principal) {
        tracing::warn!(error=%crate::agent::diagnostics::error_class(&error),"Console mailbox registration refused");
    }

    // Establish monitor ownership synchronously, before constructing work that
    // cancellation or registry abortion can drop without ever polling.
    let (cleanup_sender, cleanup_receiver) = tokio::sync::oneshot::channel();
    if let Err(error) = guard.attach_acknowledgement(cleanup_sender) {
        tracing::warn!(
            error_kind = console_error_kind(&error),
            session_id = %session_id,
            "Console input cleanup ownership could not be attached"
        );
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Console input cleanup ownership is unconfirmed",
        );
    }
    let source = ConsoleTurnSource::Admitted {
        proof,
        guard,
        owner_authenticated,
    };

    // Everything up to here is bounded bookkeeping. What follows is the agent
    // turn itself, which has no upper bound — tool chains, sub-agents, and
    // compaction can legitimately run for hours, and PRX no longer cuts them off
    // on a clock. Awaiting it inside this handler would tie the turn's lifetime
    // to the HTTP connection, so a closed tab or a proxy hang-up would destroy a
    // turn mid-flight, after its side effects had already begun. It therefore
    // runs as a detached job: visible in `prx tasks list`, killable by id, and
    // still there when the client comes back.
    let settle_state = rf.as_ref().map(|_| state.clone());
    let work = run_console_message_job(turn_runtime, state, message, turns, source, target);
    let label = format!("gateway:session_message:{session_id}");
    if let Some(rf) = rf {
        let accepted = rf.accepted();
        let (slot_key, job_id) = (rf.slot.key().to_owned(), rf.job_id);
        let slot = rf.slot;
        let in_flight = rf.in_flight;
        let key = rf.admission.key;
        // The slot is released exactly when the job's work ends or is dropped.
        let job = jobs::submit_run_with_cleanup(
            jobs::KIND_SESSION_MESSAGE,
            label,
            async move {
                let _slot = slot;
                work.await
            },
            Some(cleanup_receiver),
            Some((std::sync::Arc::from(run_id.as_str()), job_id)),
        );
        // AC-1b2b: the run stays in flight until the job's terminal phase;
        // a delete that arrived meanwhile is then applied to its rows.
        if let Some(settle_state) = settle_state {
            tokio::spawn(async move {
                job.finished().await;
                drop(in_flight);
                rf_delete::settle_deleted(&settle_state, &key.workspace_id, &key.owner_id, &key.session_key).await;
            });
        }
        if rf_message::attach_job(&slot_key, job_id)
            && let Some(work_id) = jobs::work_id_of(job_id)
        {
            // A cancel by run id arrived while the input was being admitted.
            tokio::spawn(crate::runtime::registry::kill(work_id, true));
        }
        return accepted;
    }
    let job = jobs::submit_with_cleanup(jobs::KIND_SESSION_MESSAGE, label, work, Some(cleanup_receiver));
    // The job owns the turn now; hand the uploaded files over with it.
    uploads.release();

    if async_mode {
        return (StatusCode::ACCEPTED, Json(job.accepted_body())).into_response();
    }
    job.wait().await.into_response()
}

/// The unbounded half of `POST /sessions/{id}/message`, run as a job.
///
/// Returns the exact `(status, body)` the synchronous handler used to produce,
/// so waiting clients see no contract change and polling clients see the same
/// payload under `result`.
fn run_console_message_job(
    turn_runtime: std::sync::Arc<crate::gateway::TurnRuntimeGeneration>,
    state: AppState,
    message: String,
    turns: Vec<crate::memory::ConversationTurn>,
    source: ConsoleTurnSource,
    target: Option<TargetedTurn>,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<jobs::JobOutput, String>> + Send>> {
    // Keep the owned Console work out of the generic job submitter's stack.
    // Construction does not poll it; the same input guard retains its sender.
    Box::pin(run_console_message_job_work(
        turn_runtime,
        state,
        message,
        turns,
        source,
        target,
    ))
}

async fn run_console_message_job_work(
    turn_runtime: std::sync::Arc<crate::gateway::TurnRuntimeGeneration>,
    state: AppState,
    message: String,
    turns: Vec<crate::memory::ConversationTurn>,
    source: ConsoleTurnSource,
    target: Option<TargetedTurn>,
) -> Result<jobs::JobOutput, String> {
    let (source_proof, input_guard, owner_authenticated) = match source {
        #[cfg(test)]
        ConsoleTurnSource::Message(proof) => (proof, None, false),
        ConsoleTurnSource::Admitted {
            proof,
            guard,
            owner_authenticated,
        } => (proof, Some(guard), owner_authenticated),
    };
    let runtime_envelope = source_proof.envelope().clone();
    let runtime =
        crate::messaging::runtime::MessagingRuntime::for_config(&turn_runtime.config_generation.effective).ok();
    let address = runtime.as_ref().map(|runtime| {
        runtime.address(
            &runtime_envelope.resolved_owner_id(),
            &runtime_envelope.session_key,
            runtime_envelope.agent_id.as_deref().unwrap_or("main"),
        )
    });
    let _mailbox_gate = if let Some(address) = &address {
        Some(crate::messaging::receiver::gate(address).lock_owned().await)
    } else {
        None
    };
    let cancellation =
        jobs::current_cancellation().map_or_else(tokio_util::sync::CancellationToken::new, |token| token.child_token());
    let _cancel_guard = cancellation.clone().drop_guard();
    let config = &turn_runtime.config_generation.effective;
    let cleanup_ticket = turn_runtime
        .publication_slot
        .cleanup
        .register(cancellation.clone())
        .map_err(|_| "Console turn admission is closed".to_owned())?;
    // Register the job's cancellation/acknowledgement owner before releasing
    // the HTTP input ticket. This transfer has no await or unowned interval.
    let inherited_fabric = input_guard.as_ref().map(|guard| guard.ingress_fabric());
    let admission_acknowledgement = if let Some(input_guard) = input_guard {
        let (input_ticket, acknowledgement) = input_guard
            .into_terminal_parts()
            .map_err(|_| "Console input ownership transfer failed".to_owned())?;
        input_ticket.finish(true);
        acknowledgement
    } else {
        jobs::register_terminal_cleanup()
    };
    let admission_fabric = inherited_fabric.unwrap_or_else(|| {
        MemoryFabric::new(state.mem.clone(), runtime_envelope.workspace_id.clone())
            .with_event_recording(config.memory.event_recording_config())
    });
    let mut admission_guard = crate::gateway::turn_source::GatewayAdmissionGuard::new(
        &source_proof,
        admission_fabric,
        cleanup_ticket,
        admission_acknowledgement,
    );
    // Prediction transport accounting and later work retain the same owner.
    // Register its monitor acknowledgement before the first admission await.
    let child_cleanup = match crate::runtime::turn_cleanup::CleanupScope::new(
        Some(&turn_runtime.publication_slot.cleanup),
        cancellation.child_token(),
    ) {
        Ok(scope) => scope.with_acknowledgement(jobs::register_terminal_cleanup()),
        Err(_) => {
            let error = "Console child cleanup admission is closed";
            admission_guard
                .finish_failure(cancellation.is_cancelled(), error)
                .await
                .map_err(|_| "Console admission failure persistence is unconfirmed".to_owned())?;
            return Err(error.to_owned());
        }
    };
    let decision = {
        let proof = &source_proof;
        let admission = child_cleanup
            .scope_admission(Box::pin(async {
                let admitted: Result<_, String> = async {
                    proof
                        .verify_runtime_input(&turn_runtime, &message)
                        .map_err(|error| crate::providers::sanitize_api_error(&error.to_string()))?;
                    let service = turn_runtime
                        .publication_slot
                        .publish(&turn_runtime, &state)
                        .await
                        .map_err(|_| "Console decision publication failed".to_owned())?;
                    let kind = crate::runtime::turn_context::AgentTurnKind::SingleShot;
                    let context = service
                        .open_issued_turn_context(proof, kind, &cancellation)
                        .await
                        .map_err(|error| crate::providers::sanitize_api_error(&error.to_string()))?;
                    let advice = service.advisory().analyze_turn(context.runtime(), &cancellation).await;
                    Ok::<_, String>((service, context, advice))
                }
                .await;
                admitted.map_err(anyhow::Error::msg)
            }))
            .await;
        let (service, context, advice) = match admission {
            Ok(admitted) => admitted,
            Err(error) => {
                // No future is still polling admission. Close/drain its owned
                // attempts before acknowledging the failed input terminal.
                let admission_error = crate::providers::sanitize_api_error(&format!("{error:#}"));
                let drained = child_cleanup.run(Box::pin(async { Err::<(), _>(error) })).await;
                let error = match drained {
                    Err(error) if error.is::<crate::runtime::turn_cleanup::CleanupIncomplete>() => format!(
                        "{admission_error}; Console admission cleanup is unconfirmed: {}",
                        crate::providers::sanitize_api_error(&format!("{error:#}")),
                    ),
                    _ => admission_error,
                };
                if admission_guard
                    .finish_failure(cancellation.is_cancelled(), &error)
                    .await
                    .is_err()
                {
                    return Err(format!("{error}; Console admission failure persistence is unconfirmed"));
                }
                return Err(error);
            }
        };
        let provider_trace = None;
        ConsoleDecisionTurn {
            service: std::sync::Arc::clone(service),
            provider_trace,
            context,
            advice,
            proof,
            cancellation: cancellation.clone(),
        }
    };
    let (cleanup_ticket, admission_acknowledgement) = admission_guard
        .into_terminal_parts()
        .map_err(|error| error.to_string())?;
    let terminal_guard = crate::gateway::turn_source::publication_slot::GatewayTerminalGuard::new(
        cleanup_ticket,
        std::sync::Arc::clone(&decision.service),
        std::sync::Arc::clone(&decision.context),
        MemoryFabric::new(state.mem.clone(), runtime_envelope.workspace_id.clone()),
        std::sync::Arc::clone(config),
        runtime_envelope.clone(),
        admission_acknowledgement,
    )
    .map_err(|error| error.to_string())?;
    let terminal_guard = if let Some((trace, route)) = &decision.provider_trace {
        terminal_guard.with_provider_trace(std::sync::Arc::clone(trace), route.as_ref().clone())
    } else {
        terminal_guard
    };
    let work = async {
        let turn = match run_console_runtime_turn(
            std::sync::Arc::clone(&turn_runtime),
            &state,
            &message,
            turns,
            &decision,
            target.as_ref(),
        )
        .await
        {
            Ok(turn) => turn,
            Err(error) => {
                // A stable HTTP error response still represents a failed turn.
                // Finalize it before returning a successful job transport result.
                {
                    terminal_guard
                        .finish_failure(&error, cancellation.is_cancelled())
                        .await
                        .map_err(|_| {
                            format!(
                                "Console terminal cleanup persistence is unconfirmed ({})",
                                terminal_guard.failure_reason()
                            )
                        })?;
                }
                if let Some(tools) = crate::providers::tool_schema_refused_tools(&error) {
                    // A configuration problem, not a failed job: the caller
                    // gets a stable code with every refused tool, and the log
                    // keeps the whole refusal, unclipped.
                    tracing::error!(
                        error = %crate::security::redaction::redact_secrets(&format!("{error:#}")),
                        "Console turn refused before transport: the provider cannot accept some offered tool schemas"
                    );
                    return Ok(jobs::JobOutput::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        serde_json::json!({
                            "error": "tool_schema_refused",
                            "tools": tools,
                            "message": "The configured provider cannot accept some of the tool schemas this turn \
                                        offered, so nothing was sent. The gateway log names the configuration \
                                        to change.",
                        }),
                    ));
                }
                return Err(crate::providers::sanitize_api_error(&error.to_string()));
            }
        };
        let reply = turn.reply.clone();

        if !owner_authenticated
            && let Err((status, body)) = crate::gateway::authorize_gateway_config_resource_mutation(
                config,
                "gateway_api:sessions:assistant_message",
                ResourceRiskLevel::Low,
            )
        {
            {
                terminal_guard
                    .finish_error(&anyhow::anyhow!("Console assistant persistence authorization denied"))
                    .await
                    .map_err(|_| {
                        format!(
                            "Console terminal cleanup persistence is unconfirmed ({})",
                            terminal_guard.failure_reason()
                        )
                    })?;
            }
            return Ok(jobs::JobOutput::new(status, body.0));
        }

        let provider_outcome = crate::agent::terminal::provider_outcome_from_trace(
            &turn.route_decision,
            turn.provider_started_at,
            turn.trace,
        );
        let terminal_id = turn
            .envelope
            .run_id
            .clone()
            .ok_or_else(|| "Console admitted run missing".to_owned())?;
        let terminal_fabric = MemoryFabric::new(state.mem.clone(), turn.envelope.workspace_id.clone());
        let config = &turn_runtime.config_generation.effective;
        let terminal_binding = decision
            .service
            .terminal_binding(&decision.context)
            .map_err(|error| error.to_string())?;
        let receipt = match crate::gateway::turn_source::publication_slot::commit_terminal(
            Some(&terminal_guard),
            &terminal_fabric,
            crate::agent::terminal::TurnTerminalDraft {
                binding: Some(terminal_binding),
                terminal_id,
                scope: turn.envelope.message_scope(),
                status: crate::agent::terminal::TurnTerminalStatus::Completed,
                history: Some(crate::agent::terminal::TurnHistoryProjection {
                    assistant_content: reply.clone(),
                    history_commit_len: turn.history_commit_len,
                }),
                history_scope: None,
                provider_outcome: Some(provider_outcome),
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: if turn.awaiting_approval {
                        "console turn awaiting human approval"
                    } else {
                        "console turn completed"
                    }
                    .to_string(),
                    started_at: turn.provider_started_at,
                    finished_at: chrono::Utc::now(),
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
            },
            config,
        )
        .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                let reason: crate::runtime::turn_terminal::TerminalPersistReason = terminal_guard.failure_reason();
                tracing::warn!(
                    reason = %reason,
                    error = %crate::agent::diagnostics::error_class(&error),
                    "Failed to commit shared console terminal event"
                );
                return Err(format!("Console terminal persistence is unconfirmed ({reason})"));
            }
        };

        if console_recordable(config, state.mem.as_ref()) {
            // A recorded console turn's transcript is the projection of its
            // committed, redacted assistant event, exactly as its input was
            // projected at admission.
            if let Some(assistant) = receipt.assistant_event.as_ref() {
                if let Err(error) = state.mem.project_console_message(assistant).await {
                    tracing::error!("Failed to project assistant session turn: {error}");
                    return Err("Failed to persist assistant response".to_string());
                }
            }
        } else {
            let record_event = matches!(
                turn.envelope.agent_partition.as_ref().map(|partition| &partition.key),
                Some(crate::memory::PartitionKey::Named { .. })
            );
            if let Err(error) =
                append_console_turn(&state, &turn.envelope, "assistant", &reply, record_event, false).await
            {
                tracing::error!("Failed to persist assistant session turn: {error}");
                return Err("Failed to persist assistant response".to_string());
            }
        }

        let body = if turn.awaiting_approval {
            serde_json::json!({
                "status": "awaiting_approval",
                "reply": reply,
                "decision_id": decision.context.decision_id().to_string(),
            })
        } else {
            serde_json::to_value(SendMessageResponse {
                status: "ok".to_string(),
                reply,
            })
            .map_err(|error| format!("Failed to encode session reply: {error}"))?
        };
        Ok(jobs::JobOutput::new(StatusCode::OK, body))
    };
    let result = child_cleanup
        .run(Box::pin(async {
            let result: Result<jobs::JobOutput, String> = work.await;
            result.map_err(anyhow::Error::msg)
        }))
        .await
        .map_err(|error| crate::providers::sanitize_api_error(&error.to_string()));
    if let Err(error) = &result {
        terminal_guard
            .finish_failure(&anyhow::anyhow!(error.clone()), cancellation.is_cancelled())
            .await
            .map_err(|_| {
                format!(
                    "Console terminal cleanup persistence is unconfirmed ({})",
                    terminal_guard.failure_reason()
                )
            })?;
    }
    result
}

pub async fn get_session_media(
    State(state): State<AppState>,
    Query(query): Query<SessionMediaQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let token = extract_resource_auth_token(&headers);
    if state.pairing.require_pairing() && !state.pairing.is_authenticated(&token) {
        return json_error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }

    let requested_path = query.path.trim();
    if requested_path.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "path is required");
    }

    let uploads_root = {
        let config = state.config.load_full();
        config.workspace_dir.join(UPLOADS_DIR_NAME)
    };

    let uploads_root = match fs::canonicalize(&uploads_root).await {
        Ok(path) => path,
        Err(_) => return json_error(StatusCode::NOT_FOUND, "Upload not found"),
    };

    let raw_path = PathBuf::from(requested_path);
    if raw_path.is_absolute() {
        return json_error(StatusCode::BAD_REQUEST, "Absolute paths are not allowed");
    }
    if raw_path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return json_error(StatusCode::BAD_REQUEST, "Path traversal is not allowed");
    }
    let candidate_path = uploads_root.join(raw_path);

    let resolved_path = match fs::canonicalize(&candidate_path).await {
        Ok(path) => path,
        Err(_) => return json_error(StatusCode::NOT_FOUND, "Upload not found"),
    };

    if !resolved_path.starts_with(&uploads_root) {
        return json_error(StatusCode::FORBIDDEN, "Access denied");
    }

    let bytes = match fs::read(&resolved_path).await {
        Ok(bytes) => bytes,
        Err(_) => return json_error(StatusCode::NOT_FOUND, "Upload not found"),
    };

    let content_type = media_content_type_for_path(&resolved_path);
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
pub(crate) async fn drive_disabled_engine_console_entry(workspace: &std::path::Path) {
    tests::drive_disabled_engine_console_entry(workspace).await;
}

#[cfg(test)]
pub(crate) async fn drive_search_console_entry(config: crate::config::Config, input: &str) -> anyhow::Result<()> {
    tests::drive_search_console_entry(config, input).await
}
