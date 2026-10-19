#![cfg(test)]
//! Same-window allocation phase diagnostics; no formal performance acceptance.
#![allow(clippy::expect_used)] // Owned diagnostic fixtures fail loudly on invalid evidence.
use super::*;
use crate::decision::builtin::allocation_counter::{
    CheckpointError, ProcessCheckpoint, capture_process_checkpoint, count_process_future,
};

#[derive(Clone, serde::Serialize)]
struct PhasePoint {
    boundary: &'static str,
    allocation_counter_generation: u64,
    rust_allocation_attempts: u64,
    elapsed_us: u128,
}

#[derive(Clone, serde::Serialize)]
struct ActorAcknowledgment {
    actor: &'static str,
    succeeded: bool,
}

#[derive(Clone, serde::Serialize)]
struct PhaseTrace {
    allocation_counter_generation: u64,
    points: [PhasePoint; 4],
    acknowledgments: [ActorAcknowledgment; 2],
    final_rust_allocation_attempts: u64,
    final_elapsed_us: u128,
    unattributed_final_tail: u64,
}

#[derive(Clone, serde::Serialize)]
struct SourceBinding {
    workspace_id: String,
    owner_id: String,
    session_key: String,
    run_id: String,
    source_event_id: String,
    config_generation_id: u64,
    config_source_revision: String,
}

fn trace_matches(trace: &PhaseTrace, mode: &str, expected_generation: u64) -> bool {
    let second = if mode == "decision" {
        "decide_return"
    } else if mode == "idle" {
        "no_decision"
    } else {
        return false;
    };
    let names = ["capture", second, "decision_writer_ack", "causal_event_ack"];
    trace.allocation_counter_generation == expected_generation
        && expected_generation > 0
        && trace
            .points
            .iter()
            .zip(names)
            .all(|(point, name)| point.boundary == name && point.allocation_counter_generation == expected_generation)
        && trace
            .points
            .iter()
            .zip(trace.points.iter().skip(1))
            .all(|(earlier, later)| {
                earlier.rust_allocation_attempts <= later.rust_allocation_attempts
                    && earlier.elapsed_us <= later.elapsed_us
            })
        && trace.acknowledgments[0].actor == "decision_writer"
        && trace.acknowledgments[0].succeeded
        && trace.acknowledgments[1].actor == "causal_events"
        && trace.acknowledgments[1].succeeded
        && trace.points[3].rust_allocation_attempts <= trace.final_rust_allocation_attempts
        && trace.points[3].elapsed_us <= trace.final_elapsed_us
        && trace.unattributed_final_tail
            == trace
                .final_rust_allocation_attempts
                .saturating_sub(trace.points[3].rust_allocation_attempts)
}

fn binding_matches(binding: &SourceBinding, proof: &AgentTurnProof) -> bool {
    binding.workspace_id == proof.workspace_id()
        && binding.owner_id == proof.owner_id()
        && binding.session_key == proof.session_key()
        && binding.run_id == proof.run_id()
        && Some(binding.source_event_id.as_str()) == proof.source_event_id()
        && binding.config_generation_id == proof.config_generation_id()
        && binding.config_source_revision == proof.config_source_revision()
}

async fn acknowledged_phases(
    service: &TurnDecisionService,
    token: &ProcessCheckpoint<'_>,
    started: Instant,
) -> ([u64; 2], [u128; 2]) {
    // One unchanged deadline; event drain must follow actual writer acknowledgment.
    let deadline = Instant::now() + Duration::from_secs(5);
    assert!(
        service
            .store
            .writer()
            .expect("real decision writer")
            .drain(deadline)
            .await
    );
    let writer = token
        .read()
        .expect("same active allocation window after writer acknowledgment");
    let writer_us = started.elapsed().as_micros();
    assert!(
        service
            .durable_events
            .as_ref()
            .expect("real durable causal sink")
            .drain(deadline)
            .await
    );
    let events = token
        .read()
        .expect("same active allocation window after event acknowledgment");
    let events_us = started.elapsed().as_micros();
    ([writer, events], [writer_us, events_us])
}

