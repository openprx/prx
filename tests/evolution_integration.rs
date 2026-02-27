use anyhow::Result;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tempfile::tempdir;
use openprx::self_system::evolution::{
    with_trace, Actor, AnnotationPipeline, AsyncJsonlWriter, CandidatePriority, ChangeType,
    CircuitBreaker, CircuitBreakerState, DataBasis, DecisionLog, DecisionType, EngineCycleInput,
    EvolutionAnalyzer, EvolutionCandidate, EvolutionConfig, EvolutionEngine, EvolutionLayer,
    EvolutionLog, EvolutionMode, EvolutionPipeline, EvolutionResult, EvolutionTrigger,
    JsonlRetentionPolicy, JsonlStoragePaths, JsonlToSqliteIndexer, MemoryAccessLog, MemoryAction,
    MemoryEvolutionEngine, Outcome, RollbackManager, TaskType,
};
use openprx::self_system::orchestrator::{
    ChangeOperation, ChangeTarget, CycleOutcome, EvolutionCycle, EvolutionProposal,
    EvolutionSignals, EvolutionValidation, FitnessTrend, RiskLevel, ValidationStatus,
};

fn base_config(storage_root: &std::path::Path) -> EvolutionConfig {
    let mut cfg = EvolutionConfig::default();
    cfg.runtime.storage_dir = storage_root.to_string_lossy().to_string();
    cfg.runtime.batch_size = 1;
    cfg
}

fn sample_decision(trace_id: &str, experiment_id: &str, ts: &str, outcome: Outcome) -> DecisionLog {
    DecisionLog {
        timestamp: ts.to_string(),
        experiment_id: experiment_id.to_string(),
        trace_id: trace_id.to_string(),
        decision_type: DecisionType::RuntimePolicy,
        task_type: TaskType::Planning,
        risk_level: 2,
        actor: Actor::Agent,
        input_context: "ctx".to_string(),
        action_taken: "act".to_string(),
        outcome,
        tokens_used: 32,
        latency_ms: 8,
        user_correction: None,
        config_snapshot_hash: "cfg-a".to_string(),
    }
}

fn sample_memory(
    trace_id: &str,
    experiment_id: &str,
    ts: &str,
    useful: Option<bool>,
) -> MemoryAccessLog {
    MemoryAccessLog {
        timestamp: ts.to_string(),
        experiment_id: experiment_id.to_string(),
        trace_id: trace_id.to_string(),
        action: MemoryAction::Read,
        memory_id: "mem-1".to_string(),
        task_context: "ctx".to_string(),
        task_type: TaskType::Planning,
        actor: Actor::Agent,
        was_useful: useful,
        useful_annotation_source: None,
        annotation_confidence: None,
        tokens_consumed: 16,
    }
}

async fn seed_three_day_digests(analyzer: &EvolutionAnalyzer, now: chrono::DateTime<Utc>) {
    let _ = analyzer
        .generate_daily_digest(now - Duration::days(2))
        .await;
    let _ = analyzer
        .generate_daily_digest(now - Duration::days(1))
        .await;
}

#[tokio::test]
async fn trace_id_end_to_end_pipeline_to_evolution_log() -> Result<()> {
    let dir = tempdir()?;
    let storage_root = dir.path().join("logs");
    let cfg_path = dir.path().join("evolution_config.toml");

    let mut cfg = base_config(&storage_root);
    cfg.runtime.mode = EvolutionMode::Shadow;
    tokio::fs::write(&cfg_path, toml::to_string_pretty(&cfg)?).await?;

    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            JsonlRetentionPolicy::default(),
            1,
        )
        .await?,
    );

    let ctx = openprx::self_system::evolution::TraceContext::new();
    let now = Utc::now();
    with_trace(ctx.clone(), || async {
        let ts = now.to_rfc3339();
        let decision = sample_decision(&ctx.trace_id, &ctx.experiment_id, &ts, Outcome::Failure);
        let memory = sample_memory(&ctx.trace_id, &ctx.experiment_id, &ts, Some(false));
        writer.append_decision(&decision).await.unwrap();
        writer.append_memory_access(&memory).await.unwrap();
        writer.flush().await.unwrap();
    })
    .await;

    let analyzer = Arc::new(EvolutionAnalyzer::new(
        writer.clone(),
        storage_root.join("analysis"),
    ));
    seed_three_day_digests(analyzer.as_ref(), now).await;

    let shared = openprx::self_system::evolution::new_shared_evolution_config(cfg);
    let mut pipeline = EvolutionPipeline::new(shared.clone(), analyzer, writer.clone(), dir.path());
    let mut engine = MemoryEvolutionEngine::new(shared, &cfg_path, Some(writer));

    let report = pipeline
        .run_for_layer(
            EvolutionTrigger::Manual,
            EvolutionLayer::Memory,
            &mut engine,
            now,
        )
        .await?;

    assert!(report.selected_candidate.is_some());
    assert_eq!(
        report
            .evolution_log
            .as_ref()
            .map(|item| item.experiment_id.as_str()),
        Some(report.experiment_id.as_str())
    );

    let candidate = report.selected_candidate.expect("candidate must exist");
    assert!(candidate.evidence_ids.iter().any(|id| id == &ctx.trace_id));
    Ok(())
}

