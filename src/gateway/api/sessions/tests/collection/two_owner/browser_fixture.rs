#![cfg(test)]
//! Ignored, disposable production-API/browser fixture. Controlled model inference only.
use super::*;
struct CountedRead {
    inner: crate::tools::file_read::FileReadTool,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl crate::tools::Tool for CountedRead {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }
    fn tier(&self) -> crate::tools::traits::ToolTier {
        self.inner.tier()
    }
    fn categories(&self) -> &'static [crate::tools::traits::ToolCategory] {
        self.inner.categories()
    }
    fn supports_owner_filesystem(&self) -> bool {
        self.inner.supports_owner_filesystem()
    }
    async fn execute_with_cancellation(
        &self,
        args: serde_json::Value,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<crate::tools::ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.execute_with_cancellation(args, cancellation).await
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(args).await
    }
}
struct BrowserProvider {
    calls: AtomicUsize,
}
#[async_trait]
impl Provider for BrowserProvider {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("browser fixture requires structured controlled model calls")
    }
    fn supports_native_tools(&self) -> bool {
        true
    }
    async fn chat_traced(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        _: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatTrace> {
        use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, TokenUsage, TokenUsageSource};
        self.calls.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(model == MODEL, "unexpected served model");
        let last_user = request
            .messages
            .iter()
            .rposition(|message| message.role == "user")
            .context("actual input")?;
        let input = &request.messages.get(last_user).context("actual user message")?.content;
        // This is a controlled fixture model, not real model inference. It never
        // grants permission, fabricates tool results or reports an unobserved effect.
        let actual_result = request
            .messages
            .iter()
            .skip(last_user + 1)
            .rev()
            .find(|message| message.role == "tool");
        let (calls, text) = if let Some(result) = actual_result {
            (Vec::new(), Some(result.content.clone()))
        } else {
            let name = if input.starts_with("file_read:") {
                "file_read"
            } else {
                "file_write"
            };
            let tools = request.tools.context("actual published tool schema")?;
            anyhow::ensure!(
                tools.iter().any(|tool| tool.name == name),
                "requested tool is not in actual published surface"
            );
            let args = if name == "file_read" {
                serde_json::json!({"path": TARGET})
            } else {
                let mut matching = OWNERS.iter().filter(|owner| input.contains(owner.2));
                let content = matching.next().context("fixture owner payload")?.2;
                anyhow::ensure!(matching.next().is_none(), "ambiguous fixture payload");
                serde_json::json!({"path": TARGET, "content": content})
            };
            (
                vec![crate::providers::ToolCall {
                    id: format!("prx-browser-{}", uuid::Uuid::new_v4()),
                    name: name.into(),
                    arguments: args.to_string(),
                }],
                None,
            )
        };
        let now = Utc::now();
        Ok(crate::providers::traits::ChatTrace {
            response: ChatResponse {
                text,
                tool_calls: calls,
                reasoning_content: None,
            },
            attempts: vec![ProviderAttempt {
                seq: 1,
                provider: "ollama".into(),
                model: model.into(),
                started_at: now,
                finished_at: now,
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "ollama".into(),
            final_model: model.into(),
            served_member: None,
            tokens_used: TokenUsage {
                prompt_tokens: Some(12),
                completion_tokens: Some(8),
                total_tokens: Some(20),
                source: TokenUsageSource::Reported,
                ..TokenUsage::default()
            },
        })
    }
}
async fn browser_work(endpoint: &str, calls: &AtomicUsize, scratch: &std::path::Path) -> anyhow::Result<()> {
    let _keyring = crate::acl::approval_grant::WitnessKeyring::global_for_tests();
    let workspace = scratch.join("workspace");
    std::fs::create_dir(&workspace)?;
    let mut config = Config::default();
    config.workspace_dir = workspace.clone();
    config.config_path = scratch.join("config.toml");
    config.causal_tree.enabled = true;
    config.autonomy.level = crate::security::AutonomyLevel::Supervised;
    config.default_provider = Some("ollama".into());
    config.default_model = Some(MODEL.into());
    config.providers.models.push(toml::from_str(&format!(
        "provider = \"ollama\"\nmodel = \"{MODEL}\"\noperations = [\"chat\"]\nnative_tools = true\n"
    ))?);
    config.causal_tree.backend.kind =
        crate::causal_tree::config::BackendSelection::Fixed(crate::decision::BackendKind::CompatibleHttp);
    config.causal_tree.backend.endpoint = endpoint.to_owned();
    config.causal_tree.backend.model = DECISION_MODEL.to_owned();
    let priced_provider = format!("custom:{endpoint}");
    config.providers.models.push(serde_json::from_value(serde_json::json!({
        "provider":priced_provider,"model":DECISION_MODEL,"operations":["decision_endpoint"]
    }))?);
    config.cost.prices.insert(
        format!("{priced_provider}/{DECISION_MODEL}"),
        crate::config::schema::ModelPricing {
            input: 0.1,
            output: 0.2,
            cache_write: 0.3,
            cache_read: 0.1,
        },
    );
    assert_eq!(config.causal_tree.policy.extra_latency_budget_ms, 500);
    let provider = Arc::new(BrowserProvider {
        calls: AtomicUsize::new(0),
    });
    let writes = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let mut state = test_app_state(config.clone(), provider.clone());
    state.config = crate::config::new_shared(config.clone());
    state.model = MODEL.into();
    state.mem = Arc::new(crate::memory::SqliteMemory::new(&workspace)?);
    state.pairing = Arc::new(PairingGuard::new(
        true,
        &OWNERS
            .iter()
            .map(|(token, _, _)| (*token).to_owned())
            .collect::<Vec<_>>(),
    ));
    state.tools_registry = Arc::new(vec![
        Box::new(CountedWrite {
            inner: crate::tools::file_write::FileWriteTool::new(Arc::new(
                crate::security::SecurityPolicy::from_config(&config.autonomy, &workspace),
            )),
            calls: Arc::clone(&writes),
        }),
        Box::new(CountedRead {
            inner: crate::tools::file_read::FileReadTool::new(
                Arc::new(crate::security::SecurityPolicy::from_config(
                    &config.autonomy,
                    &workspace,
                )),
                false,
            ),
            calls: Arc::clone(&reads),
        }),
    ]);
    let mut runtime = state.pin_turn_runtime();
    let resolved = crate::providers::selection::check(
        &config,
        "ollama",
        MODEL,
        &crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )?;
    assert_eq!(
        resolved.verification(),
        &crate::providers::catalog::Verification::Verified
    );
    let binding = crate::gateway::decision_source::GatewayProviderBinding::new(Arc::clone(&runtime.provider), resolved);
    let mutable = Arc::get_mut(&mut runtime).context("unpublished fixture runtime")?;
    mutable.provider_binding = Some(Arc::new(binding));
    mutable.decision_slot = Arc::new(crate::gateway::decision_runtime::GatewayDecisionSlot::new(
        Arc::new(crate::security::SecurityPolicy::from_config(
            &config.autonomy,
            &workspace,
        )),
        0,
    ));
    // Conversation metadata only. These actual stored anchors carry no
    // decision, pending approval, grant, selected capability or execution.
    for (token, session, _) in OWNERS {
        let owner = crate::gateway::api::ApiPrincipal::of_token(token);
        let envelope = console_runtime_envelope_for_workspace(
            workspace.to_string_lossy().as_ref(),
            session,
            "console",
            "prx_user",
        )
        .with_run_id(uuid::Uuid::new_v4().to_string())
        .with_config_generation(&runtime.config_generation)
        .with_owner_id(owner.owner_id)
        .with_owner_isolation();
        append_console_turn(
            &state,
            &envelope,
            "user",
            "Console fixture metadata anchor",
            true,
            false,
        )
        .await?;
    }
    let current = Arc::try_unwrap(runtime).map_err(|_| anyhow::anyhow!("fixture runtime unexpectedly shared"))?;
    state.turn_runtime = Some(Arc::new(crate::gateway::GatewayTurnRuntimeOwner::new(
        current,
        Arc::clone(&state.config),
        Arc::clone(&state.mem),
        None,
        None,
        None,
        None,
        Arc::clone(&state.hooks),
        #[cfg(feature = "wasm-plugins")]
        None,
    )));
    let runtime = state.pin_turn_runtime();
    let service = Arc::clone(runtime.decision_slot.publish(&runtime, &state).await?);

    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = axum::Router::new()
        .nest("/api", crate::gateway::api::router(state.clone()))
        .merge(crate::gateway::ui::router())
        .with_state(state.clone());
    let stop = tokio_util::sync::CancellationToken::new();
    let server_stop = stop.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(server_stop.cancelled_owned())
            .await
    });
    let result = std::panic::AssertUnwindSafe(async {
        let mut owners = Vec::new();
        for (token, session, content) in OWNERS {
            let owner = crate::gateway::api::ApiPrincipal::of_token(token);
            let scope = console_runtime_envelope_for_workspace(workspace.to_string_lossy().as_ref(), session, "console", "prx_user")
                .with_owner_id(owner.owner_id.clone()).with_owner_isolation().memory_principal();
            let filesystem = crate::tools::owner_filesystem::OwnerFilesystem::open(scratch, &scope.workspace_id, &owner.owner_id, &workspace)?;
            owners.push(serde_json::json!({"token":token,"session":session,"content":content,"private_file":filesystem.resolve(TARGET)?.operation_target()}));
        }
        let ready = serde_json::json!({"url":url,"owners":owners,"target":TARGET,"scratch":scratch,"stop_file":scratch.join("STOP"),"scope":"controlled local model; actual current-source Console assets/API/auth/approval/owner filesystem"});
        let bytes = serde_json::to_vec_pretty(&ready)?;
        // Fixed disposable test tokens only; retain mode-0600 ready file, never real credentials.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        use std::io::Write as _;
        options.open(scratch.join("ready.json"))?.write_all(&bytes)?;
        println!("PRX_CONSOLE_BROWSER_READY={}", scratch.join("ready.json").display());
        let deadline = tokio::time::Instant::now() + Duration::from_mins(15);
        loop {
            if scratch.join("STOP").exists() { break; }
            anyhow::ensure!(!server.is_finished(), "browser fixture listener exited early");
            anyhow::ensure!(tokio::time::Instant::now() < deadline, "browser driver deadline exceeded");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok::<(), anyhow::Error>(())
    }).catch_unwind().await;
    stop.cancel();
    let joined = match tokio::time::timeout(Duration::from_secs(5), &mut server).await {
        Ok(result) => result.is_ok_and(|served| served.is_ok()),
        Err(_) => {
            server.abort();
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut server).await;
            false
        }
    };
    let drained = runtime
        .decision_slot
        .cleanup
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await;
    let writer_drained = service.drain_writes(Instant::now() + Duration::from_secs(5)).await;
    let summary = serde_json::json!({"listener_joined":joined,"cleanup_drained":drained.is_ok(),"writer_drained":writer_drained,"model_calls":provider.calls.load(Ordering::SeqCst),"decision_calls":calls.load(Ordering::SeqCst),"write_calls":writes.load(Ordering::SeqCst),"read_calls":reads.load(Ordering::SeqCst),"browser_work_ok":matches!(&result, Ok(Ok(())))});
    std::fs::write(
        scratch.join("fixture-summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    anyhow::ensure!(
        joined && drained.is_ok() && writer_drained && matches!(result, Ok(Ok(()))),
        "browser fixture failed: {summary}"
    );
    anyhow::ensure!(
        writes.load(Ordering::SeqCst) == 8,
        "four locales times two owners require exactly eight real writes"
    );
    anyhow::ensure!(
        reads.load(Ordering::SeqCst) == 8,
        "four locales times two owners require exactly eight real reads"
    );
    for (token, session, content) in OWNERS {
        let scope = console_runtime_envelope_for_workspace(
            workspace.to_string_lossy().as_ref(),
            session,
            "console",
            "prx_user",
        )
        .with_owner_id(crate::gateway::api::ApiPrincipal::of_token(token).owner_id)
        .with_owner_isolation()
        .memory_principal();
        let events = state.mem.list_message_events_since(&scope, 0, 4096).await?;
        std::fs::write(
            scratch.join(format!("{session}-events.json")),
            serde_json::to_vec_pretty(&events)?,
        )?;
        for kind in [
            "approval.decision.pending",
            "approval.decision.reviewed",
            "approval.decision.resolved",
        ] {
            let rows = events
                .iter()
                .filter(|event| event.event_type == kind)
                .collect::<Vec<_>>();
            anyhow::ensure!(rows.len() == 4, "exactly four owner-scoped {kind} events");
            for event in rows {
                anyhow::ensure!(
                    event.owner_id == scope.owner_id
                        && event.workspace_id == scope.workspace_id
                        && event.session_key == scope.session_key,
                    "receipt escaped owner/session/workspace"
                );
                if kind == "approval.decision.resolved" {
                    let claim: serde_json::Value =
                        serde_json::from_str(event.raw_payload_json.as_deref().context("claim payload")?)?;
                    anyhow::ensure!(
                        claim.get("disposition").and_then(serde_json::Value::as_str) == Some("consumed"),
                        "approval must be consumed by actual execution"
                    );
                }
            }
        }
        anyhow::ensure!(
            events
                .iter()
                .filter(|event| event.event_type == "turn.finalized")
                .count()
                == 12,
            "four collection + four resume + four read turns must finalize"
        );
        let finals = events
            .iter()
            .filter(|event| event.event_type == "tool.execution.finalized")
            .collect::<Vec<_>>();
        anyhow::ensure!(
            finals.len() == 8,
            "four real writes and four real reads require eight final receipts"
        );
        for event in finals {
            let terminal: serde_json::Value =
                serde_json::from_str(event.raw_payload_json.as_deref().context("tool final payload")?)?;
            let reservation = terminal.get("reservation").context("final reservation")?;
            anyhow::ensure!(
                terminal.pointer("/outcome/status").and_then(serde_json::Value::as_str) == Some("succeeded"),
                "tool operation did not succeed"
            );
            let reserved = events
                .iter()
                .find(|candidate| {
                    candidate.event_type == "tool.execution.reserved"
                        && candidate.run_id == event.run_id
                        && candidate
                            .raw_payload_json
                            .as_deref()
                            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                            .as_ref()
                            == Some(reservation)
                })
                .context("exact reservation identity")?;
            let source = events
                .iter()
                .find(|candidate| {
                    candidate.event_type == "message.created"
                        && candidate.role == "user"
                        && candidate.run_id == event.run_id
                })
                .context("actual source ingress")?;
            let finalized_turn = events
                .iter()
                .find(|candidate| candidate.event_type == "turn.finalized" && candidate.run_id == event.run_id)
                .context("actual turn terminal")?;
            anyhow::ensure!(
                source.id < reserved.id && reserved.id < event.id && event.id < finalized_turn.id,
                "source/reservation/tool/turn causal order"
            );
            let run_id = event.run_id.as_deref().context("execution run identity")?;
            for linked in [source, reserved, event, finalized_turn] {
                anyhow::ensure!(
                    linked.owner_id == scope.owner_id
                        && linked.workspace_id == scope.workspace_id
                        && linked.session_key == scope.session_key
                        && linked.run_id.as_deref() == Some(run_id),
                    "source/tool/terminal owner-workspace-session-run mismatch"
                );
            }
            let capability = reservation
                .get("capability")
                .and_then(serde_json::Value::as_str)
                .context("actual reserved capability")?;
            anyhow::ensure!(
                matches!(capability, "file_write" | "file_read"),
                "unexpected browser fixture effect"
            );
            if capability == "file_write" {
                // Claim row scope is the ORIGINAL collection. The authenticated
                // execution payload binds the FRESH resume, exactly as replay.rs.
                let terminal_payload: serde_json::Value = serde_json::from_str(
                    finalized_turn
                        .raw_payload_json
                        .as_deref()
                        .context("whole-turn terminal payload")?,
                )?;
                let decision_id = terminal_payload
                    .get("decision_id")
                    .and_then(serde_json::Value::as_str)
                    .context("fresh resume decision identity")?;
                let claims = events
                    .iter()
                    .filter(|claim| claim.event_type == "approval.decision.resolved")
                    .map(|claim| {
                        let payload: serde_json::Value =
                            serde_json::from_str(claim.raw_payload_json.as_deref().context("claim payload")?)?;
                        Ok::<_, anyhow::Error>((claim, payload))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let matching = claims
                    .iter()
                    .filter(|(_, payload)| {
                        payload.pointer("/execution/run_id").and_then(serde_json::Value::as_str) == Some(run_id)
                    })
                    .collect::<Vec<_>>();
                anyhow::ensure!(
                    matching.len() == 1,
                    "each actual write requires exactly one consumed approval for its execution run"
                );
                let (claim, payload) = matching.first().context("matching consumed approval")?;
                anyhow::ensure!(
                    claim.owner_id == scope.owner_id
                        && claim.workspace_id == scope.workspace_id
                        && claim.session_key == scope.session_key,
                    "consumed approval belongs to another owner/session/workspace"
                );
                anyhow::ensure!(
                    payload.get("disposition").and_then(serde_json::Value::as_str) == Some("consumed")
                        && payload
                            .pointer("/execution/source_event_id")
                            .and_then(serde_json::Value::as_str)
                            == Some(source.event_id.as_str())
                        && payload
                            .pointer("/execution/decision_id")
                            .and_then(serde_json::Value::as_str)
                            == Some(decision_id),
                    "consumed approval differs from actual source and fresh decision"
                );
                let original_id = payload
                    .pointer("/binding/decision_id")
                    .and_then(serde_json::Value::as_str)
                    .context("original collection decision")?;
                anyhow::ensure!(
                    original_id != decision_id && claim.run_id.as_deref() != Some(run_id),
                    "collection and resume must have independent decision/run identities"
                );
                anyhow::ensure!(
                    reservation.get("operation_id").and_then(serde_json::Value::as_str)
                        == Some(format!("decision-resume:{original_id}").as_str()),
                    "actual write reservation differs from approved original call"
                );
                anyhow::ensure!(
                    source.id < claim.id && claim.id < event.id,
                    "consumption must follow fresh ingress and precede actual execution finalization"
                );
            }
        }
        let filesystem = crate::tools::owner_filesystem::OwnerFilesystem::open(
            scratch,
            &scope.workspace_id,
            scope.owner_id.as_deref().context("owner")?,
            &workspace,
        )?;
        anyhow::ensure!(
            std::fs::read_to_string(filesystem.resolve(TARGET)?.operation_target())? == content,
            "owner-private final bytes differ"
        );
    }
    anyhow::ensure!(
        !workspace.join(TARGET).exists(),
        "tool wrote shared workspace instead of owner private filesystem"
    );
    Ok(())
}
#[tokio::test]
#[ignore = "explicit managed loopback browser fixture; requires PRX_CONSOLE_BROWSER_OUTPUT"]
async fn console_browser_production_api_fixture() -> anyhow::Result<()> {
    let output =
        std::env::var_os("PRX_CONSOLE_BROWSER_OUTPUT").context("explicit fixture output directory required")?;
    let output = std::path::PathBuf::from(output);
    anyhow::ensure!(
        output.is_absolute() && output.is_dir(),
        "fixture output must be an existing absolute directory"
    );
    // Always retained, including failure/panic. Root owns subsequent verified cleanup.
    let scratch = tempfile::Builder::new()
        .prefix("prx-console-browser-")
        .tempdir_in(&output)?
        .keep();
    let calls = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let stop = tokio_util::sync::CancellationToken::new();
    let server_stop = stop.clone();
    let app = axum::Router::new()
        .route("/v1/systemone", axum::routing::post(decision_reply))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .with_state(Arc::clone(&calls));
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(server_stop.cancelled_owned())
            .await
    });
    let result = std::panic::AssertUnwindSafe(Box::pin(browser_work(&endpoint, &calls, &scratch)))
        .catch_unwind()
        .await;
    stop.cancel();
    let joined = match tokio::time::timeout(Duration::from_secs(5), &mut server).await {
        Ok(result) => result.is_ok_and(|served| served.is_ok()),
        Err(_) => {
            server.abort();
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut server).await;
            false
        }
    };
    anyhow::ensure!(
        joined && matches!(result, Ok(Ok(()))),
        "browser fixture retained at {}; decision listener joined={joined}; result={result:?}",
        scratch.display()
    );
    Ok(())
}