fn run_trace_counterfactuals(trace: &PhaseTrace, mode: &str, binding: &SourceBinding, proof: &AgentTurnProof) -> usize {
    let generation = trace.allocation_counter_generation;
    assert!(trace_matches(trace, mode, generation));
    assert!(binding_matches(binding, proof));
    let mut altered = trace.clone();
    altered.allocation_counter_generation += 1;
    assert!(!trace_matches(&altered, mode, generation));
    altered = trace.clone();
    altered.points[1].allocation_counter_generation += 1;
    assert!(!trace_matches(&altered, mode, generation));
    altered = trace.clone();
    altered.points[1].rust_allocation_attempts = trace.final_rust_allocation_attempts + 1;
    assert!(!trace_matches(&altered, mode, generation));
    altered = trace.clone();
    altered.points[1].elapsed_us = trace.final_elapsed_us + 1;
    assert!(!trace_matches(&altered, mode, generation));
    altered = trace.clone();
    altered.points[3].rust_allocation_attempts = trace.final_rust_allocation_attempts + 1;
    assert!(!trace_matches(&altered, mode, generation));
    altered = trace.clone();
    altered.acknowledgments[0].succeeded = false;
    assert!(!trace_matches(&altered, mode, generation));
    altered = trace.clone();
    altered.acknowledgments[1].succeeded = false;
    assert!(!trace_matches(&altered, mode, generation));
    altered = trace.clone();
    altered.acknowledgments.swap(0, 1);
    assert!(!trace_matches(&altered, mode, generation));
    altered = trace.clone();
    altered.points[2].boundary = "missing_actor";
    assert!(!trace_matches(&altered, mode, generation));
    altered = trace.clone();
    altered.unattributed_final_tail += 1;
    assert!(!trace_matches(&altered, mode, generation));
    let mut wrong_binding = binding.clone();
    wrong_binding.owner_id.push_str("_other");
    assert!(!binding_matches(&wrong_binding, proof));
    wrong_binding = binding.clone();
    wrong_binding.source_event_id.push_str("_other");
    assert!(!binding_matches(&wrong_binding, proof));
    assert!(trace_matches(trace, mode, generation));
    assert!(binding_matches(binding, proof));
    12
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "owned phase diagnostics require explicit fresh-process fixture parameters"]
#[allow(clippy::print_stdout)] // Raw inclusive phases and controls are preserved evidence.
async fn actual_builtin_phase_characterization() {
    let extra: usize = std::env::var("PRX_CDP_PERF_EXTRA")
        .expect("explicit N")
        .parse()
        .expect("numeric N");
    assert!([0, 1, 16, 64, 256, 512].contains(&extra));
    let class = std::env::var("PRX_CDP_PERF_CASE").expect("explicit fixture class");
    assert!(["small", "large-raw-state", "max-native-cap32"].contains(&class.as_str()));
    let reverse = std::env::var("PRX_CDP_PERF_REVERSE").is_ok_and(|v| v == "1");
    let workspace = tempfile::tempdir().expect("isolated workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_path_buf();
    config.causal_tree.enabled = true;
    config.causal_tree.policy.extra_latency_budget_ms = 5000;
    config.causal_tree.policy.max_decision_hops = 4;
    config.causal_tree.policy.max_candidates = if class == "max-native-cap32" { 32 } else { 12 };
    let profile = std::env::var("PRX_CDP_PERF_PROFILE").expect("explicit recording profile");
    assert!(["default-recording", "retain-projection"].contains(&profile.as_str()));
    config.causal_tree.store.record_redacted_projection = profile == "retain-projection";
    config.providers.models.push(
        toml::from_str(
            "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .expect("verified local model fixture"),
    );
    let generation = crate::config::new_shared(config.clone()).pin();
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut direct = BTreeSet::new();
    let mut tools = Vec::new();
    if class == "max-native-cap32" {
        for (name, categories) in [
            ("file_read", &[ToolCategory::FileSystem][..]),
            ("skills_list", &[][..]),
            ("memory_search", &[ToolCategory::Memory][..]),
            ("web_search_tool", &[ToolCategory::WebBrowsing][..]),
            ("sessions_history", &[ToolCategory::Communication][..]),
        ] {
            direct.insert(name.to_owned());
            tools.push(tool(name, categories, &dispatches));
        }
    } else {
        direct.insert("file_read".to_owned());
        tools.push(tool("file_read", &[], &dispatches));
    }
    let fixed_count = tools.len() + 1;
    tools.push(tool("image_info", &[], &dispatches));
    for n in 0..extra {
        tools.push(tool(&format!("prx_perf_{n:04}"), &[], &dispatches));
    }
    if reverse {
        tools.reverse();
    }
    let registry = Arc::new(tools);
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let provider_catalog = ProviderCatalogView::from_config(&config);
    let transports = CountingTransports(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("actual SQLite"));
    let service = build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry,
            execution,
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy {
                allowed_tools: None,
                direct_names: direct,
                ..SurfacePolicy::default()
            },
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security,
        },
    )
    .await
    .expect("actual source-owned builtin service");
    let mut input = if class == "max-native-cap32" {
        "inspect file_read skills_list memory_search web_search_tool sessions_history".to_owned()
    } else {
        "inspect file_read project evidence".to_owned()
    };
    if class == "large-raw-state" {
        while input.len() < 16384 {
            input.push_str(" neutral evidence;");
        }
        input.truncate(16384);
    }
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
        &generation,
        Arc::clone(&memory),
        &input,
        "prx_perf_run",
    )
    .await;
    drain_required(&service).await;
    let cancellation = CancellationToken::new();
    let mode = std::env::var("PRX_CDP_PHASE_MODE").expect("explicit separate decision or idle invocation");
    assert!(["decision", "idle"].contains(&mode.as_str()));
    let principal = crate::memory::MemoryPrincipal {
        workspace_id: proof.workspace_id().to_owned(),
        session_key: Some(proof.session_key().to_owned()),
        owner_id: Some(proof.owner_id().to_owned()),
        channel: proof.memory_scope().channel.clone(),
        sender: proof.memory_scope().sender_id.clone(),
        require_owner_match: proof.memory_scope().require_owner_match,
        ..crate::memory::MemoryPrincipal::default()
    };
    // Both modes have the same explicit baseline-read preparation. These are
    // instrumented diagnostics, not the unchanged fixture's first-decision metric.
    let before_events = memory
        .list_message_events_since(&principal, 0, 4096)
        .await
        .expect("owned pre-window event baseline");
    assert!(before_events.len() < 4096, "baseline must not be truncated");
    assert!(
        before_events
            .iter()
            .any(|event| Some(event.event_id.as_str()) == proof.source_event_id()),
        "genuine recorded source exists"
    );
    let connection =
        rusqlite::Connection::open_with_flags(service.store.path(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("owned read-only decision baseline");
    // Decision rows use the actual store UUID, not the ingress workspace label.
    let store_workspace = service.store.workspace_id().expect("actual decision store workspace");
    let count_decisions = || {
        let count = connection
            .query_row(
                "SELECT count(*) FROM decisions WHERE workspace_id=?1 AND owner_id=?2 AND session_key=?3 AND run_id=?4",
                rusqlite::params![
                    store_workspace.as_ref(),
                    proof.owner_id(),
                    proof.session_key(),
                    proof.run_id()
                ],
                |row| row.get::<_, i64>(0),
            )
            .expect("real run-scoped decision count");
        u64::try_from(count).expect("nonnegative actual decision count")
    };
    assert_eq!(
        count_decisions(),
        0,
        "fresh fixture has no decision rows before measurement"
    );
    let (((context, token, counts, times, started), original_material), total) = count_process_future(async {
        let started = Instant::now();
        let prepared = async {
            let (token, capture) = capture_process_checkpoint().expect("active owned process allocation window");
            let capture_us = started.elapsed().as_micros();
            let context = if mode == "decision" {
                Some(
                    service
                        .decide_agent_initial(&proof, AgentTurnKind::Repl, &cancellation)
                        .await
                        .expect("actual builtin decision"),
                )
            } else {
                None
            };
            let returned = token
                .read()
                .expect("same window at actual return or explicit idle boundary");
            let returned_us = started.elapsed().as_micros();
            let (counts, times) = acknowledged_phases(&service, &token, started).await;
            (
                context,
                token,
                [capture, returned, counts[0], counts[1]],
                [capture_us, returned_us, times[0], times[1]],
                started,
            )
        };
        let (output, captured) = crate::causal_tree::decision_engine::original_observation::material::capture(
            "same-invocation-phase",
            prepared,
        )
        .await
        .expect("bounded same-invocation phase material");
        let hops = output
            .0
            .as_ref()
            .map(|context| context.audit_hops().collect::<Vec<_>>())
            .unwrap_or_default();
        let material = captured
            .finish_material(&hops, mode != "decision")
            .expect("complete original phase material inside measured window");
        drop(hops);
        (output, material)
    })
    .await
    .expect("isolated inclusive process window");
    let final_elapsed_us = started.elapsed().as_micros();
    println!(
        "PRX_ORIGINAL_MATERIAL {}",
        std::str::from_utf8(&original_material).expect("encoded material JSON")
    );
    assert_eq!(
        token.read(),
        Err(CheckpointError::Inactive),
        "wrapper closes original window"
    );
    assert!(total >= counts[3], "global total retains the trailing interval");
    let allocation_generation = token.allocation_generation();
    let second = if mode == "decision" {
        "decide_return"
    } else {
        "no_decision"
    };
    let names = ["capture", second, "decision_writer_ack", "causal_event_ack"];
    let trace = PhaseTrace {
        allocation_counter_generation: allocation_generation,
        points: std::array::from_fn(|index| PhasePoint {
            boundary: names.get(index).expect("fixed four phase names"),
            allocation_counter_generation: allocation_generation,
            rust_allocation_attempts: *counts.get(index).expect("fixed four phase counts"),
            elapsed_us: *times.get(index).expect("fixed four phase times"),
        }),
        acknowledgments: [
            ActorAcknowledgment {
                actor: "decision_writer",
                succeeded: true,
            },
            ActorAcknowledgment {
                actor: "causal_events",
                succeeded: true,
            },
        ],
        final_rust_allocation_attempts: total,
        final_elapsed_us,
        unattributed_final_tail: total - counts[3],
    };
    let binding = SourceBinding {
        workspace_id: proof.workspace_id().to_owned(),
        owner_id: proof.owner_id().to_owned(),
        session_key: proof.session_key().to_owned(),
        run_id: proof.run_id().to_owned(),
        source_event_id: proof.source_event_id().expect("genuine source row").to_owned(),
        config_generation_id: proof.config_generation_id(),
        config_source_revision: proof.config_source_revision().to_owned(),
    };
    assert!(trace_matches(&trace, &mode, allocation_generation));
    assert!(binding_matches(&binding, &proof));
    let after_events = memory
        .list_message_events_since(&principal, 0, 4096)
        .await
        .expect("real durable event readback");
    assert!(after_events.len() < 4096, "event readback must not be truncated");
    let committed = crate::decision::telemetry::CausalEventKind::DecisionCommitted.as_str();
    let incomplete = crate::decision::telemetry::CausalEventKind::OutcomeIncomplete.as_str();
    assert!(
        !after_events.iter().any(|event| event.event_type == incomplete),
        "incomplete durable evidence invalidates diagnostic"
    );
    let semantics = context.as_ref().map_or_else(
        || {
            assert_eq!(count_decisions(), 0, "idle control creates no decision rows");
            assert_eq!(
                serde_json::to_value(&after_events).expect("idle after rows"),
                serde_json::to_value(&before_events).expect("idle baseline rows"),
                "idle control creates or changes no durable events"
            );
            None
        },
        |context| {
            let actual = semantic_context(&service, &proof, context);
            let expected_ids: BTreeSet<String> = context.audit_hops().map(|hop| hop.decision_id.to_string()).collect();
            assert_eq!(
                count_decisions(),
                u64::try_from(expected_ids.len()).expect("bounded actual chain")
            );
            let mut committed_ids = BTreeSet::new();
            let mut committed_count = 0;
            for event in after_events.iter().filter(|event| event.event_type == committed) {
                assert_eq!(event.workspace_id, proof.workspace_id());
                assert_eq!(event.owner_id.as_deref(), Some(proof.owner_id()));
                assert_eq!(event.session_key.as_deref(), Some(proof.session_key()));
                assert_eq!(event.run_id.as_deref(), Some(proof.run_id()));
                assert_eq!(event.config_generation_id, Some(proof.config_generation_id()));
                assert_eq!(
                    event.config_source_revision.as_deref(),
                    Some(proof.config_source_revision())
                );
                let payload: Value =
                    serde_json::from_str(event.raw_payload_json.as_deref().expect("actual causal payload"))
                        .expect("valid stored payload");
                assert_eq!(
                    payload.get("source_event_id").and_then(Value::as_str),
                    proof.source_event_id()
                );
                assert!(payload.get("incomplete").is_none_or(Value::is_null));
                let id = event
                    .correlation_id
                    .as_ref()
                    .expect("actual committed decision linkage");
                assert!(expected_ids.contains(id));
                assert!(
                    committed_ids.insert(id.clone()),
                    "exactly one committed event per real decision"
                );
                committed_count += 1;
            }
            assert_eq!(committed_ids, expected_ids);
            assert_eq!(committed_count, context.audit_hops().count());
            Some(actual)
        },
    );
    assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    let mutations = if std::env::var("PRX_CDP_PHASE_MUTATIONS").is_ok_and(|value| value == "1") {
        run_trace_counterfactuals(&trace, &mode, &binding, &proof)
    } else {
        0
    };
    println!(
        "PRX_CDP_PHASE_CHARACTERIZATION {}",
        json!({"kind":"actual-builtin-phase-diagnostic-v1","scope":"instrumented initialized/drained and baseline-read setup; all-thread Rust attempts, same-window inclusive total; no formalH4/A0/P0 or exclusive actor attribution","mode":mode,"class":class,"recording_profile":profile,"extra_inventory":extra,"reverse_registry":reverse,"fixed_capabilities":fixed_count,"raw_input_bytes":input.len(),"configured_candidate_cap":config.causal_tree.policy.max_candidates,"process_id":std::process::id(),"source_binding":binding,"semantics":semantics,"event_rows_before":before_events.len(),"event_rows_after":after_events.len(),"process_pilot":{"rust_allocation_attempts":total,"allocation_phase_trace":trace},"counterfactual_mutations":mutations})
    );
}