#[tokio::test]
async fn record_analyze_evolve_closed_loop_produces_candidate() -> Result<()> {
    let dir = tempdir()?;
    let storage_root = dir.path().join("logs");
    let cfg_path = dir.path().join("evolution_config.toml");

    let mut cfg = base_config(&storage_root);
    cfg.runtime.mode = EvolutionMode::Auto;
    tokio::fs::write(&cfg_path, toml::to_string_pretty(&cfg)?).await?;

    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            JsonlRetentionPolicy::default(),
            1,
        )
        .await?,
    );

    let ts = Utc::now().to_rfc3339();
    writer
        .append_decision(&sample_decision("trace-1", "exp-1", &ts, Outcome::Failure))
        .await?;
    writer
        .append_memory_access(&sample_memory("trace-1", "exp-1", &ts, Some(false)))
        .await?;
    writer.flush().await?;

    let analyzer = Arc::new(EvolutionAnalyzer::new(
        writer.clone(),
        storage_root.join("analysis"),
    ));
    seed_three_day_digests(analyzer.as_ref(), Utc::now()).await;

    let shared = openprx::self_system::evolution::new_shared_evolution_config(cfg);
    let mut pipeline = EvolutionPipeline::new(shared.clone(), analyzer, writer.clone(), dir.path());
    let mut engine = MemoryEvolutionEngine::new(shared, &cfg_path, Some(writer));

    let report = pipeline
        .run_for_layer(
            EvolutionTrigger::Manual,
            EvolutionLayer::Memory,
            &mut engine,
            Utc::now(),
        )
        .await?;

    assert!(report.selected_candidate.is_some());
    Ok(())
}

#[tokio::test]
async fn shadow_mode_generates_recommendation_without_applying_change() -> Result<()> {
    let dir = tempdir()?;
    let storage_root = dir.path().join("logs");
    let cfg_path = dir.path().join("evolution_config.toml");

    let mut cfg = base_config(&storage_root);
    cfg.runtime.mode = EvolutionMode::Shadow;
    let original = toml::to_string_pretty(&cfg)?;
    tokio::fs::write(&cfg_path, &original).await?;

    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            JsonlRetentionPolicy::default(),
            1,
        )
        .await?,
    );

    let ts = Utc::now().to_rfc3339();
    writer
        .append_decision(&sample_decision("trace-2", "exp-2", &ts, Outcome::Failure))
        .await?;
    writer
        .append_memory_access(&sample_memory("trace-2", "exp-2", &ts, Some(false)))
        .await?;
    writer.flush().await?;

    let analyzer = Arc::new(EvolutionAnalyzer::new(
        writer.clone(),
        storage_root.join("analysis"),
    ));
    seed_three_day_digests(analyzer.as_ref(), Utc::now()).await;

    let shared = openprx::self_system::evolution::new_shared_evolution_config(cfg);
    let mut pipeline = EvolutionPipeline::new(shared.clone(), analyzer, writer.clone(), dir.path());
    let mut engine = MemoryEvolutionEngine::new(shared, &cfg_path, Some(writer));

    let report = pipeline
        .run_for_layer(
            EvolutionTrigger::Manual,
            EvolutionLayer::Memory,
            &mut engine,
            Utc::now(),
        )
        .await?;

    let after = tokio::fs::read_to_string(&cfg_path).await?;
    assert_eq!(after, original);
    assert!(report.shadow_mode);
    assert!(report.selected_candidate.is_some());
    Ok(())
}

