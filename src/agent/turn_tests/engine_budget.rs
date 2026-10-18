#![cfg(test)]
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]

use super::*;
use crate::agent::turn_tests::business_tool_specs_of;

/// The only part of the appended prompt sections the preflight cannot pin
/// exactly. `CompiledToolContext::apply_to_messages` joins them onto the
/// system message with a blank line, so the outgoing text is one `\n\n`
/// longer than the text that was budgeted, and the two token streams meet
/// at a boundary a standalone measurement cannot see. One token of join,
/// doubled when the character heuristic is the estimator in play.
const APPENDED_SECTION_JOIN_SLACK_TOKENS: usize = 2 * TOKEN_HEURISTIC_BUDGET_MULTIPLIER;

/// Tokens the compiled snapshot appends to the system prompt, measured the
/// way a fixture can measure them: through the same `apply_to_messages`
/// the request itself goes through, onto an empty system message. Slightly
/// above the production measurement, which tokenizes the sections on their
/// own, by the probe message's role and framing.
fn compiled_instruction_tokens(tool_specs: &[crate::tools::ToolSpec]) -> usize {
    let compiled = crate::agent::turn_context::CompiledToolContext::new(true, tool_specs.to_vec());
    let mut probe = vec![ChatMessage::system(String::new())];
    compiled.apply_to_messages(&mut probe);
    budgeted_tokens(estimate_history_tokens_with_source(&probe))
}

// Q58 fixture sizing preserves the intended main-request window while
// retaining the production decision reservation and original response reserve.
fn decision_sized_fixture_config(
    mut config: crate::config::AgentCompactionConfig,
) -> crate::config::AgentCompactionConfig {
    let main_window = config.max_context_tokens.checked_sub(config.reserve_tokens).unwrap();
    let mut available = main_window;
    while available - (available / 16).min(4096) < main_window {
        available = available.checked_add(1).unwrap();
    }
    let decision_tokens = (available / 16).min(4096);
    assert!(
        decision_tokens >= 137,
        "fixture must fund the observed candidate estimates"
    );
    assert_eq!(available - decision_tokens, main_window);
    config.max_context_tokens = available.checked_add(config.reserve_tokens).unwrap();
    config
}

fn fixture_main_compaction(config: &crate::config::AgentCompactionConfig) -> crate::config::AgentCompactionConfig {
    let mut main = config.clone();
    let available = main.max_context_tokens.checked_sub(main.reserve_tokens).unwrap();
    main.reserve_tokens = main.reserve_tokens.checked_add((available / 16).min(4096)).unwrap();
    main
}

#[tokio::test]
async fn configured_extra_token_ratio_caps_real_turn_decision_admission() {
    let workspace = TempDir::new().expect("budget fixture workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.agent.compaction.max_context_tokens = 32_768;
    config.agent.compaction.max_context_tokens_explicit = true;
    config.agent.compaction.reserve_tokens = 4_096;
    config.causal_tree.policy.extra_token_ratio_limit = 0.01;

    let tools_registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
    let admission =
        crate::agent::turn_tests::admission::AdmittedTurn::with_config(config, "Explain `2 + 2`.", tools_registry)
            .await;

    let baseline_tokens = 32_768_usize - 4_096;
    let ratio_limit = ((baseline_tokens as f64) * f64::from(0.01_f32)).floor() as usize;
    assert_eq!(ratio_limit, 286, "fixture must exercise the configured ratio cap");
    assert!(ratio_limit < baseline_tokens / 16, "the ratio must be the active cap");
    assert_eq!(admission.compaction().reserve_tokens, 4_096 + ratio_limit);
    assert_eq!(
        admission
            .compaction()
            .max_context_tokens
            .checked_sub(admission.compaction().reserve_tokens),
        Some(baseline_tokens - ratio_limit),
        "the production admission must reserve no more than the configured extra-token ratio"
    );
}

/// A history that fits the window on its own can still overrun it once the
/// native tool schemas are appended to the same request. The budget has to
/// account for them before the provider is reached, not after.
#[tokio::test]
async fn context_budget_counts_native_tool_schemas_before_provider_call() {
    // Q45: Native schemas participate in request token budgeting.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(WideSchemaTool::with_fields("file_read", 16))];
        let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
        let tool_specs = business_tool_specs_of(&tools_registry);
        let tool_payload = crate::agent::turn_context::measure_native_tool_payload(&tool_specs);
        let tool_tokens = budgeted_tokens(tool_payload.measurement);

        // Four times the schema payload, so trimming can still bring the whole
        // request under the window instead of the schemas alone overrunning it.
        let mut history = context_budget_fixture_history(tool_tokens.saturating_mul(4));
        history
            .last_mut()
            .expect("current fixture input")
            .content
            .push_str(" Use file_read.");
        let original_len = history.len();
        let config = context_budget_fixture_config(&history, tool_tokens / 2);
        let usable_window = config.max_context_tokens.saturating_sub(config.reserve_tokens);
        // Q53: scale the paired schemas and history together, retaining the same
        // headroom formula and response reserve. The real decision slice is capped
        // at 4096 and the loopback plan splits that slice across two possible sends.
        let decision_tokens = (usable_window / 16).min(4096);
        assert_eq!(
            decision_tokens, 4096,
            "scaled fixture must reach the existing decision reservation cap"
        );
        assert_eq!(config.reserve_tokens, CONTEXT_BUDGET_FIXTURE_RESERVE_TOKENS);

        assert!(
            !plan_context_budget(&history, &config, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
            "the fixture history must fit the window on its own"
        );
        assert!(
            budgeted_tokens(estimate_history_tokens_with_source(&history)).saturating_add(tool_tokens) > usable_window,
            "the fixture schemas must push the complete request over the window"
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let max_request_tokens = Arc::new(AtomicUsize::new(0));
        let provider = NativeToolProvider::recording("done", &calls, &max_request_tokens);

        let workspace = TempDir::new().expect("budget fixture workspace");
        let mut admission_config = crate::config::Config::default();
        admission_config.workspace_dir = workspace.path().to_path_buf();
        admission_config.agent.compaction = config.clone();
        let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_config(
            admission_config,
            &history.last().expect("current fixture input").content,
            Arc::clone(&tools_registry),
        )
        .await;
        admission.assert_core_specs(&tool_specs);

        let result = run_turn(
            &mut history,
            crate::agent::turn_request::TurnRequest {
                channel_name: "cli",
                scope: None,
                mode: ChatMode::default(),
                silent: true,
                expose_stay_silent: false,
                multimodal: &crate::config::MultimodalConfig::default(),
                schedule: crate::agent::turn_request::TurnSchedule {
                    read_only_tool_concurrency_window: 2,
                    priority_scheduling_enabled: false,
                    low_priority_tool_names: Vec::new(),
                },
                compaction: Some(admission.compaction()),
                decision: admission.decision(),
            },
            crate::agent::turn_request::TurnServices {
                provider: &provider,
                provider_name: "ollama",
                model: "prx-local-model",
                temperature: 0.0,
                tools_registry,
                observer: &NoopObserver,
                hooks: &HookManager::new(std::env::temp_dir()),
                approval: None,
                approval_resolver: None,
                runtime_adapter: Some(admission.adapter()),
                memory: ToolLoopMemory::none(),
            },
            crate::agent::turn_request::TurnDelivery {
                cancellation: None,
                on_delta: None,
                on_tool_call: None,
            },
        )
        .await
        .map(|(outcome, _trace)| outcome.into_text())
        .expect("the preflight must keep the turn inside the window, not fail it");

        assert_eq!(result, "done");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one provider round-trip");
        assert!(
            history.len() < original_len,
            "the existing policy must shrink the request before the provider is reached"
        );
        assert!(
            max_request_tokens.load(Ordering::SeqCst)
                <= usable_window.saturating_add(APPENDED_SECTION_JOIN_SLACK_TOKENS),
            "the provider must not be called with a request the window cannot hold: {} tokens against {usable_window}",
            max_request_tokens.load(Ordering::SeqCst)
        );
    })
    .await;
}

/// The control: the same history and the same window, with small schemas,
/// must reach the provider exactly as it did before tool payloads were
/// counted.
#[tokio::test]
async fn small_native_tool_schemas_leave_the_turn_on_the_unchanged_provider_path() {
    configured_small_native_schema_fixture().await;
}

#[tokio::test]
async fn excluded_native_tool_schema_does_not_admit_provider_work() {
    small_native_schema_fixture(true).await;
}

// Keep this owned fixture's concrete work out of CleanupScope's select state.
#[inline(never)]
fn boxed_native_schema_work<'a>(
    work: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'a,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
    Box::pin(work)
}

