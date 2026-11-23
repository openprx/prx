/// POST /webhook — main webhook endpoint
async fn handle_webhook(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let async_mode = jobs::wants_async(uri.query(), &headers);
    let bearer_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    let webhook_token_header = headers
        .get("X-Webhook-Token")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let webhook_signature_header = headers
        .get("X-Webhook-Signature")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let rate_key = client_key_from_request(Some(peer_addr), &headers, state.trust_forwarded_headers);
    if !state.rate_limiter.allow_webhook(&rate_key) {
        tracing::warn!("/webhook rate limit exceeded");
        let err = serde_json::json!({
            "error": "Too many webhook requests. Please retry later.",
            "retry_after": RATE_LIMIT_WINDOW_SECS,
        });
        return (StatusCode::TOO_MANY_REQUESTS, Json(err));
    }

    // ── Bearer token auth (pairing) ──
    if state.pairing.require_pairing() {
        if !state.pairing.is_authenticated(bearer_token) {
            tracing::warn!("Webhook: rejected — not paired / invalid bearer token");
            let err = serde_json::json!({
                "error": "Unauthorized — pair first via POST /pair, then send Authorization: Bearer <token>"
            });
            return (StatusCode::UNAUTHORIZED, Json(err));
        }
    }

    // ── Standalone webhook token auth (optional, additional layer) ──
    if let Some(ref token_hash) = state.webhook_token_hash {
        let header_hash = webhook_token_header.map(hash_webhook_secret);
        match header_hash {
            Some(val) if constant_time_eq(&val, token_hash.as_ref()) => {}
            _ => {
                tracing::warn!("Webhook: rejected request — invalid or missing X-Webhook-Token");
                let err = serde_json::json!({"error": "Unauthorized — invalid or missing X-Webhook-Token header"});
                return (StatusCode::UNAUTHORIZED, Json(err));
            }
        }
    }

    // ── Webhook HMAC auth (optional, additional layer) ──
    if let Some(ref signing_secret) = state.webhook_signing_secret {
        match webhook_signature_header {
            Some(signature) if verify_webhook_hmac_signature(signing_secret, &body, signature) => {}
            _ => {
                tracing::warn!("Webhook: rejected request — invalid or missing X-Webhook-Signature");
                let err = serde_json::json!({"error": "Unauthorized — invalid or missing X-Webhook-Signature header"});
                return (StatusCode::UNAUTHORIZED, Json(err));
            }
        }
    }

    // Additional credential-based limiter reduces distributed IP rotation bypass.
    let credential_rate_key = if !bearer_token.is_empty() {
        format!("bearer:{}", hash_webhook_secret(bearer_token))
    } else if let Some(token) = webhook_token_header {
        format!("token:{}", hash_webhook_secret(token))
    } else if let Some(signature) = webhook_signature_header {
        format!("signature:{}", hash_webhook_secret(signature))
    } else {
        "public".to_string()
    };
    // The authenticated credential this request presented, in two forms: the
    // idempotency scope (process-local; hashed again into every stored
    // idempotency key) and the owner credential of the webhook turn
    // (`GatewayFabricContext::webhook_owner_id`, R-MCPB F-2), whose
    // domain-separated digest is what reaches owner ids, session keys and logs
    // (R-MCPBfg N-4). Both follow the same precedence.
    let owner_credential = if state.pairing.require_pairing() {
        webhook_owner_credential("bearer", bearer_token)
    } else if let (Some(_), Some(token)) = (&state.webhook_token_hash, webhook_token_header) {
        webhook_owner_credential("token", token)
    } else if let Some(signing_secret) = state.webhook_signing_secret.as_deref() {
        webhook_owner_credential("signing-secret", signing_secret)
    } else {
        "public".to_string()
    };
    let idempotency_scope = if state.pairing.require_pairing() {
        format!("bearer:{}", hash_webhook_secret(bearer_token))
    } else if let (Some(_), Some(token)) = (&state.webhook_token_hash, webhook_token_header) {
        format!("token:{}", hash_webhook_secret(token))
    } else if let Some(signing_secret) = state.webhook_signing_secret.as_deref() {
        format!("signing-secret:{}", hash_webhook_secret(signing_secret))
    } else {
        "public".to_string()
    };
    if !state.rate_limiter.allow_webhook_credential(&credential_rate_key) {
        tracing::warn!("/webhook credential rate limit exceeded");
        let err = serde_json::json!({
            "error": "Too many webhook requests. Please retry later.",
            "retry_after": RATE_LIMIT_WINDOW_SECS,
        });
        return (StatusCode::TOO_MANY_REQUESTS, Json(err));
    }
    // Admission, before any idempotency claim is taken.
    if let Err(refusal) = state.admit_turn_runtime() {
        return turn_not_ready_plain(refusal);
    }

    // ── Parse body ──
    let webhook_body: WebhookBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("Webhook JSON parse error: {e}");
            let err = serde_json::json!({
                "error": "Invalid JSON body. Expected: {\"message\": \"...\"}"
            });
            return (StatusCode::BAD_REQUEST, Json(err));
        }
    };

    // ── Idempotency (optional) ──
    let request_idempotency_key = match idempotency_key_from_header(&headers, "X-Idempotency-Key") {
        Ok(key) => key,
        Err(message) => {
            let err = serde_json::json!({ "error": message });
            return (StatusCode::BAD_REQUEST, Json(err));
        }
    };
    let mut idempotency_digest = None;
    let mut idempotency_claim = None;
    if let Some(raw_key) = request_idempotency_key.as_deref() {
        if let Err(error) =
            authorize_gateway_resource_mutation(&state, "gateway:webhook:idempotency", ResourceRiskLevel::Low)
        {
            return error;
        }
        let digest = webhook_idempotency_digest(&idempotency_scope, raw_key);
        let fingerprint = webhook_request_fingerprint(&body);
        match state.idempotency_store.claim(digest.clone(), fingerprint) {
            IdempotencyClaimOutcome::Acquired(claim) => {
                idempotency_digest = Some(digest);
                idempotency_claim = Some(claim);
            }
            IdempotencyClaimOutcome::Processing => {
                let body = serde_json::json!({
                    "status": "processing",
                    "idempotent": true,
                    "message": "A request with this idempotency key is still processing"
                });
                return (StatusCode::CONFLICT, Json(body));
            }
            IdempotencyClaimOutcome::Replay(replay) => {
                return (StatusCode::OK, Json(replay.json_body()));
            }
            IdempotencyClaimOutcome::ReplayUnavailable {
                response_id,
                result_hash,
            } => {
                let body = replay_unavailable_body(response_id, &result_hash);
                return (StatusCode::CONFLICT, Json(body));
            }
            IdempotencyClaimOutcome::RequestConflict => {
                let body = serde_json::json!({
                    "status": "request_conflict",
                    "idempotent": true,
                    "message": "This idempotency key was used with a different request body"
                });
                return (StatusCode::CONFLICT, Json(body));
            }
            IdempotencyClaimOutcome::RetryUnavailable => {
                let body = serde_json::json!({
                    "status": "retry_unavailable",
                    "idempotent": true,
                    "message": "This failed request is not eligible for retry"
                });
                return (StatusCode::CONFLICT, Json(body));
            }
            IdempotencyClaimOutcome::AtCapacity => {
                let body = serde_json::json!({
                    "error": "Idempotency capacity is temporarily exhausted",
                    "retryable": true
                });
                return (StatusCode::SERVICE_UNAVAILABLE, Json(body));
            }
        }
    }

    // From here on the request may run for as long as the agent turn behind it
    // needs, so it becomes a detached job rather than staying inside this
    // handler's future. Two things follow from that. The turn survives a client
    // that hangs up — a webhook sender's read timeout no longer destroys work
    // that had already started writing memory and calling tools. And the
    // idempotency claim travels *with* the work instead of with the connection,
    // so a dropped connection can no longer release the claim underneath a run
    // that is still going.
    let job = jobs::submit(
        jobs::KIND_WEBHOOK,
        format!("gateway:webhook:{}", fabric_channel_label(&webhook_body)),
        run_webhook_job(
            state,
            webhook_body,
            owner_credential,
            idempotency_digest,
            idempotency_claim,
        ),
    );

    if async_mode {
        return (StatusCode::ACCEPTED, Json(job.accepted_body()));
    }
    job.wait().await.into_parts()
}