#[tokio::test]
async fn gate_rejects_candidate_when_threshold_not_met() -> Result<()> {
    let dir = tempdir()?;
    let storage_root = dir.path().join("logs");
    let cfg_path = dir.path().join("evolution_config.toml");

    let mut cfg = base_config(&storage_root);
    cfg.runtime.mode = EvolutionMode::Auto;
    cfg.gate.min_improvement = 1.0;
    tokio::fs::write(&cfg_path, toml::to_string_pretty(&cfg)?).await?;

    let shared = openprx::self_system::evolution::new_shared_evolution_config(cfg);
    let mut engine = MemoryEvolutionEngine::new(shared, &cfg_path, None);

    let mut target = BTreeMap::new();
    target.insert("task_type".to_string(), "planning".to_string());
    let result = engine
        .run_cycle(EngineCycleInput {
            cycle_id: "cycle-gate-reject".to_string(),
            analyzer_candidates: vec![EvolutionCandidate {
                target,
                current_value: "failure_rate=0.6".to_string(),
                suggested_value: "increase_validation".to_string(),
                evidence_ids: vec!["trace-gate".to_string()],
                priority: CandidatePriority::High,
                backfill_after_days: 3,
            }],
        })
        .await?;

    assert!(matches!(result.cycle.outcome, CycleOutcome::Failed));
    assert!(matches!(
        result.evolution_log.as_ref().and_then(|v| v.result.clone()),
        Some(EvolutionResult::Rejected)
    ));
    Ok(())
}

struct RollbackTriggerEngine {
    target_path: std::path::PathBuf,
}

#[async_trait]
impl EvolutionEngine for RollbackTriggerEngine {
    fn name(&self) -> &'static str {
        "rollback_trigger_engine"
    }

    fn layer(&self) -> EvolutionLayer {
        EvolutionLayer::Memory
    }

    async fn run_cycle(
        &mut self,
        input: EngineCycleInput,
    ) -> Result<openprx::self_system::evolution::CycleResult> {
        let proposal = EvolutionProposal {
            id: "proposal-rb".to_string(),
            summary: "force rollback".to_string(),
            rationale: "low judge score path".to_string(),
            risk_level: RiskLevel::Low,
            target: ChangeTarget::ConfigFile {
                path: self.target_path.to_string_lossy().to_string(),
            },
            operation: ChangeOperation::Write {
                content: "value = 99".to_string(),
            },
        };

        Ok(openprx::self_system::evolution::CycleResult {
            layer: EvolutionLayer::Memory,
            proposal: Some(proposal),
            cycle: EvolutionCycle {
                id: input.cycle_id,
                started_at: Utc::now().to_rfc3339(),
                finished_at: Utc::now().to_rfc3339(),
                signals: EvolutionSignals {
                    memory_count: 1,
                    health_components: 1,
                    health_error_components: 0,
                    cron_runs: 0,
                    cron_failure_ratio: 0.0,
                },
                trend: FitnessTrend {
                    window: 1,
                    previous_average: 0.5,
                    latest_score: 0.4,
                    is_declining: true,
                },
                proposal: None,
                validation: EvolutionValidation {
                    status: ValidationStatus::Regressed,
                    before_score: 0.5,
                    after_score: 0.4,
                    delta: -0.1,
                    notes: "unsafe violation timeout".to_string(),
                },
                outcome: CycleOutcome::Applied,
                alert: None,
                errors: Vec::new(),
            },
            evolution_log: Some(EvolutionLog {
                experiment_id: "placeholder".to_string(),
                timestamp: Utc::now().to_rfc3339(),
                layer: EvolutionLayer::Memory,
                change_type: ChangeType::Tune,
                before_value: "value=1".to_string(),
                after_value: "value=2".to_string(),
                trigger_reason: "rollback test".to_string(),
                data_basis: DataBasis {
                    sample_count: 1,
                    time_range_days: 1,
                    key_metrics: HashMap::new(),
                    patterns_found: vec!["pattern".to_string()],
                },
                result: None,
            }),
            needs_human_approval: false,
            shadow_mode: false,
        })
    }
}