async fn small_native_schema_fixture(excluded: bool) {
    let wide_registry = crate::agent::turn_tests::admission::registry_with_search(vec![Box::new(
        WideSchemaTool::with_fields("file_read", 1),
    )]);
    let wide_specs = business_tool_specs_of(&wide_registry);
    let wide_tool_tokens =
        budgeted_tokens(crate::agent::turn_context::measure_native_tool_payload(&wide_specs).measurement);

    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(NarrowSchemaTool { name: "file_read" })];
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let tool_specs = business_tool_specs_of(&tools_registry);
    let tool_tokens = budgeted_tokens(crate::agent::turn_context::measure_native_tool_payload(&tool_specs).measurement);

    let mut history = context_budget_fixture_history(wide_tool_tokens.saturating_mul(4));
    history
        .last_mut()
        .expect("current fixture input")
        .content
        .push_str(" Use file_read on /prx/missing.");
    let original_len = history.len();
    let config = context_budget_fixture_config(&history, wide_tool_tokens / 2);
    let usable_window = config.max_context_tokens.saturating_sub(config.reserve_tokens);
    assert!(
        budgeted_tokens(estimate_history_tokens_with_source(&history)).saturating_add(tool_tokens) <= usable_window,
        "the control schemas must still fit the same window"
    );

    let tmp = TempDir::new().unwrap();
    let cleanup =
        crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new()).unwrap();
    cleanup
        .run(boxed_native_schema_work(async {
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
            let mut admission_config = crate::config::Config::default();
            admission_config.workspace_dir = tmp.path().to_path_buf();
            admission_config.agent.compaction = config.clone();
            admission_config.causal_tree.enabled = false;
            admission_config.tool_tiering.always_include = vec!["file_read".to_owned()];
            if excluded {
                admission_config
                    .tool_tiering
                    .always_exclude
                    .push("file_read".to_owned());
            }
            admission_config.cost.prices.insert(
                "ollama/prx-local-model".to_owned(),
                crate::config::schema::ModelPricing {
                    input: 1.0,
                    output: 1.0,
                    cache_write: 1.0,
                    cache_read: 1.0,
                },
            );
            assert!(matches!(
                crate::llm::route_decision::usage_cost_resolution(
                    "ollama", "prx-local-model", 1, 0, 0, 0, &admission_config.cost
                ),
                crate::llm::route_decision::UsagePrice::Priced(value) if value > 0.0
            ));
            let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_memory(
                admission_config,
                &history.last().expect("current fixture input").content,
                Arc::clone(&tools_registry),
                Arc::clone(&memory),
            )
            .await;
            let decision = admission.decision();
            if excluded {
                assert!(
                    decision
                        .service
                        .agent_execution_specs(decision.proof, decision.context)
                        .is_err()
                );
                assert!(matches!(
                    decision.context.disposition().unwrap(),
                    crate::runtime::turn_decision::TurnDisposition::Close { .. }
                ));
            } else {
                let diagnostic = decision.context.audit_hops().map(|hop| serde_json::json!({
                    "selection":format!("{:?}", hop.selection),
                    "factory_prediction":format!("{:?}", hop.prediction),
                    "factory_attempts":hop.prediction.as_ref().map(|prediction| format!("{:?}", prediction.attempts)),
                    "request_compiled":hop.request_compiled,
                    "candidates":format!("{:?}", hop.candidates),
                    "post_rehearsal":format!("{:?}", hop.post_rehearsal),
                    "pre_commit":format!("{:?}", hop.pre_commit),
                    "audit":format!("{:?}", hop.audit_trace),
                })).collect::<Vec<_>>();
                eprintln!(
                    "actual_small_native_admission={}",
                    serde_json::to_string(&diagnostic).unwrap()
                );
                admission.assert_core_specs(&tool_specs);
            }
            let runtime = admission.memory_runtime();

            let calls = Arc::new(AtomicUsize::new(0));
            let max_request_tokens = Arc::new(AtomicUsize::new(0));
            let provider = NativeToolProvider::recording("done", &calls, &max_request_tokens);

            let result = run_turn(
                &mut history,
                crate::agent::turn_request::TurnRequest {
                    channel_name: "terminal",
                    scope: None,
                    mode: ChatMode::default(),
                    silent: true,
                    expose_stay_silent: false,
                    multimodal: &crate::config::MultimodalConfig::default(),
                    schedule: crate::agent::turn_request::TurnSchedule {
                        read_only_tool_concurrency_window: 2,
                        priority_scheduling_enabled: false,
                        low_priority_tool_names: Vec::new(),
                    },
                    compaction: Some(admission.compaction()),
                    decision: admission.decision(),
                },
                crate::agent::turn_request::TurnServices {
                    provider: &provider,
                    provider_name: "ollama",
                    model: "prx-local-model",
                    temperature: 0.0,
                    tools_registry,
                    observer: &NoopObserver,
                    hooks: &HookManager::new(tmp.path().to_path_buf()),
                    approval: None,
                    approval_resolver: None,
                    runtime_adapter: Some(admission.adapter()),
                    memory: runtime,
                },
                crate::agent::turn_request::TurnDelivery {
                    cancellation: None,
                    on_delta: None,
                    on_tool_call: None,
                },
            )
            .await
            .map(|(outcome, _trace)| outcome.into_text());

            if excluded {
                assert_eq!(calls.load(Ordering::SeqCst), 0, "excluded tool admits no provider work");
                let error = result.expect_err("the low-level engine must reject a closed admission");
                assert!(
                    error.to_string().contains("decision has no executable selection"),
                    "{error:#}"
                );
                return Ok(());
            }
            let result = result.expect("a request that fits must reach the provider untouched");

            assert_eq!(result, "done");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                history.len(),
                original_len.saturating_add(1),
                "nothing may be compacted away; only the answer is appended"
            );

            // The durable event and the request that left for the provider must
            // report the same total, because both are produced by one estimator.
            let principal = admission.principal();
            let events = memory.list_message_events_since(&principal, 0, 10).await.unwrap();
            let payload: serde_json::Value = serde_json::from_str(
                events
                    .iter()
                    .find(|event| event.event_type == "llm.request.context.compiled")
                    .and_then(|event| event.raw_payload_json.as_deref())
                    .expect("compiled request context payload"),
            )
            .unwrap();
            assert_eq!(
                payload["request_estimate"]["total_tokens"].as_u64(),
                u64::try_from(max_request_tokens.load(Ordering::SeqCst)).ok(),
                "the reported total must be the total that was sent"
            );
            Ok(())
        }))
        .await
        .unwrap();
}