/// Label for the registry row of a webhook job, so `prx tasks list` shows which
/// delivery target the run belongs to rather than a bare route name.
fn fabric_channel_label(body: &WebhookBody) -> &str {
    body.reply_target.as_deref().unwrap_or("anonymous")
}

/// The unbounded half of `POST /webhook`, run as a job.
///
/// Owns the idempotency claim for the whole run: the claim is only resolved by
/// code that knows how the run ended, never by the request future going away.
fn run_webhook_job(
    state: AppState,
    webhook_body: WebhookBody,
    credential: String,
    idempotency_digest: Option<String>,
    idempotency_claim: Option<IdempotencyClaim>,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<jobs::JobOutput, String>> + Send>> {
    // Keep owned webhook work out of the generic job submitter's future.
    // Construction does not poll it; the same idempotency claim moves with it.
    Box::pin(run_webhook_job_work(
        state,
        webhook_body,
        credential,
        idempotency_digest,
        idempotency_claim,
    ))
}

async fn run_webhook_job_work(
    state: AppState,
    webhook_body: WebhookBody,
    credential: String,
    idempotency_digest: Option<String>,
    idempotency_claim: Option<IdempotencyClaim>,
) -> Result<jobs::JobOutput, String> {
    let mut idempotency_claim = idempotency_claim;
    let message = webhook_body.message.as_str();

    let autosave_key =
        if state.auto_save && should_autosave_gateway_message(webhook_body.reply_target.as_deref(), message) {
            if let Err((status, body)) =
                authorize_gateway_resource_mutation(&state, "gateway:webhook:autosave", ResourceRiskLevel::Low)
            {
                return Ok(jobs::JobOutput::new(status, body.0));
            }
            // First observable side effect of the attempt: past this point a
            // redelivery of the same key would duplicate it.
            if let Some(claim) = idempotency_claim.as_mut() {
                claim.mark_dispatched();
            }
            let key = webhook_memory_key(idempotency_digest.as_deref());
            Some(key)
        } else {
            None
        };

    let provider_label = state
        .config
        .load_full()
        .default_provider
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let model_label = state.model.clone();
    let started_at = Instant::now();

    state
        .observer
        .record_event(&crate::observability::ObserverEvent::AgentStart {
            provider: provider_label.clone(),
            model: model_label.clone(),
        });
    state
        .observer
        .record_event(&crate::observability::ObserverEvent::LlmRequest {
            provider: provider_label.clone(),
            model: model_label.clone(),
            messages_count: 1,
        });

    let fabric_ctx = GatewayFabricContext::generic_webhook(
        webhook_body.reply_target.as_deref(),
        &credential,
        idempotency_digest
            .as_deref()
            .map(|digest| format!("gateway:webhook:{digest}")),
    );
    // The turn records the inbound message and may call tools, so from here the
    // attempt is not replayable by re-execution — only by the stored result.
    if let Some(claim) = idempotency_claim.as_mut() {
        claim.mark_dispatched();
    }
    let mut stage = TurnFailureClass::Internal;
    match run_gateway_chat_with_multimodal(
        &state,
        &provider_label,
        message,
        &fabric_ctx,
        autosave_key.as_deref(),
        &mut stage,
    )
    .await
    {
        Ok(GatewayTurnResult {
            response,
            replay_unavailable,
        }) => {
            let response_id = match replay_unavailable.as_ref() {
                Some(reference) => {
                    Uuid::parse_str(&reference.terminal_id).map_err(|_| "invalid durable response id".to_owned())?
                }
                None => Uuid::new_v4(),
            };
            if let Some(claim) = idempotency_claim.take() {
                let completed = if let Some(reference) = replay_unavailable {
                    claim.succeed_unavailable(response_id, reference.result_hash)
                } else {
                    let result_hash = webhook_result_hash(&response, &model_label);
                    let replay = IdempotencyReplay {
                        response_id,
                        response: Arc::from(response.as_str()),
                        model: Arc::from(model_label.as_str()),
                    };
                    claim.succeed(replay, result_hash)
                };
                if !completed {
                    tracing::error!("Webhook idempotency ownership was lost before success commit");
                    let err = serde_json::json!({
                        "error": "Request completed but its idempotency result could not be committed"
                    });
                    return Ok(jobs::JobOutput::new(StatusCode::SERVICE_UNAVAILABLE, err));
                }
            }
            let duration = started_at.elapsed();
            state
                .observer
                .record_event(&crate::observability::ObserverEvent::LlmResponse {
                    provider: provider_label.clone(),
                    model: model_label.clone(),
                    duration,
                    success: true,
                    error_message: None,
                });
            state
                .observer
                .record_metric(&crate::observability::traits::ObserverMetric::RequestLatency(duration));
            state
                .observer
                .record_event(&crate::observability::ObserverEvent::AgentEnd {
                    provider: provider_label,
                    model: model_label,
                    duration,
                    tokens_used: None,
                    cost_usd: None,
                });

            let body = serde_json::json!({
                "response": response,
                "model": state.model,
                "response_id": response_id.to_string(),
            });
            Ok(jobs::JobOutput::new(StatusCode::OK, body))
        }
        Err(e) => {
            if let Some(unavailable) = e.downcast_ref::<crate::agent::terminal::oversized_reply::ReplayUnavailable>() {
                let response_id =
                    Uuid::parse_str(&unavailable.terminal_id).map_err(|_| "invalid durable response id".to_owned())?;
                if let Some(claim) = idempotency_claim.take() {
                    if !claim.succeed_unavailable(response_id, unavailable.result_hash.clone()) {
                        return Ok(jobs::JobOutput::new(
                            StatusCode::SERVICE_UNAVAILABLE,
                            serde_json::json!({"error":"Idempotency ownership was lost"}),
                        ));
                    }
                }
                return Ok(jobs::JobOutput::new(
                    StatusCode::CONFLICT,
                    replay_unavailable_body(response_id, &unavailable.result_hash),
                ));
            }
            if let Some(claim) = idempotency_claim.take() {
                let _ = claim.fail();
            }
            if e.is::<crate::runtime::ingress_recovery::AlreadyAccepted>() {
                return Ok(jobs::JobOutput::new(
                    StatusCode::CONFLICT,
                    serde_json::json!({"error":"input_already_accepted","status":"unconfirmed","message":"Execution was not repeated; the previously accepted input has an unconfirmed outcome."}),
                ));
            }
            let duration = started_at.elapsed();
            let sanitized = providers::sanitize_api_error(&e.to_string());

            state
                .observer
                .record_event(&crate::observability::ObserverEvent::LlmResponse {
                    provider: provider_label.clone(),
                    model: model_label.clone(),
                    duration,
                    success: false,
                    error_message: Some(sanitized.clone()),
                });
            state
                .observer
                .record_metric(&crate::observability::traits::ObserverMetric::RequestLatency(duration));
            state
                .observer
                .record_event(&crate::observability::ObserverEvent::Error {
                    component: "gateway".to_string(),
                    message: sanitized,
                });
            state
                .observer
                .record_event(&crate::observability::ObserverEvent::AgentEnd {
                    provider: provider_label,
                    model: model_label,
                    duration,
                    tokens_used: None,
                    cost_usd: None,
                });

            if let Some(tools) = providers::tool_schema_refused_tools(&e) {
                // A configuration problem, not a transient one: the whole
                // refusal (every tool, the configuration to add) is logged
                // unclipped, and the authenticated caller gets a stable code
                // with the refused tools.
                tracing::error!(
                    error = %crate::security::redaction::redact_secrets(&format!("{e:#}")),
                    "Webhook turn refused before transport: the provider cannot accept some offered tool schemas"
                );
                let err = serde_json::json!({
                    "error": "tool_schema_refused",
                    "tools": tools,
                    "message": "The configured provider cannot accept some of the tool schemas this request \
                                offered, so nothing was sent. The gateway log names the configuration to change.",
                });
                return Ok(jobs::JobOutput::new(StatusCode::INTERNAL_SERVER_ERROR, err));
            }
            // "LLM request failed" stands for a failed provider request only;
            // every other failure answers the closed class of its step, never
            // the error text.
            let reason = TurnFailureClass::classify(stage, &e);
            tracing::error!(
                reason = %reason,
                error = %crate::agent::diagnostics::error_class(&e),
                "Webhook turn failed"
            );
            let err = if reason == TurnFailureClass::Provider {
                serde_json::json!({"error": "LLM request failed"})
            } else {
                serde_json::json!({"error": "turn_failed", "reason": reason.code()})
            };
            Ok(jobs::JobOutput::new(StatusCode::INTERNAL_SERVER_ERROR, err))
        }
    }
}

/// Registry label for one accepted channel-webhook delivery.
///
/// Named the way every other registry row names a correspondent: the channel in
/// the clear, the chat only as [`crate::security::op_id::ref_for_channel_recipient`]'s
/// fingerprint. `reply_target` on this route is a platform address — a Telegram
/// chat id, a WhatsApp JID — and this label is published verbatim by
/// `GET /api/runtime/tasks`, so writing it out would leak through the operator
/// plane the very address the send path takes care to fingerprint. Sharing that
/// one function is also what keeps the digest here equal to the digest an
/// outbound send to the same chat produces, so the two rows stay correlatable.
///
/// MUTATION GUARD: interpolate `reply_target` directly and
/// `a_channel_webhook_job_label_fingerprints_the_chat` fails.
fn channel_webhook_job_label(channel_key: &str, first: Option<&crate::channels::traits::ChannelMessage>) -> String {
    first.map_or_else(
        || format!("gateway:{channel_key}"),
        |msg| {
            format!(
                "gateway:{channel_key}:{}",
                crate::security::op_id::ref_for_channel_recipient(channel_key, &msg.reply_target)
            )
        },
    )
}

/// Shared tail of the three platform-pushed channel webhooks.
///
/// These routes differ from `POST /webhook` in one decisive way: the sender is a
/// platform with a response deadline, not a client that chose to wait. Linq
/// documents "Response timeout: 10 seconds" and retries "up to 10 times over ~25
/// minutes"; Meta retries a WhatsApp delivery "with decreasing frequency until
/// the request succeeds, for up to 7 days". An agent turn has no upper bound and
/// routinely exceeds any of those, so awaiting the turn inside the handler — as
/// these three did — guaranteed the platform would give up, mark the delivery
/// failed, and redeliver, running the whole turn again for every retry.
///
/// The ack therefore reports that the work was *accepted*, not that it finished:
/// the turns run as a detached job, visible in `prx tasks list` and killable with
/// `prx tasks kill`, and the reply reaches the user through the channel's own
/// outbound API exactly as before. The delivery is claimed before submission, so
/// a retry that races the accepted run is answered as a duplicate instead of
/// starting a second copy of it.
/// The `503` a non-`/api` turn route answers while the turn runtime is not
/// ready, in those routes' own `{"error"}` shape.
fn turn_not_ready_plain(refusal: TurnNotReady) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": refusal.to_string() })),
    )
}