#[tokio::test]
async fn rollback_triggers_when_judge_score_below_threshold() -> Result<()> {
    let dir = tempdir()?;
    let storage_root = dir.path().join("logs");
    let target = dir.path().join("target.toml");
    tokio::fs::write(&target, "value = 1\n").await?;

    let rollback_dir = dir.path().join(".evolution/rollback/memory");
    let manager = RollbackManager::new(&target, &rollback_dir, 5);
    let _ = manager.backup_current_version().await?;
    tokio::fs::write(&target, "value = 2\n").await?;

    let cfg = base_config(&storage_root);
    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            JsonlRetentionPolicy::default(),
            1,
        )
        .await?,
    );

    let ts = Utc::now().to_rfc3339();
    writer
        .append_decision(&sample_decision(
            "trace-rb",
            "exp-rb",
            &ts,
            Outcome::Failure,
        ))
        .await?;
    writer
        .append_memory_access(&sample_memory("trace-rb", "exp-rb", &ts, Some(false)))
        .await?;
    writer.flush().await?;

    let analyzer = Arc::new(EvolutionAnalyzer::new(
        writer.clone(),
        storage_root.join("analysis"),
    ));
    seed_three_day_digests(analyzer.as_ref(), Utc::now()).await;

    let shared = openprx::self_system::evolution::new_shared_evolution_config(cfg);
    let mut pipeline = EvolutionPipeline::new(shared, analyzer, writer, dir.path());
    let mut engine = RollbackTriggerEngine {
        target_path: target.clone(),
    };

    let report = pipeline
        .run_for_layer(
            EvolutionTrigger::Manual,
            EvolutionLayer::Memory,
            &mut engine,
            Utc::now(),
        )
        .await?;

    assert!(report.rolled_back);
    assert!(matches!(
        report.evolution_log.as_ref().map(|item| &item.change_type),
        Some(ChangeType::Rollback)
    ));
    assert_eq!(tokio::fs::read_to_string(&target).await?, "value = 1\n");
    Ok(())
}

#[test]
fn circuit_breaker_opens_and_pauses_execution_after_consecutive_failures() {
    let mut breaker = CircuitBreaker::new(2, 24);
    let now = Utc::now();

    breaker.record_failure(now);
    breaker.record_failure(now);

    assert_eq!(breaker.state(), CircuitBreakerState::Open);
    assert!(!breaker.can_execute(now));
}

#[tokio::test]
async fn sqlite_index_imports_jsonl_and_supports_queries() -> Result<()> {
    let dir = tempdir()?;
    let storage_root = dir.path().join("logs");
    let sqlite_path = dir.path().join("evolution.sqlite");

    let writer = AsyncJsonlWriter::new(
        JsonlStoragePaths::new(storage_root.clone()),
        JsonlRetentionPolicy::default(),
        1,
    )
    .await?;

    let ts = Utc::now().to_rfc3339();
    writer
        .append_memory_access(&sample_memory("trace-sql", "exp-sql", &ts, Some(true)))
        .await?;
    writer
        .append_decision(&sample_decision(
            "trace-sql",
            "exp-sql",
            &ts,
            Outcome::Success,
        ))
        .await?;
    writer
        .append_evolution(&EvolutionLog {
            experiment_id: "exp-sql".to_string(),
            timestamp: ts,
            layer: EvolutionLayer::Memory,
            change_type: ChangeType::Tune,
            before_value: "a".to_string(),
            after_value: "b".to_string(),
            trigger_reason: "index test".to_string(),
            data_basis: DataBasis {
                sample_count: 1,
                time_range_days: 1,
                key_metrics: HashMap::new(),
                patterns_found: vec![],
            },
            result: Some(EvolutionResult::Neutral),
        })
        .await?;
    writer.flush().await?;

    let indexer = JsonlToSqliteIndexer::new(&sqlite_path, &storage_root)?;
    let summary = indexer.import_incremental()?;
    assert!(summary.imported_memory_rows >= 1);
    assert!(summary.imported_decision_rows >= 1);
    assert!(summary.imported_evolution_rows >= 1);

    let by_exp = indexer.by_experiment("exp-sql")?;
    assert!(by_exp.len() >= 3);
    Ok(())
}

#[tokio::test]
async fn was_useful_annotation_auto_inference_updates_unknown_records() -> Result<()> {
    let dir = tempdir()?;
    let storage_root = dir.path().join("logs");

    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            JsonlRetentionPolicy::default(),
            1,
        )
        .await?,
    );

    let ts = Utc::now().to_rfc3339();
    writer
        .append_decision(&sample_decision(
            "trace-ann",
            "exp-ann",
            &ts,
            Outcome::Success,
        ))
        .await?;
    writer
        .append_memory_access(&sample_memory("trace-ann", "exp-ann", &ts, None))
        .await?;
    writer.flush().await?;

    let pipeline = AnnotationPipeline::new(writer.clone(), &storage_root);
    let report = pipeline.run_daily(Utc::now(), None).await?;
    assert!(report.applied_updates >= 1);

    let since = Utc::now() - Duration::hours(24);
    let rows = writer.read_memory_access_since(since).await?;
    let updated = rows
        .iter()
        .find(|item| item.trace_id == "trace-ann")
        .and_then(|item| item.was_useful);
    assert_eq!(updated, Some(true));
    Ok(())
}