async fn configured_small_native_schema_fixture() {
    use futures::FutureExt;
    let excluded = false;
    let wide_registry = crate::agent::turn_tests::admission::registry_with_search(vec![Box::new(
        WideSchemaTool::with_fields("file_read", 16),
    )]);
    let wide_specs = business_tool_specs_of(&wide_registry);
    let wide_tool_tokens =
        budgeted_tokens(crate::agent::turn_context::measure_native_tool_payload(&wide_specs).measurement);

    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(WideSchemaTool::with_fields("file_read", 1))];
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let tool_specs = business_tool_specs_of(&tools_registry);
    let tool_tokens = budgeted_tokens(crate::agent::turn_context::measure_native_tool_payload(&tool_specs).measurement);

    let mut history = context_budget_fixture_history(wide_tool_tokens.saturating_mul(4));
    history
        .last_mut()
        .expect("current fixture input")
        .content
        .push_str(" Use file_read.");
    let original_len = history.len();
    let config = context_budget_fixture_config(&history, wide_tool_tokens / 2);
    let usable_window = config.max_context_tokens.saturating_sub(config.reserve_tokens);
    // Q53: scale the paired schemas and history together, retaining the same
    // headroom formula and response reserve. The real decision slice is capped
    // at 4096 and the loopback plan splits that slice across two possible sends.
    let decision_tokens = (usable_window / 16).min(4096);
    assert_eq!(
        decision_tokens, 4096,
        "scaled fixture must reach the existing decision reservation cap"
    );
    assert_eq!(config.reserve_tokens, CONTEXT_BUDGET_FIXTURE_RESERVE_TOKENS);
    let main_input_limit = usable_window.checked_sub(decision_tokens).expect("main input room");
    let narrow_total = budgeted_tokens(estimate_history_tokens_with_source(&history))
        .saturating_add(tool_tokens)
        .saturating_add(compiled_instruction_tokens(&tool_specs));
    assert!(
        narrow_total <= main_input_limit,
        "scaled small request must fit after the unchanged decision reservation"
    );
    // Preserve the original registered-wide construction premise; it is not
    // proof that an actual business-only wide request would exceed this window.
    assert!(
        budgeted_tokens(estimate_history_tokens_with_source(&history)).saturating_add(wide_tool_tokens) > usable_window,
        "paired wide request must still exceed the same original window"
    );

    assert!(
        budgeted_tokens(estimate_history_tokens_with_source(&history)).saturating_add(tool_tokens) <= usable_window,
        "the control schemas must still fit the same window"
    );

    let workspace = TempDir::new().unwrap().keep();
    eprintln!("owned_native_schema_workspace={}", workspace.display());
    let tmp = workspace.as_path();
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("native_tool")
        .await
        .expect("actual native decision endpoint")
        .retain_evidence_at(workspace.clone());
    let endpoint = driver.endpoint().to_owned();
    driver
        .run_factory(true, || async {
            let cleanup =
                crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                    .unwrap();
            let work = boxed_native_schema_work(async {
                let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp).unwrap());
                let mut admission_config = crate::config::Config::default();
                admission_config.workspace_dir = tmp.to_path_buf();
                admission_config.agent.compaction = config.clone();
                admission_config.causal_tree.enabled = false;
                admission_config.tool_tiering.always_include = vec!["file_read".to_owned()];
                if excluded {
                    admission_config
                        .tool_tiering
                        .always_exclude
                        .push("file_read".to_owned());
                }
                admission_config.cost.prices.insert(
                    "ollama/prx-local-model".to_owned(),
                    crate::config::schema::ModelPricing {
                        input: 1.0,
                        output: 1.0,
                        cache_write: 1.0,
                        cache_read: 1.0,
                    },
                );
                assert!(matches!(
                    crate::llm::route_decision::usage_cost_resolution(
                        "ollama", "prx-local-model", 1, 0, 0, 0, &admission_config.cost
                    ),
                    crate::llm::route_decision::UsagePrice::Priced(value) if value > 0.0
                ));
                let calls = Arc::new(AtomicUsize::new(0));
                let max_request_tokens = Arc::new(AtomicUsize::new(0));
                let provider = NativeToolProvider::recording("done", &calls, &max_request_tokens);
                let admission = crate::agent::turn_tests::admission::AdmittedTurn::configured_native_schema(
                    admission_config,
                    &history.last().expect("current fixture input").content,
                    Arc::clone(&tools_registry),
                    Arc::clone(&memory),
                    &endpoint,
                    &provider,
                )
                .await;
                let decision = admission.decision();
                if excluded {
                    assert!(
                        decision
                            .service
                            .agent_execution_specs(decision.proof, decision.context)
                            .is_err()
                    );
                    assert!(matches!(
                        decision.context.disposition().unwrap(),
                        crate::runtime::turn_decision::TurnDisposition::Close { .. }
                    ));
                } else {
                    let diagnostic = decision.context.audit_hops().map(|hop| serde_json::json!({
                    "selection":format!("{:?}", hop.selection),
                    "factory_prediction":format!("{:?}", hop.prediction),
                    "factory_attempts":hop.prediction.as_ref().map(|prediction| format!("{:?}", prediction.attempts)),
                    "request_compiled":hop.request_compiled,
                    "candidates":format!("{:?}", hop.candidates),
                    "post_rehearsal":format!("{:?}", hop.post_rehearsal),
                    "pre_commit":format!("{:?}", hop.pre_commit),
                    "audit":format!("{:?}", hop.audit_trace),
                })).collect::<Vec<_>>();
                    eprintln!(
                        "actual_small_native_admission={}",
                        serde_json::to_string(&diagnostic).unwrap()
                    );
                    let business_specs: Vec<_> = tool_specs
                        .iter()
                        .filter(|spec| spec.name == "file_read")
                        .cloned()
                        .collect();
                    assert_eq!(business_specs.len(), 1, "one exact file_read business definition");
                    admission.assert_core_specs(&business_specs);
                    assert!(
                        decision
                            .service
                            .agent_capability_search_spec(decision.proof, decision.context)
                            .expect("optional search publication")
                            .is_none(),
                        "this fixture has no discoverable family"
                    );
                    let actual_narrow_total = budgeted_tokens(estimate_history_tokens_with_source(&history))
                        .saturating_add(budgeted_tokens(
                            crate::agent::turn_context::measure_native_tool_payload(&business_specs).measurement,
                        ))
                        .saturating_add(compiled_instruction_tokens(&business_specs));
                    assert!(
                        actual_narrow_total <= main_input_limit,
                        "actual file_read-only request must fit the unchanged main window"
                    );
                }
                let runtime = admission.memory_runtime();

                let result = run_turn(
                    &mut history,
                    crate::agent::turn_request::TurnRequest {
                        channel_name: "terminal",
                        scope: None,
                        mode: ChatMode::default(),
                        silent: true,
                        expose_stay_silent: false,
                        multimodal: &crate::config::MultimodalConfig::default(),
                        schedule: crate::agent::turn_request::TurnSchedule {
                            read_only_tool_concurrency_window: 2,
                            priority_scheduling_enabled: false,
                            low_priority_tool_names: Vec::new(),
                        },
                        compaction: Some(admission.compaction()),
                        decision: admission.decision(),
                    },
                    crate::agent::turn_request::TurnServices {
                        provider: &provider,
                        provider_name: "ollama",
                        model: "prx-local-model",
                        temperature: 0.0,
                        tools_registry,
                        observer: &NoopObserver,
                        hooks: &HookManager::new(tmp.to_path_buf()),
                        approval: None,
                        approval_resolver: None,
                        runtime_adapter: Some(admission.adapter()),
                        memory: runtime,
                    },
                    crate::agent::turn_request::TurnDelivery {
                        cancellation: None,
                        on_delta: None,
                        on_tool_call: None,
                    },
                )
                .await
                .map(|(outcome, _trace)| outcome.into_text());

                if excluded {
                    assert_eq!(calls.load(Ordering::SeqCst), 0, "excluded tool admits no provider work");
                    let error = result.expect_err("the low-level engine must reject a closed admission");
                    assert!(
                        error.to_string().contains("decision has no executable selection"),
                        "{error:#}"
                    );
                    return Ok(());
                }
                let result = result.expect("a request that fits must reach the provider untouched");

                assert_eq!(result, "done");
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert_eq!(
                    history.len(),
                    original_len.saturating_add(1),
                    "nothing may be compacted away; only the answer is appended"
                );

                // The durable event and the request that left for the provider must
                // report the same total, because both are produced by one estimator.
                let principal = admission.principal();
                let events = memory.list_message_events_since(&principal, 0, 10).await.unwrap();
                let payload: serde_json::Value = serde_json::from_str(
                    events
                        .iter()
                        .find(|event| event.event_type == "llm.request.context.compiled")
                        .and_then(|event| event.raw_payload_json.as_deref())
                        .expect("compiled request context payload"),
                )
                .unwrap();
                assert_eq!(
                    payload["request_estimate"]["total_tokens"].as_u64(),
                    u64::try_from(max_request_tokens.load(Ordering::SeqCst)).ok(),
                    "the reported total must be the total that was sent"
                );
                let _ = admission.decision().context.terminal_execution_trace();
                Ok(())
            });
            let caught = cleanup
                .run(async { Ok(std::panic::AssertUnwindSafe(work).catch_unwind().await) })
                .await
                .expect("actual native fixture owner and both queues drain");
            match caught {
                Ok(result) => result.unwrap(),
                Err(panic) => std::panic::resume_unwind(panic),
            }
        })
        .await;
}
/// One estimator, two inputs: the preflight measures `history`, the event
/// measures the prepared request. The event carries both totals, so the
/// difference between what the window decision was made on and what
/// actually left is a number an operator can read.
#[tokio::test]
async fn the_compiled_request_event_reports_the_budgeted_and_the_prepared_totals() {
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(NarrowSchemaTool { name: "file_read" })];
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let tool_specs = business_tool_specs_of(&tools_registry);
    let tool_surface = crate::agent::turn_context::measure_request_tool_surface(true, &tool_specs);
    let native_tokens = budgeted_tokens(tool_surface.native_payload.measurement);
    let instruction_tokens = budgeted_tokens(tool_surface.instructions);
    assert!(
        instruction_tokens > 0,
        "the fixture must publish sections that are appended after the budget"
    );

    let mut history = context_budget_fixture_history(instruction_tokens.saturating_mul(8).max(4096));
    history
        .last_mut()
        .expect("current fixture input")
        .content
        .push_str(" Use file_read.");
    let config = decision_sized_fixture_config(context_budget_fixture_config(
        &history,
        instruction_tokens.saturating_mul(4),
    ));
    let main_compaction = fixture_main_compaction(&config);
    let history_tokens = budgeted_tokens(estimate_history_tokens_with_source(&history));
    assert!(
        !plan_context_budget(&history, &main_compaction, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
        "nothing may be trimmed, so the budgeted figures stay comparable with the fixture"
    );

    let tmp = TempDir::new().expect("test: temporary directory");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
    let mut admission_config = crate::config::Config::default();
    admission_config.workspace_dir = tmp.path().to_path_buf();
    admission_config.agent.compaction = config.clone();
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_memory(
        admission_config,
        &history.last().expect("current fixture input").content,
        Arc::clone(&tools_registry),
        Arc::clone(&memory),
    )
    .await;
    admission.assert_core_specs(&tool_specs);
    let runtime = admission.memory_runtime();

    let calls = Arc::new(AtomicUsize::new(0));
    let max_request_tokens = Arc::new(AtomicUsize::new(0));
    let provider = NativeToolProvider::recording("done", &calls, &max_request_tokens);

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "terminal",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: Some(admission.compaction()),
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &HookManager::new(tmp.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: runtime,
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect("a request that fits must reach the provider");
    assert_eq!(result, "done");

    let principal = admission.principal();
    let events = memory
        .list_message_events_since(&principal, 0, 10)
        .await
        .expect("test: message events");
    let payload: serde_json::Value = serde_json::from_str(
        events
            .iter()
            .find(|event| event.event_type == "llm.request.context.compiled")
            .and_then(|event| event.raw_payload_json.as_deref())
            .expect("test: compiled request context payload"),
    )
    .expect("test: payload json");
    let estimate = &payload["request_estimate"];

    assert_eq!(
        estimate["budgeted_messages_tokens"].as_u64(),
        u64::try_from(history_tokens).ok(),
        "the budgeted message figure is the history the preflight measured"
    );
    assert_eq!(
        estimate["budgeted_total_tokens"].as_u64(),
        u64::try_from(
            history_tokens
                .saturating_add(native_tokens)
                .saturating_add(instruction_tokens)
                .saturating_add(APPENDED_SECTION_FRAMING_TOKENS)
        )
        .ok(),
        "the budgeted total is that history plus everything the tool surface adds, framing included"
    );
    assert_eq!(
        estimate["instruction_tokens"].as_u64(),
        u64::try_from(instruction_tokens).ok(),
        "the appended sections are reported on their own so the delta is attributable"
    );
    assert_eq!(
        estimate["total_tokens"].as_u64(),
        u64::try_from(max_request_tokens.load(Ordering::SeqCst)).ok(),
        "the prepared total is the total that was sent"
    );
    assert_ne!(
        estimate["total_tokens"].as_u64(),
        estimate["budgeted_messages_tokens"]
            .as_u64()
            .map(|tokens| tokens.saturating_add(u64::try_from(native_tokens).unwrap_or(u64::MAX))),
        "the prepared request carries the appended sections the budgeted message figure does not"
    );
}

/// The compiled snapshot appends its prompt sections to the system message
/// after the preflight has run, so a request that fits the window only
/// because those sections were invisible to the budget has to be brought
/// back inside it.
///
/// `gpio_read` is one of the names that make the snapshot render the
/// hardware-access section, so the appended prompt text dominates this
/// fixture's very small native payload: what overruns the window here is
/// prompt, not schema.
#[tokio::test]
async fn context_budget_counts_the_appended_prompt_sections_before_the_provider_call() {
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(NarrowSchemaTool { name: "gpio_read" })];
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    // This structural request retains the separately gated search control on
    // its terminal provider hop, so the budget must include that exact schema.
    let tool_specs = tool_specs_of(&tools_registry);
    let appended_tokens = compiled_instruction_tokens(&tool_specs);
    let payload_tokens =
        budgeted_tokens(crate::agent::turn_context::measure_native_tool_payload(&tool_specs).measurement);
    assert!(
        payload_tokens < appended_tokens,
        "the appended sections, not the schemas, must be what overruns this fixture: \
             {payload_tokens} payload tokens against {appended_tokens} appended tokens"
    );

    let mut history = context_budget_fixture_history(appended_tokens.saturating_mul(8).max(4096));
    history
        .last_mut()
        .expect("current fixture input")
        .content
        .push_str(" Use gpio_read.");
    let oldest_turn = history.get(1).map(|message| message.content.clone());
    assert!(oldest_turn.is_some(), "the fixture must have a message to give up");
    let config = decision_sized_fixture_config(context_budget_fixture_config(&history, appended_tokens));
    let main_compaction = fixture_main_compaction(&config);
    let usable_window = main_compaction
        .max_context_tokens
        .saturating_sub(main_compaction.reserve_tokens);
    assert!(
        !plan_context_budget(&history, &main_compaction, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
        "the fixture history must fit the window on its own"
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let max_request_tokens = Arc::new(AtomicUsize::new(0));
    let provider = NativeToolProvider::recording("done", &calls, &max_request_tokens);

    let workspace = TempDir::new().expect("budget fixture workspace");
    let mut admission_config = crate::config::Config::default();
    admission_config.workspace_dir = workspace.path().to_path_buf();
    admission_config.agent.compaction = config.clone();
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_config(
        admission_config,
        &history.last().expect("current fixture input").content,
        Arc::clone(&tools_registry),
    )
    .await;
    admission.assert_core_specs(&tool_specs);

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: Some(admission.compaction()),
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &HookManager::new(std::env::temp_dir()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect("the preflight must keep the turn inside the window, not fail it");

    assert_eq!(result, "done");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one provider round-trip");
    assert_ne!(
        history.get(1).map(|message| message.content.clone()),
        oldest_turn,
        "the appended prompt sections must be budgeted, so the oldest turn has to give way to them"
    );
    assert!(
        max_request_tokens.load(Ordering::SeqCst) <= usable_window.saturating_add(APPENDED_SECTION_JOIN_SLACK_TOKENS),
        "the provider must not be called with a request the window cannot hold: {} tokens against {usable_window}",
        max_request_tokens.load(Ordering::SeqCst)
    );
}

/// The mid-turn trim that runs after a tool round has to aim at the same
/// reserved window the preflight aims at. Trimming against the unreserved
/// window undoes the reservation, and the next iteration's preflight then
/// has to compact history the trim could have shortened for free.
#[tokio::test]
async fn the_mid_turn_trim_after_a_tool_round_aims_at_the_reserved_window() {
    // Q45: Post-tool-round trimming uses the reserved context window.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        // The same admitted tool supplies the wide schema and the large result,
        // so the reserved surface is the surface that actually executes.
        let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(
            WideSchemaTool::with_fields("file_read", 1).with_output("mid turn tool output ".repeat(100)),
        )];
        let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
        let tool_specs = business_tool_specs_of(&tools_registry);
        let payload_tokens =
            budgeted_tokens(crate::agent::turn_context::measure_native_tool_payload(&tool_specs).measurement);
        let appended_tokens = compiled_instruction_tokens(&tool_specs);

        // The window holds the history and the whole published tool surface,
        // with nothing to spare: the first iteration fits exactly, and only the
        // tool round pushes it over.
        let mut history = context_budget_fixture_history(payload_tokens.saturating_mul(2));
        history
            .last_mut()
            .expect("current fixture input")
            .content
            .push_str(" Use file_read.");
        let oldest_turn = history
            .get(1)
            .map(|message| message.content.clone())
            .expect("test: the fixture must have a message the trim can free");
        let config = crate::config::AgentCompactionConfig {
            // Summary compaction is what an over-budget preflight reaches for,
            // and the mark it leaves in history is what makes the difference
            // observable from outside the loop.
            mode: crate::config::AgentCompactionMode::Aggressive,
            ..decision_sized_fixture_config(context_budget_fixture_config(
                &history,
                payload_tokens.saturating_add(appended_tokens),
            ))
        };
        assert!(
            !plan_context_budget(&history, &config, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
            "the fixture history must fit the window on its own"
        );

        let provider = NativeScriptedProvider::new(vec![
            ChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "file_read".to_string(),
                    arguments: "{}".to_string(),
                }],
                reasoning_content: None,
            },
            ChatResponse {
                text: Some("done".to_string()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
        ]);

        // The admitted tool must produce the large result that fills the round;
        // reuse the same durable backend and source scope throughout.
        let tmp = TempDir::new().expect("test: temporary directory");
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
        let mut admission_config = crate::config::Config::default();
        admission_config.workspace_dir = tmp.path().to_path_buf();
        admission_config.agent.compaction = config.clone();
        let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_memory(
            admission_config,
            &history.last().expect("current fixture input").content,
            Arc::clone(&tools_registry),
            Arc::clone(&memory),
        )
        .await;
        admission.assert_core_specs(&tool_specs);
        let runtime = admission.memory_runtime();

        let result = run_turn(
            &mut history,
            crate::agent::turn_request::TurnRequest {
                channel_name: "cli",
                scope: None,
                mode: ChatMode::default(),
                silent: true,
                expose_stay_silent: false,
                multimodal: &crate::config::MultimodalConfig::default(),
                schedule: crate::agent::turn_request::TurnSchedule {
                    read_only_tool_concurrency_window: 2,
                    priority_scheduling_enabled: false,
                    low_priority_tool_names: Vec::new(),
                },
                compaction: Some(admission.compaction()),
                decision: admission.decision(),
            },
            crate::agent::turn_request::TurnServices {
                provider: &provider,
                provider_name: "ollama",
                model: "prx-local-model",
                temperature: 0.0,
                tools_registry,
                observer: &NoopObserver,
                hooks: &HookManager::new(tmp.path().to_path_buf()),
                approval: None,
                approval_resolver: None,
                runtime_adapter: Some(admission.adapter()),
                memory: runtime,
            },
            crate::agent::turn_request::TurnDelivery {
                cancellation: None,
                on_delta: None,
                on_tool_call: None,
            },
        )
        .await
        .map(|(outcome, _trace)| outcome.into_text())
        .expect("the turn must complete");

        assert_eq!(result, "done");
        assert!(
            !history
                .iter()
                .any(|message| message.content.starts_with("[Context compacted at")),
            "the mid-turn trim already had the room; the next preflight must not have to compact"
        );
        // Beside the absence assertion: the trim has to have actually run, or
        // the fixture could pass by never reaching it at all.
        assert!(
            !history.iter().any(|message| message.content == oldest_turn),
            "the mid-turn trim must have freed the oldest turn against the reserved window"
        );
    })
    .await;
}