fn accept_channel_webhook_delivery(
    state: &AppState,
    channel: Arc<dyn Channel>,
    channel_key: &'static str,
    body: &Bytes,
    messages: Vec<crate::channels::traits::ChannelMessage>,
    memory_key: fn(&crate::channels::traits::ChannelMessage) -> String,
) -> (StatusCode, Json<serde_json::Value>) {
    // Admission, before the delivery is claimed: a refused delivery is
    // redelivered by the platform once the runtime is ready.
    if let Err(refusal) = state.admit_turn_runtime() {
        return turn_not_ready_plain(refusal);
    }
    let dedup_ttl = Duration::from_secs(state.config.load_full().gateway.channel_webhook_dedup_ttl_secs.max(1));
    let claim = match webhook_dedup::claim(webhook_dedup::delivery_key(channel_key, body), dedup_ttl) {
        webhook_dedup::DeliveryOutcome::Fresh(claim) => claim,
        webhook_dedup::DeliveryOutcome::Duplicate => {
            tracing::info!("{channel_key} webhook: redelivery of an already accepted payload — not run again");
            return (
                StatusCode::OK,
                Json(serde_json::json!({ "status": "duplicate", "idempotent": true })),
            );
        }
    };

    let label = channel_webhook_job_label(channel_key, messages.first());
    let job = jobs::submit(
        jobs::KIND_CHANNEL_WEBHOOK,
        label,
        run_channel_webhook_job(state.clone(), channel, channel_key, messages, memory_key, claim),
    );

    // Every platform on this route treats any 2xx as delivered; 200 is the one
    // status all three accept without special handling.
    (StatusCode::OK, Json(job.accepted_body()))
}