/// A tool payload larger than the whole configured window drives the usable
/// input window to zero, and every trim below it then removes messages
/// until it hits a floor. The current turn's user message is the request
/// itself and is never part of what may be freed: answering a question that
/// is no longer in the transcript is worse than serving an over-budget
/// request, which the provider-side overflow retry already covers.
#[tokio::test]
async fn an_oversized_tool_payload_never_trims_away_the_current_user_message() {
    // Q45: Oversized schema budgeting retains current user message.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        const CURRENT_TURN_MARKER: &str = "current-turn-request-marker: use file_read";

        let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(WideSchemaTool::new("file_read"))];
        let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
        let tool_specs = business_tool_specs_of(&tools_registry);
        let payload_tokens =
            budgeted_tokens(crate::agent::turn_context::measure_native_tool_payload(&tool_specs).measurement);

        let mut history = context_budget_fixture_history(payload_tokens / 8);
        history.push(ChatMessage::user(CURRENT_TURN_MARKER));
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Aggressive,
            reserve_tokens: CONTEXT_BUDGET_FIXTURE_RESERVE_TOKENS,
            keep_recent_messages: 2,
            memory_flush: false,
            max_context_tokens: payload_tokens / 2,
            max_context_tokens_explicit: true,
            ..crate::config::AgentCompactionConfig::default()
        };
        assert!(
            payload_tokens > config.max_context_tokens,
            "the fixture schemas alone must exceed the whole configured window"
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let max_request_tokens = Arc::new(AtomicUsize::new(0));
        let provider = NativeToolProvider::recording("done", &calls, &max_request_tokens);

        let workspace = TempDir::new().expect("budget fixture workspace");
        let mut admission_config = crate::config::Config::default();
        admission_config.workspace_dir = workspace.path().to_path_buf();
        admission_config.agent.compaction = config.clone();
        let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_config(
            admission_config,
            &history.last().expect("current fixture input").content,
            Arc::clone(&tools_registry),
        )
        .await;
        admission.assert_core_specs(&tool_specs);

        let result = run_turn(
            &mut history,
            crate::agent::turn_request::TurnRequest {
                channel_name: "cli",
                scope: None,
                mode: ChatMode::default(),
                silent: true,
                expose_stay_silent: false,
                multimodal: &crate::config::MultimodalConfig::default(),
                schedule: crate::agent::turn_request::TurnSchedule {
                    read_only_tool_concurrency_window: 2,
                    priority_scheduling_enabled: false,
                    low_priority_tool_names: Vec::new(),
                },
                compaction: Some(admission.compaction()),
                decision: admission.decision(),
            },
            crate::agent::turn_request::TurnServices {
                provider: &provider,
                provider_name: "ollama",
                model: "prx-local-model",
                temperature: 0.0,
                tools_registry,
                observer: &NoopObserver,
                hooks: &HookManager::new(std::env::temp_dir()),
                approval: None,
                approval_resolver: None,
                runtime_adapter: Some(admission.adapter()),
                memory: ToolLoopMemory::none(),
            },
            crate::agent::turn_request::TurnDelivery {
                cancellation: None,
                on_delta: None,
                on_tool_call: None,
            },
        )
        .await
        .map(|(outcome, _trace)| outcome.into_text())
        .expect("an unfittable request keeps the existing provider-side fallback, not a new rejection");

        assert_eq!(result, "done");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "there is no context-budget rejection path: the provider is still reached"
        );
        assert!(
            history
                .iter()
                .any(|message| message.role == "user" && message.content.contains(CURRENT_TURN_MARKER)),
            "the current turn's user message must survive a window too small to hold the schemas"
        );
    })
    .await;
}

/// The tool schemas and the appended prompt sections have already taken
/// their share of the window when a tool result is written into history, so
/// the inline cap has to be planned on the same reserved window the
/// preflight and the mid-turn trim are planned on.
#[tokio::test]
async fn the_inline_tool_result_budget_aims_at_the_reserved_window() {
    // Q45: Inline tool-result trimming respects reserved context window.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        const INLINE_MARKER: &str = "inline-budget-probe-marker";
        /// Room left over once the published tool surface is reserved.
        const RESERVED_SLACK_TOKENS: usize = 400;

        let output = format!("{INLINE_MARKER} {}", "inline budget payload ".repeat(45));
        let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(
            WideSchemaTool::with_fields("file_read", 1).with_output(output.clone()),
        )];
        let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
        let tool_specs = business_tool_specs_of(&tools_registry);
        let tool_surface = crate::agent::turn_context::measure_request_tool_surface(true, &tool_specs);
        let surface_tokens = budgeted_request_tool_tokens(tool_surface);

        let mut history = context_budget_fixture_history(surface_tokens / 8);
        history
            .last_mut()
            .expect("current fixture input")
            .content
            .push_str(" Use file_read.");
        let config = context_budget_fixture_config(&history, surface_tokens.saturating_add(RESERVED_SLACK_TOKENS));
        let reserved = reserve_request_tool_surface(&config, tool_surface);
        assert!(
            output.len() <= tool_result_inline_budget_for_history("file_read", &history, Some(&config)),
            "the unreserved window would have kept the whole result inline"
        );
        assert!(
            output.len() > tool_result_inline_budget_for_history("file_read", &history, Some(&reserved)),
            "the reserved window must not have room for the whole result"
        );

        let provider = NativeScriptedProvider::new(vec![
            ChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "file_read".to_string(),
                    arguments: "{}".to_string(),
                }],
                reasoning_content: None,
            },
            ChatResponse {
                text: Some("done".to_string()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
        ]);

        // The tool has to really execute: a loop with no durable ledger refuses
        // execution and leaves a one-line error in place of the result.
        let tmp = TempDir::new().expect("test: temporary directory");
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
        let mut admission_config = crate::config::Config::default();
        admission_config.workspace_dir = tmp.path().to_path_buf();
        admission_config.agent.compaction = config.clone();
        let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_memory(
            admission_config,
            &history.last().expect("current fixture input").content,
            Arc::clone(&tools_registry),
            Arc::clone(&memory),
        )
        .await;
        admission.assert_core_specs(&tool_specs);
        let runtime = admission.memory_runtime();

        let result = run_turn(
            &mut history,
            crate::agent::turn_request::TurnRequest {
                channel_name: "cli",
                scope: None,
                mode: ChatMode::default(),
                silent: true,
                expose_stay_silent: false,
                multimodal: &crate::config::MultimodalConfig::default(),
                schedule: crate::agent::turn_request::TurnSchedule {
                    read_only_tool_concurrency_window: 2,
                    priority_scheduling_enabled: false,
                    low_priority_tool_names: Vec::new(),
                },
                compaction: Some(admission.compaction()),
                decision: admission.decision(),
            },
            crate::agent::turn_request::TurnServices {
                provider: &provider,
                provider_name: "ollama",
                model: "prx-local-model",
                temperature: 0.0,
                tools_registry,
                observer: &NoopObserver,
                hooks: &HookManager::new(tmp.path().to_path_buf()),
                approval: None,
                approval_resolver: None,
                runtime_adapter: Some(admission.adapter()),
                memory: runtime,
            },
            crate::agent::turn_request::TurnDelivery {
                cancellation: None,
                on_delta: None,
                on_tool_call: None,
            },
        )
        .await
        .map(|(outcome, _trace)| outcome.into_text())
        .expect("test: the turn must complete");

        assert_eq!(result, "done");
        let inline = history
            .iter()
            .find(|message| message.content.contains(INLINE_MARKER))
            .expect("test: the tool result must still be in history");
        assert!(
            inline.content.contains("[document_ingest_ref]"),
            "the reserved window has no room for the whole result, so it must be capped"
        );
        assert!(
            !inline.content.contains(&output),
            "a capped result carries a preview and a reference, not the whole payload"
        );
    })
    .await;
}

/// Room the mid-turn fixtures leave beside the history: enough that the
/// first iteration fits the reserved window with nothing to spare, and far
/// less than one tool round adds, so only the round can push the turn over.
const MID_TURN_SATURATED_SLACK_TOKENS: usize = 8;

/// Room the control fixture leaves: more than a whole tool round, so the
/// mid-turn trim never fires and history must come out untouched.
const MID_TURN_UNSATURATED_SLACK_TOKENS: usize = 4_096;

/// A window that holds exactly this history plus `slack_tokens` once the
/// published tool surface has taken its share of it.
///
/// Summary compaction is off: it would leave a marker in history and change
/// what the next request is made of, and these fixtures are about the trim.
fn mid_turn_floor_fixture_config(
    history: &[ChatMessage],
    tool_surface: crate::agent::turn_context::RequestToolMeasurement,
    slack_tokens: usize,
) -> crate::config::AgentCompactionConfig {
    crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        reserve_tokens: CONTEXT_BUDGET_FIXTURE_RESERVE_TOKENS,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: budgeted_tokens(estimate_history_tokens_with_source(history))
            .saturating_add(slack_tokens)
            .saturating_add(budgeted_request_tool_tokens(tool_surface))
            .saturating_add(CONTEXT_BUDGET_FIXTURE_RESERVE_TOKENS),
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    }
}

/// Premise for the saturated mid-turn fixtures: freeing the older exchange
/// alone could not have satisfied the trim, so it had to reach the
/// question.
///
/// The older exchange's absence proves only that *a* trim ran. If the
/// fixture's arithmetic drifted so the trim stopped right after that
/// exchange, the question would never have been removed, and the
/// "restored exactly once" assertions would pass while proving nothing
/// about the floor (R-B6i R3). So the history the trim faced, minus the
/// older exchange, must still exceed the limit the mid-turn trim enforces.
/// It is rebuilt from the second request with the fixture's own system
/// prompt, because the served system message also carries the rendered
/// tool instructions the reserved window already accounts for.
fn assert_the_trim_had_to_reach_the_question(second: &[ChatMessage], reserved: &crate::config::AgentCompactionConfig) {
    let without_the_older_exchange: Vec<ChatMessage> =
        std::iter::once(ChatMessage::system("stable base system prompt"))
            .chain(second.iter().filter(|message| message.role != "system").cloned())
            .collect();
    let tokens = estimate_history_tokens(&without_the_older_exchange);
    let limit = reserved.max_context_tokens.saturating_sub(reserved.reserve_tokens);
    assert!(
        tokens > limit,
        "premise: the history minus the older exchange ({tokens} tokens) must still exceed the \
             mid-turn limit ({limit}), or the trim stopped before the question and this fixture \
             proves nothing about the floor"
    );
}