/// The unbounded half of a platform channel webhook, run as a job.
///
/// Owns the delivery claim for the whole run, so the claim is resolved by code
/// that knows whether the turn reached a side effect — never by the request
/// future going away.
fn run_channel_webhook_job(
    state: AppState,
    channel: Arc<dyn Channel>,
    channel_key: &'static str,
    messages: Vec<crate::channels::traits::ChannelMessage>,
    memory_key: fn(&crate::channels::traits::ChannelMessage) -> String,
    claim: webhook_dedup::DeliveryClaim,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<jobs::JobOutput, String>> + Send>> {
    // Keep owned channel work out of the generic job submitter's future.
    // Construction does not poll it; the same delivery claim moves with it.
    Box::pin(run_channel_webhook_job_work(
        state,
        channel,
        channel_key,
        messages,
        memory_key,
        claim,
    ))
}

async fn run_channel_webhook_job_work(
    state: AppState,
    channel: Arc<dyn Channel>,
    channel_key: &'static str,
    messages: Vec<crate::channels::traits::ChannelMessage>,
    memory_key: fn(&crate::channels::traits::ChannelMessage) -> String,
    mut claim: webhook_dedup::DeliveryClaim,
) -> Result<jobs::JobOutput, String> {
    let provider_label = state
        .config
        .load_full()
        .default_provider
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    for msg in &messages {
        tracing::info!(
            "{channel_key} message from {}: {}",
            msg.sender,
            truncate_with_ellipsis(&msg.content, 50)
        );

        // Auto-save to memory
        let autosave_key = if state.auto_save && should_autosave_gateway_message(Some(&msg.reply_target), &msg.content)
        {
            if let Err((status, body)) = authorize_gateway_resource_mutation(
                &state,
                &gateway_channel_webhook_operation(channel_key, "autosave"),
                ResourceRiskLevel::Low,
            ) {
                return Ok(jobs::JobOutput::new(status, body.0));
            }
            // First observable side effect: past this point a redelivery would
            // duplicate it, so the claim must outlive the run.
            claim.mark_dispatched();
            let key = memory_key(msg);
            Some(key)
        } else {
            None
        };

        let fabric_ctx = GatewayFabricContext::channel_message(msg);
        // The turn records the inbound message and may call tools, so from here
        // the delivery is not replayable by re-execution.
        claim.mark_dispatched();
        let mut stage = TurnFailureClass::Internal;
        match run_gateway_chat_with_multimodal(
            &state,
            &provider_label,
            &msg.content,
            &fabric_ctx,
            autosave_key.as_deref(),
            &mut stage,
        )
        .await
        {
            Ok(GatewayTurnResult { response, .. }) => {
                if let Err((status, body)) = authorize_gateway_resource_mutation(
                    &state,
                    &gateway_channel_webhook_operation(channel_key, "send"),
                    ResourceRiskLevel::Low,
                ) {
                    return Ok(jobs::JobOutput::new(status, body.0));
                }
                if let Err(e) = channel.send(&SendMessage::new(response, &msg.reply_target)).await {
                    tracing::error!("Failed to send {channel_key} reply: {e}");
                }
            }
            Err(e) => {
                let reason = TurnFailureClass::classify(stage, &e);
                tracing::error!(reason = %reason, error = %crate::security::redaction::redact_provider_text(&format!("{e:#}")), "Turn failed for {channel_key} message");
                if let Err((status, body)) = authorize_gateway_resource_mutation(
                    &state,
                    &gateway_channel_webhook_operation(channel_key, "send"),
                    ResourceRiskLevel::Low,
                ) {
                    return Ok(jobs::JobOutput::new(status, body.0));
                }
                let _ = channel
                    .send(&SendMessage::new(
                        if e.is::<crate::runtime::ingress_recovery::AlreadyAccepted>() {"This input was already accepted. Execution was not repeated; its outcome remains unconfirmed."} else {"Sorry, I couldn't process your message right now."},
                        &msg.reply_target,
                    ))
                    .await;
            }
        }
    }

    Ok(jobs::JobOutput::new(
        StatusCode::OK,
        serde_json::json!({ "status": "ok", "messages": messages.len() }),
    ))
}

/// `WhatsApp` verification query params
#[derive(serde::Deserialize)]
pub struct WhatsAppVerifyQuery {
    #[serde(rename = "hub.mode")]
    pub mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    pub verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    pub challenge: Option<String>,
}

/// GET /whatsapp — Meta webhook verification
async fn handle_whatsapp_verify(
    State(state): State<AppState>,
    Query(params): Query<WhatsAppVerifyQuery>,
) -> impl IntoResponse {
    let Some(ref wa) = state.whatsapp else {
        return (StatusCode::NOT_FOUND, "WhatsApp not configured".to_string());
    };

    // Verify the token matches (constant-time comparison to prevent timing attacks)
    let token_matches = params
        .verify_token
        .as_deref()
        .is_some_and(|t| constant_time_eq(t, wa.verify_token()));
    if params.mode.as_deref() == Some("subscribe") && token_matches {
        if let Some(ch) = params.challenge {
            tracing::info!("WhatsApp webhook verified successfully");
            return (StatusCode::OK, ch);
        }
        return (StatusCode::BAD_REQUEST, "Missing hub.challenge".to_string());
    }

    tracing::warn!("WhatsApp webhook verification failed — token mismatch");
    (StatusCode::FORBIDDEN, "Forbidden".to_string())
}

/// Verify `WhatsApp` webhook signature (`X-Hub-Signature-256`).
///
/// Returns true if the signature is valid, false otherwise.
/// See: <https://developers.facebook.com/docs/graph-api/webhooks/getting-started#verification-requests>
pub fn verify_whatsapp_signature(app_secret: &str, body: &[u8], signature_header: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    // Signature format: "sha256=<hex_signature>"
    let Some(hex_sig) = signature_header.strip_prefix("sha256=") else {
        return false;
    };

    // Decode hex signature
    let Ok(expected) = hex::decode(hex_sig) else {
        return false;
    };

    // Compute HMAC-SHA256
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(app_secret.as_bytes()) else {
        return false;
    };
    mac.update(body);

    // Constant-time comparison
    mac.verify_slice(&expected).is_ok()
}

/// POST /whatsapp — incoming message webhook
async fn handle_whatsapp_message(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    let Some(ref wa) = state.whatsapp else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "WhatsApp not configured"})),
        );
    };

    // ── Security: Verify X-Hub-Signature-256 — reject if signing secret not configured ──
    let app_secret = match &state.whatsapp_app_secret {
        Some(secret) => secret,
        None => {
            tracing::error!("WhatsApp webhook received but signing secret not configured — rejecting");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Webhook signing secret not configured"})),
            );
        }
    };
    {
        let signature = headers
            .get("X-Hub-Signature-256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !verify_whatsapp_signature(app_secret, &body, signature) {
            tracing::warn!(
                "WhatsApp webhook signature verification failed (signature: {})",
                if signature.is_empty() { "missing" } else { "invalid" }
            );
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Invalid signature"})),
            );
        }
    }

    // Parse JSON body
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid JSON payload"})),
        );
    };

    // Parse messages from the webhook payload
    let messages = wa.parse_webhook_payload(&payload);

    if messages.is_empty() {
        // Acknowledge the webhook even if no messages (could be status updates)
        return (StatusCode::OK, Json(serde_json::json!({"status": "ok"})));
    }

    // Webhook-delivered channels never pass through the channel dispatch loop,
    // so their inbound activity is recorded here instead (see
    // `channels::activity`). Reporting only — nothing acts on it.
    crate::channels::activity::record_inbound(wa.name());

    let channel: Arc<dyn Channel> = Arc::clone(wa) as Arc<dyn Channel>;
    accept_channel_webhook_delivery(&state, channel, "whatsapp", &body, messages, whatsapp_memory_key)
}