/// Records the exact message list of every request it is asked to serve and
/// answers from a script. The capability flag picks the tool dialect, so one
/// fixture covers the native and the prompt-guided path.
struct RequestRecordingProvider {
    native_tools: bool,
    responses: Arc<Mutex<VecDeque<ChatResponse>>>,
    requests: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
}

impl RequestRecordingProvider {
    fn new(native_tools: bool, responses: Vec<ChatResponse>) -> Self {
        Self {
            native_tools,
            responses: Arc::new(Mutex::new(responses.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<Vec<ChatMessage>> {
        self.requests.lock().expect("test: request log lock").clone()
    }
}

#[async_trait]
impl Provider for RequestRecordingProvider {
    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        crate::providers::traits::ProviderCapabilities {
            native_tool_calling: self.native_tools,
            vision: false,
        }
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        anyhow::bail!("test: chat_with_system is not on the tool-loop path these fixtures drive")
    }

    async fn chat(&self, request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        self.requests
            .lock()
            .expect("test: request log lock")
            .push(request.messages.to_vec());
        self.responses
            .lock()
            .expect("test: response script lock")
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("test: the scripted provider ran out of responses"))
    }
}

/// One prompt-guided tool call, in the tag form `parse_tool_calls` reads.
fn prompt_guided_tool_call_response(tool: &str) -> ChatResponse {
    ChatResponse {
        text: Some(format!(
            "<tool_call>\n{{\"name\":\"{tool}\",\"arguments\":{{}}}}\n</tool_call>"
        )),
        tool_calls: Vec::new(),
        reasoning_content: None,
    }
}

fn scripted_text_response(text: &str) -> ChatResponse {
    ChatResponse {
        text: Some(text.to_string()),
        tool_calls: Vec::new(),
        reasoning_content: None,
    }
}

/// The preflight's request floor only guards the top of an iteration. The
/// trim that runs *after* a tool round had no floor at all, so on a
/// saturated window it deleted the turn's question before the next
/// iteration's preflight could capture it — and from there the model went on
/// working without ever seeing what it was asked.
///
/// Prompt-guided half: the round comes back as a `[Tool results]` user
/// block, so once the question is gone nothing in history resembles it and
/// no later floor can reconstruct it.
#[tokio::test]
async fn the_mid_turn_trim_keeps_a_prompt_guided_request_for_the_next_iteration() {
    const QUESTION_MARKER: &str = "mid-turn-prompt-guided-request-marker";
    const OLDER_TURN_MARKER: &str = "mid-turn-prompt-guided-older-turn-marker";

    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(NamedLargeOutputTool {
        name: "file_read".to_string(),
        output: "mid turn tool output ".repeat(8),
    })];
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let tool_specs = business_tool_specs_of(&tools_registry);
    let tool_surface = crate::agent::turn_context::measure_request_tool_surface(false, &tool_specs);

    let question = format!("{QUESTION_MARKER} Use file_read. {}", "question-bulk ".repeat(1600));
    // A small older exchange ahead of the question. The trim frees it before
    // it reaches the question, so its absence from the second request is what
    // proves the window really saturated and the trim really ran.
    let mut history = vec![
        ChatMessage::system("stable base system prompt"),
        ChatMessage::user(format!("{OLDER_TURN_MARKER} settled earlier")),
        ChatMessage::assistant("settled"),
        ChatMessage::user(question.clone()),
    ];
    let config = decision_sized_fixture_config(mid_turn_floor_fixture_config(
        &history,
        tool_surface,
        MID_TURN_SATURATED_SLACK_TOKENS,
    ));
    let reserved = reserve_request_tool_surface(&fixture_main_compaction(&config), tool_surface);
    assert!(
        !plan_context_budget(&history, &reserved, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
        "the fixture's first iteration must fit the reserved window, or the trim under test \
             never has a request to protect"
    );

    let provider = RequestRecordingProvider::new(
        false,
        vec![
            prompt_guided_tool_call_response("file_read"),
            scripted_text_response("done"),
        ],
    );

    // Reuse the actual memory backend for admission and the tool result,
    // so the round is produced in the same source-owned execution scope.
    let tmp = TempDir::new().expect("test: temporary directory");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
    let mut admission_config = crate::config::Config::default();
    admission_config.workspace_dir = tmp.path().to_path_buf();
    admission_config.agent.compaction = config.clone();
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_memory(
        admission_config,
        &question,
        Arc::clone(&tools_registry),
        Arc::clone(&memory),
    )
    .await;
    admission.assert_core_specs(&tool_specs);
    let runtime = admission.memory_runtime();

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: Some(admission.compaction()),
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &HookManager::new(tmp.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: runtime,
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect("test: the turn must complete");

    assert_eq!(result, "done");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "the fixture must reach a second iteration");
    let first = requests.first().expect("test: the first request was recorded");
    assert!(
        first
            .iter()
            .any(|message| message.role == "user" && message.content.contains(QUESTION_MARKER)),
        "premise: the first iteration carried the question"
    );

    let second = requests.get(1).expect("test: the second request was recorded");
    assert!(
        !second.iter().any(|message| message.content.contains(OLDER_TURN_MARKER)),
        "premise: the window must really have saturated. The older exchange is what the trim \
             frees before it reaches the question, so if it is still in the second request no trim \
             ran and this fixture proves nothing about the floor"
    );
    assert_the_trim_had_to_reach_the_question(second, &reserved);
    assert_eq!(
        second
            .iter()
            .filter(|message| message.role == "user" && message.content.contains(QUESTION_MARKER))
            .count(),
        1,
        "the mid-turn trim deleted the turn's question, so the second iteration asks the model to \
             continue a turn whose request is no longer in the transcript — and it must come back \
             exactly once, never duplicated by a second floor"
    );
    assert!(
        second.iter().any(|message| message
            .content
            .contains(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX)),
        "the round the model just paid for must still be in the request beside the question"
    );
}

/// The native-mode twin. Here the round is an assistant `tool_calls`
/// envelope followed by `tool` results, so beside the question the request
/// has a pairing invariant to keep: no restored message may fall between an
/// envelope and the results answering it.
#[tokio::test]
async fn the_mid_turn_trim_keeps_a_native_request_and_its_tool_round_paired() {
    const QUESTION_MARKER: &str = "mid-turn-native-request-marker";
    const OLDER_TURN_MARKER: &str = "mid-turn-native-older-turn-marker";

    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(NamedLargeOutputTool {
        name: "file_read".to_string(),
        output: "mid turn tool output ".repeat(8),
    })];
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let tool_specs = business_tool_specs_of(&tools_registry);
    let tool_surface = crate::agent::turn_context::measure_request_tool_surface(true, &tool_specs);

    let question = format!("{QUESTION_MARKER} Use file_read. {}", "question-bulk ".repeat(1600));
    // Same older exchange as the prompt-guided twin, for the same reason: it
    // is freed before the question, so its absence proves the trim ran.
    let mut history = vec![
        ChatMessage::system("stable base system prompt"),
        ChatMessage::user(format!("{OLDER_TURN_MARKER} settled earlier")),
        ChatMessage::assistant("settled"),
        ChatMessage::user(question.clone()),
    ];
    let config = decision_sized_fixture_config(mid_turn_floor_fixture_config(
        &history,
        tool_surface,
        MID_TURN_SATURATED_SLACK_TOKENS,
    ));
    let reserved = reserve_request_tool_surface(&fixture_main_compaction(&config), tool_surface);
    assert!(
        !plan_context_budget(&history, &reserved, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
        "the fixture's first iteration must fit the reserved window, or the trim under test \
             never has a request to protect"
    );

    let provider = RequestRecordingProvider::new(
        true,
        vec![
            ChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "file_read".to_string(),
                    arguments: "{}".to_string(),
                }],
                reasoning_content: None,
            },
            scripted_text_response("done"),
        ],
    );

    let tmp = TempDir::new().expect("test: temporary directory");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
    let mut admission_config = crate::config::Config::default();
    admission_config.workspace_dir = tmp.path().to_path_buf();
    admission_config.agent.compaction = config.clone();
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_memory(
        admission_config,
        &question,
        Arc::clone(&tools_registry),
        Arc::clone(&memory),
    )
    .await;
    admission.assert_core_specs(&tool_specs);
    let runtime = admission.memory_runtime();

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: Some(admission.compaction()),
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &HookManager::new(tmp.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: runtime,
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect("test: the turn must complete");

    assert_eq!(result, "done");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "the fixture must reach a second iteration");
    let second = requests.get(1).expect("test: the second request was recorded");
    assert!(
        !second.iter().any(|message| message.content.contains(OLDER_TURN_MARKER)),
        "premise: the window must really have saturated. The older exchange is what the trim \
             frees before it reaches the question, so if it is still in the second request no trim \
             ran and this fixture proves nothing about the floor"
    );
    assert_the_trim_had_to_reach_the_question(second, &reserved);
    assert_eq!(
        second
            .iter()
            .filter(|message| message.content.contains(QUESTION_MARKER))
            .count(),
        1,
        "the turn's question must reach the second iteration exactly once"
    );
    assert!(
        second.iter().any(|message| message.role == "tool"),
        "the round the model just paid for must still be in the request beside the question"
    );
    assert_eq!(
        first_orphan_tool_index(second),
        None,
        "no restored message may strand a tool result from the envelope that requested it"
    );
    let envelope_index = second
        .iter()
        .position(|message| message.role == "assistant" && is_tool_roundtrip_companion(message))
        .expect("test: the assistant tool_calls envelope must still be in the request");
    assert_eq!(
        second.get(envelope_index.saturating_add(1)).map(|m| m.role.as_str()),
        Some("tool"),
        "the assistant tool_calls envelope must be immediately followed by its results"
    );
}