/// POST /linq — incoming message webhook (iMessage/RCS/SMS via Linq)
async fn handle_linq_webhook(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    let Some(ref linq) = state.linq else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Linq not configured"})),
        );
    };

    let body_str = String::from_utf8_lossy(&body);

    // ── Security: Verify X-Webhook-Signature — reject if signing secret not configured ──
    let signing_secret = match &state.linq_signing_secret {
        Some(secret) => secret,
        None => {
            tracing::error!("Linq webhook received but signing secret not configured — rejecting");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Webhook signing secret not configured"})),
            );
        }
    };
    {
        let timestamp = headers
            .get("X-Webhook-Timestamp")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let signature = headers
            .get("X-Webhook-Signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !crate::channels::linq::verify_linq_signature(signing_secret, &body_str, timestamp, signature) {
            tracing::warn!(
                "Linq webhook signature verification failed (signature: {})",
                if signature.is_empty() { "missing" } else { "invalid" }
            );
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Invalid signature"})),
            );
        }
    }

    // Parse JSON body
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid JSON payload"})),
        );
    };

    // Parse messages from the webhook payload
    let messages = linq.parse_webhook_payload(&payload);

    if messages.is_empty() {
        // Acknowledge the webhook even if no messages (could be status/delivery events)
        return (StatusCode::OK, Json(serde_json::json!({"status": "ok"})));
    }

    // Webhook-delivered channels never pass through the channel dispatch loop,
    // so their inbound activity is recorded here instead (see
    // `channels::activity`). Reporting only — nothing acts on it.
    crate::channels::activity::record_inbound(linq.name());

    let channel: Arc<dyn Channel> = Arc::clone(linq) as Arc<dyn Channel>;
    accept_channel_webhook_delivery(&state, channel, "linq", &body, messages, linq_memory_key)
}