/// The control. On a window with room to spare the mid-turn trim does not
/// run, and the floor must therefore change nothing at all: no second copy
/// of the request, no message moved, nothing freed.
#[tokio::test]
async fn an_unsaturated_mid_turn_window_leaves_history_exactly_as_it_was() {
    // Q45: Unsaturated context window preserves history unchanged.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        const QUESTION_MARKER: &str = "mid-turn-control-request-marker";
        const OLDER_TURN_MARKER: &str = "mid-turn-control-older-turn-marker";

        let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(NamedLargeOutputTool {
            name: "file_read".to_string(),
            output: "mid turn tool output ".repeat(8),
        })];
        let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
        let tool_specs = business_tool_specs_of(&tools_registry);
        let tool_surface = crate::agent::turn_context::measure_request_tool_surface(false, &tool_specs);

        let question = format!("{QUESTION_MARKER} Use file_read. {}", "question-bulk ".repeat(200));
        // The same fixture the two saturated tests use, so the contrast is the
        // window and nothing else: here the older exchange must still be there.
        let mut history = vec![
            ChatMessage::system("stable base system prompt"),
            ChatMessage::user(format!("{OLDER_TURN_MARKER} settled earlier")),
            ChatMessage::assistant("settled"),
            ChatMessage::user(question.clone()),
        ];
        let config = mid_turn_floor_fixture_config(&history, tool_surface, MID_TURN_UNSATURATED_SLACK_TOKENS);

        let provider = RequestRecordingProvider::new(
            false,
            vec![
                prompt_guided_tool_call_response("file_read"),
                scripted_text_response("done"),
            ],
        );

        let tmp = TempDir::new().expect("test: temporary directory");
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
        let mut admission_config = crate::config::Config::default();
        admission_config.workspace_dir = tmp.path().to_path_buf();
        admission_config.agent.compaction = config.clone();
        let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_memory(
            admission_config,
            &question,
            Arc::clone(&tools_registry),
            Arc::clone(&memory),
        )
        .await;
        admission.assert_core_specs(&tool_specs);
        let runtime = admission.memory_runtime();

        let result = run_turn(
            &mut history,
            crate::agent::turn_request::TurnRequest {
                channel_name: "cli",
                scope: None,
                mode: ChatMode::default(),
                silent: true,
                expose_stay_silent: false,
                multimodal: &crate::config::MultimodalConfig::default(),
                schedule: crate::agent::turn_request::TurnSchedule {
                    read_only_tool_concurrency_window: 2,
                    priority_scheduling_enabled: false,
                    low_priority_tool_names: Vec::new(),
                },
                compaction: Some(admission.compaction()),
                decision: admission.decision(),
            },
            crate::agent::turn_request::TurnServices {
                provider: &provider,
                provider_name: "ollama",
                model: "prx-local-model",
                temperature: 0.0,
                tools_registry,
                observer: &NoopObserver,
                hooks: &HookManager::new(tmp.path().to_path_buf()),
                approval: None,
                approval_resolver: None,
                runtime_adapter: Some(admission.adapter()),
                memory: runtime,
            },
            crate::agent::turn_request::TurnDelivery {
                cancellation: None,
                on_delta: None,
                on_tool_call: None,
            },
        )
        .await
        .map(|(outcome, _trace)| outcome.into_text())
        .expect("test: the turn must complete");

        assert_eq!(result, "done");
        assert_eq!(
            history
                .iter()
                .filter(|message| message.content.contains(QUESTION_MARKER))
                .count(),
            1,
            "an untriggered trim must not leave a second copy of the request behind"
        );
        assert_eq!(
            history.first().map(|message| message.role.as_str()),
            Some("system"),
            "nothing may be inserted above the system prompt"
        );
        assert!(
            history
                .iter()
                .any(|message| message.content.contains(OLDER_TURN_MARKER)),
            "an untriggered trim frees nothing, so the older exchange the saturated fixtures lose \
                 must still be here"
        );
        assert_eq!(
            history.get(3).map(|message| message.content.as_str()),
            Some(question.as_str()),
            "the request must still be exactly where the turn put it"
        );
        assert_eq!(
            history.get(4).map(|message| message.role.as_str()),
            Some("assistant"),
            "the round must follow the request in the order the loop wrote it"
        );
        assert!(
            history.get(5).is_some_and(|message| message.role == "user"
                && message
                    .content
                    .starts_with(crate::agent::turn_contracts::TOOL_RESULTS_PREFIX)),
            "the tool results must still follow the assistant turn that asked for them"
        );
    })
    .await;
}

/// The role sequence of a recorded request, which is what a provider
/// template sees before it sees any content.
fn role_sequence(messages: &[ChatMessage]) -> Vec<&str> {
    messages.iter().map(|message| message.role.as_str()).collect()
}

/// The preflight runs at the top of *every* iteration, so from the second
/// one on it edits a history that ends in the round just executed — and it,
/// not the mid-turn trim, decides the shape of the request the provider
/// actually receives. With compaction off its only removal is a delete-only
/// trim, and putting the request back at the tail there opens the transcript
/// with the round's `assistant` turn and trails the question behind the
/// results it triggered.
///
/// Prompt-guided half: the results block is itself a `user` message, so the
/// tail restore also lands two `user` turns side by side — the alternation
/// break [`append_corrective_aside`] folds away rather than create, and that
/// strict chat templates reject outright.
#[tokio::test]
async fn a_trim_only_preflight_opens_the_prompt_guided_second_request_on_the_question() {
    const QUESTION_MARKER: &str = "preflight-position-prompt-guided-marker";
    const OLDER_TURN_MARKER: &str = "preflight-position-prompt-guided-older-turn-marker";

    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(NamedLargeOutputTool {
        name: "file_read".to_string(),
        output: "mid turn tool output ".repeat(8),
    })];
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let tool_specs = business_tool_specs_of(&tools_registry);
    let tool_surface = crate::agent::turn_context::measure_request_tool_surface(false, &tool_specs);

    let question = format!("{QUESTION_MARKER} Use file_read. {}", "question-bulk ".repeat(1600));
    let mut history = vec![
        ChatMessage::system("stable base system prompt"),
        ChatMessage::user(format!("{OLDER_TURN_MARKER} settled earlier")),
        ChatMessage::assistant("settled"),
        ChatMessage::user(question.clone()),
    ];
    let config = decision_sized_fixture_config(mid_turn_floor_fixture_config(
        &history,
        tool_surface,
        MID_TURN_SATURATED_SLACK_TOKENS,
    ));
    let reserved = reserve_request_tool_surface(&fixture_main_compaction(&config), tool_surface);

    let provider = RequestRecordingProvider::new(
        false,
        vec![
            prompt_guided_tool_call_response("file_read"),
            scripted_text_response("done"),
        ],
    );

    let tmp = TempDir::new().expect("test: temporary directory");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
    let mut admission_config = crate::config::Config::default();
    admission_config.workspace_dir = tmp.path().to_path_buf();
    admission_config.agent.compaction = config.clone();
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_memory(
        admission_config,
        &question,
        Arc::clone(&tools_registry),
        Arc::clone(&memory),
    )
    .await;
    admission.assert_core_specs(&tool_specs);
    let runtime = admission.memory_runtime();

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: Some(admission.compaction()),
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &HookManager::new(tmp.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: runtime,
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect("test: the turn must complete");

    assert_eq!(result, "done");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "the fixture must reach a second iteration");
    let second = requests.get(1).expect("test: the second request was recorded");
    assert!(
        !second.iter().any(|message| message.content.contains(OLDER_TURN_MARKER)),
        "premise: the window must really have saturated, or no trim ran and this fixture says \
             nothing about where a restore puts the request"
    );
    assert_the_trim_had_to_reach_the_question(second, &reserved);
    assert_eq!(
        role_sequence(second),
        vec!["system", "user", "assistant", "user"],
        "the question belongs above the round it triggered: the transcript must open on the \
             user's question, not on the assistant turn that answered it"
    );
    assert!(
        second
            .get(1)
            .is_some_and(|message| message.content.contains(QUESTION_MARKER)),
        "the `user` turn opening the transcript must be the question itself"
    );
    assert!(
        !second
            .windows(2)
            .any(|pair| pair.iter().all(|message| message.role == "user")),
        "no two adjacent `user` turns: no provider in this crate coalesces them and strict chat \
             templates reject the alternation break"
    );
}