/// POST /nextcloud-talk — incoming message webhook (Nextcloud Talk bot API)
async fn handle_nextcloud_talk_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let Some(ref nextcloud_talk) = state.nextcloud_talk else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Nextcloud Talk not configured"})),
        );
    };

    let body_str = String::from_utf8_lossy(&body);

    // ── Security: Verify Nextcloud Talk HMAC signature — reject if secret not configured ──
    let webhook_secret = match &state.nextcloud_talk_webhook_secret {
        Some(secret) => secret,
        None => {
            tracing::error!("Nextcloud Talk webhook received but signing secret not configured — rejecting");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Webhook signing secret not configured"})),
            );
        }
    };
    {
        let random = headers
            .get("X-Nextcloud-Talk-Random")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let signature = headers
            .get("X-Nextcloud-Talk-Signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !crate::channels::nextcloud_talk::verify_nextcloud_talk_signature(
            webhook_secret,
            random,
            &body_str,
            signature,
        ) {
            tracing::warn!(
                "Nextcloud Talk webhook signature verification failed (signature: {})",
                if signature.is_empty() { "missing" } else { "invalid" }
            );
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Invalid signature"})),
            );
        }
    }

    // Parse JSON body
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid JSON payload"})),
        );
    };

    // Parse messages from webhook payload
    let messages = nextcloud_talk.parse_webhook_payload(&payload);
    if messages.is_empty() {
        // Acknowledge webhook even if payload does not contain actionable user messages.
        return (StatusCode::OK, Json(serde_json::json!({"status": "ok"})));
    }

    // Webhook-delivered channels never pass through the channel dispatch loop,
    // so their inbound activity is recorded here instead (see
    // `channels::activity`). Reporting only — nothing acts on it.
    crate::channels::activity::record_inbound(nextcloud_talk.name());

    let channel: Arc<dyn Channel> = Arc::clone(nextcloud_talk) as Arc<dyn Channel>;
    accept_channel_webhook_delivery(
        &state,
        channel,
        "nextcloud_talk",
        &body,
        messages,
        nextcloud_talk_memory_key,
    )
}


#[cfg(test)]
pub(crate) use tests::{drive_disabled_engine_webhook_entry, drive_search_webhook_entry};