/// The native twin. Here the round is an assistant `tool_calls` envelope
/// plus `tool` results, so the tail restore's damage is the leading
/// `assistant` turn — which the dialects that validate the opening role
/// reject — rather than an adjacent `user` pair.
#[tokio::test]
async fn a_trim_only_preflight_opens_the_native_second_request_on_the_question() {
    const QUESTION_MARKER: &str = "preflight-position-native-marker";
    const OLDER_TURN_MARKER: &str = "preflight-position-native-older-turn-marker";

    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(NamedLargeOutputTool {
        name: "file_read".to_string(),
        output: "mid turn tool output ".repeat(8),
    })];
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let tool_specs = business_tool_specs_of(&tools_registry);
    let tool_surface = crate::agent::turn_context::measure_request_tool_surface(true, &tool_specs);

    let question = format!("{QUESTION_MARKER} Use file_read. {}", "question-bulk ".repeat(1600));
    let mut history = vec![
        ChatMessage::system("stable base system prompt"),
        ChatMessage::user(format!("{OLDER_TURN_MARKER} settled earlier")),
        ChatMessage::assistant("settled"),
        ChatMessage::user(question.clone()),
    ];
    let config = decision_sized_fixture_config(mid_turn_floor_fixture_config(
        &history,
        tool_surface,
        MID_TURN_SATURATED_SLACK_TOKENS,
    ));
    let reserved = reserve_request_tool_surface(&fixture_main_compaction(&config), tool_surface);

    let provider = RequestRecordingProvider::new(
        true,
        vec![
            ChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "file_read".to_string(),
                    arguments: "{}".to_string(),
                }],
                reasoning_content: None,
            },
            scripted_text_response("done"),
        ],
    );

    let tmp = TempDir::new().expect("test: temporary directory");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("test: sqlite memory"));
    let mut admission_config = crate::config::Config::default();
    admission_config.workspace_dir = tmp.path().to_path_buf();
    admission_config.agent.compaction = config.clone();
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_memory(
        admission_config,
        &question,
        Arc::clone(&tools_registry),
        Arc::clone(&memory),
    )
    .await;
    admission.assert_core_specs(&tool_specs);
    let runtime = admission.memory_runtime();

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: Some(admission.compaction()),
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &HookManager::new(tmp.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: runtime,
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect("test: the turn must complete");

    assert_eq!(result, "done");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "the fixture must reach a second iteration");
    let second = requests.get(1).expect("test: the second request was recorded");
    assert!(
        !second.iter().any(|message| message.content.contains(OLDER_TURN_MARKER)),
        "premise: the window must really have saturated, or no trim ran and this fixture says \
             nothing about where a restore puts the request"
    );
    assert_the_trim_had_to_reach_the_question(second, &reserved);
    assert_eq!(
        role_sequence(second),
        vec!["system", "user", "assistant", "tool"],
        "the question belongs above the round it triggered: the transcript must open on the \
             user's question, not on the assistant envelope that requested the tool"
    );
    assert!(
        second
            .get(1)
            .is_some_and(|message| message.content.contains(QUESTION_MARKER)),
        "the `user` turn opening the transcript must be the question itself"
    );
    assert_eq!(
        first_orphan_tool_index(second),
        None,
        "moving the request off the tail must not strand a tool result from its envelope"
    );
}

#[tokio::test]
async fn preflight_compacts_before_provider_call_when_over_hard_limit() {
    let first_call_tokens = Arc::new(AtomicUsize::new(0));
    let provider = RecordingBudgetProvider {
        first_call_tokens: Arc::clone(&first_call_tokens),
    };
    let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
    let observer = NoopObserver;
    let hooks = HookManager::new(std::env::temp_dir());
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Aggressive,
        reserve_tokens: 10,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: 4106,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let mut history = vec![ChatMessage::system("sys")];
    for i in 0..24 {
        history.push(ChatMessage::user(format!("turn {i} {}", "long context ".repeat(4096))));
    }

    let available = config.max_context_tokens.checked_sub(config.reserve_tokens).unwrap();
    let fixture_main_window = available.checked_sub((available / 16).min(4096)).unwrap();
    assert_eq!(config.reserve_tokens, 10);
    assert!((available / 16).min(4096) >= 128);
    assert!(budgeted_tokens(estimate_history_tokens_with_source(&history)) > fixture_main_window);

    let workspace = TempDir::new().expect("budget fixture workspace");
    let mut admission_config = crate::config::Config::default();
    admission_config.workspace_dir = workspace.path().to_path_buf();
    admission_config.agent.compaction = config.clone();
    let tools_registry = Arc::new(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_config(
        admission_config,
        &history.last().expect("current fixture input").content,
        Arc::clone(&tools_registry),
    )
    .await;
    assert_eq!(
        admission
            .compaction()
            .max_context_tokens
            .checked_sub(admission.compaction().reserve_tokens),
        Some(fixture_main_window),
    );
    admission.assert_selected_specs(&[]);

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: Some(admission.compaction()),
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &observer,
            hooks: &hooks,
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect("preflight should keep provider call within budget");

    assert_eq!(result, "done");
    // Changed with the current-request floor. One fixture turn is larger
    // than the main window on its own, and the last of them is
    // this turn's request: the preflight frees everything else and keeps
    // it, so the bound is the window plus exactly that request instead of
    // the window alone.
    let current_request_tokens = budgeted_tokens(estimate_history_tokens_with_source(&[ChatMessage::user(format!(
        "turn 23 {}",
        "long context ".repeat(4096)
    ))]));
    assert!(current_request_tokens > fixture_main_window);
    assert!(
        !history.iter().any(|message| message.content.starts_with("turn 0 ")),
        "the older turns must still be compacted away before the provider is reached"
    );
    assert!(
        history.iter().any(|message| message.content.starts_with("turn 23 ")),
        "the current turn's request must reach the provider"
    );
    assert!(
        first_call_tokens.load(Ordering::SeqCst) <= fixture_main_window.saturating_add(current_request_tokens),
        "first provider call must be the main hard limit plus at most the current request"
    );
}

#[tokio::test]
async fn legacy_preflight_provider_history_keeps_real_user_question_last_after_compaction() {
    let current_question = "What should ISS-037 answer now?";
    let captured_history = Arc::new(Mutex::new(None));
    let provider = CapturingCompactionProvider {
        captured_history: Arc::clone(&captured_history),
    };
    let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
    let observer = NoopObserver;
    let hooks = HookManager::new(std::env::temp_dir());
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 10,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 4106,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let mut history = vec![ChatMessage::system("sys")];
    for i in 0..24 {
        history.push(ChatMessage::user(format!(
            "old turn {i} {}",
            "long context ".repeat(400)
        )));
    }
    history.push(ChatMessage::user(current_question));

    let workspace = TempDir::new().expect("budget fixture workspace");
    let mut admission_config = crate::config::Config::default();
    admission_config.workspace_dir = workspace.path().to_path_buf();
    admission_config.agent.compaction = config.clone();
    let tools_registry = Arc::new(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_config(
        admission_config,
        &history.last().expect("current fixture input").content,
        Arc::clone(&tools_registry),
    )
    .await;
    admission.assert_selected_specs(&[]);

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: Some(admission.compaction()),
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &observer,
            hooks: &hooks,
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect("legacy provider call should complete");

    let captured = captured_history
        .lock()
        .expect("captured history lock should be valid")
        .clone()
        .expect("provider-bound history should be captured");
    let last = captured.last().expect("captured history should not be empty");
    assert_eq!(last.role, "user");
    assert_eq!(last.content, current_question);
    let refresh_index = captured
        .iter()
        .position(|message| message.content.starts_with("[Post-compaction context refresh]"))
        .expect("refresh marker should be present");
    assert!(
        refresh_index + 1 < captured.len(),
        "refresh marker must not be the trailing provider-bound message"
    );
    assert!(
        result.contains(current_question),
        "assistant response must be bound to the real current user question"
    );
}

#[tokio::test]
async fn off_mode_preflight_trims_without_silent_summary() {
    let first_call_tokens = Arc::new(AtomicUsize::new(0));
    let provider = RecordingBudgetProvider {
        first_call_tokens: Arc::clone(&first_call_tokens),
    };
    let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
    let observer = NoopObserver;
    let hooks = HookManager::new(std::env::temp_dir());
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        reserve_tokens: 10,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: 4106,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let mut history = vec![ChatMessage::system("sys")];
    for i in 0..24 {
        history.push(ChatMessage::user(format!(
            "turn {i} {}",
            "long context ".repeat(if i == 23 { 40 } else { 400 })
        )));
    }

    let available = config.max_context_tokens.checked_sub(config.reserve_tokens).unwrap();
    let fixture_main_window = available.checked_sub((available / 16).min(4096)).unwrap();
    assert_eq!(config.reserve_tokens, 10);
    assert!((available / 16).min(4096) >= 128);
    assert!(budgeted_tokens(estimate_history_tokens_with_source(&history)) > fixture_main_window);
    let current_request_tokens = budgeted_tokens(estimate_history_tokens_with_source(&[history
        .last()
        .expect("current request")
        .clone()]));
    assert!(current_request_tokens <= fixture_main_window);

    let workspace = TempDir::new().expect("budget fixture workspace");
    let mut admission_config = crate::config::Config::default();
    admission_config.workspace_dir = workspace.path().to_path_buf();
    admission_config.agent.compaction = config.clone();
    let tools_registry = Arc::new(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_config(
        admission_config,
        &history.last().expect("current fixture input").content,
        Arc::clone(&tools_registry),
    )
    .await;
    assert_eq!(
        admission
            .compaction()
            .max_context_tokens
            .checked_sub(admission.compaction().reserve_tokens),
        Some(fixture_main_window),
    );
    admission.assert_selected_specs(&[]);

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: Some(admission.compaction()),
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &observer,
            hooks: &hooks,
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect("off-mode preflight should trim without summary");

    assert_eq!(result, "done");
    assert!(
        first_call_tokens.load(Ordering::SeqCst) <= fixture_main_window,
        "off-mode first provider call must be below the main hard limit"
    );
    assert!(
        history
            .iter()
            .all(|message| !message.content.contains("[Context compacted at")),
        "Off mode must not silently insert a compaction summary"
    );
}
