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
use crate::self_system::evolution::config::{EvolutionConfig, EvolutionMode, new_shared_evolution_config};
use crate::self_system::evolution::engine::{CycleResult, EngineCycleInput};
use crate::self_system::evolution::record::{Actor, AnnotationSource, DecisionType, MemoryAction, Outcome, TaskType};
use crate::self_system::evolution::safety_utils::acquire_file_lock;
use crate::self_system::evolution::storage::{JsonlRetentionPolicy, JsonlStoragePaths};
use crate::self_system::evolution::{
    CycleOutcome, EvolutionCycle, EvolutionProposal, EvolutionSignals, EvolutionValidation, FitnessTrend, RiskLevel,
    ValidationStatus,
};
use crate::self_system::fitness::{
    FitnessEvidence, FitnessMetricStatus, FitnessReport, FitnessReportStatus, FitnessSubscores, FitnessWeights,
    FitnessWindow,
};
use async_trait::async_trait;
use chrono::TimeZone;
use std::collections::BTreeMap;
use tempfile::tempdir;

struct MockEngine;

fn store_neutral_fitness_backfill(workspace: &Path) {
    let store = FitnessStore::new(workspace);
    for day in [
        "2026-02-17",
        "2026-02-18",
        "2026-02-19",
        "2026-02-21",
        "2026-02-22",
        "2026-02-23",
    ] {
        let report = fitness_report(day, 0.5);
        store.store(&report, 180).unwrap();
    }
}

fn fitness_report(day: &str, score: f64) -> FitnessReport {
    let parsed = day.parse::<chrono::NaiveDate>().unwrap();
    let start = DateTime::<Utc>::from_naive_utc_and_offset(parsed.and_hms_opt(0, 0, 0).unwrap(), Utc);
    FitnessReport {
        version: "2".to_string(),
        status: FitnessReportStatus::Final,
        window: FitnessWindow {
            date: day.to_string(),
            timezone: "UTC".to_string(),
            start: start.to_rfc3339(),
            end: (start + Duration::days(1)).to_rfc3339(),
        },
        subscores: FitnessSubscores::default(),
        metric_status: FitnessMetricStatus::default(),
        weights: FitnessWeights::default(),
        final_score: Some(score),
        confidence: 0.8,
        coverage: 0.8,
        evidence: FitnessEvidence::default(),
        generated_at: Utc::now().to_rfc3339(),
    }
}

#[test]
fn fitness_backfill_uses_three_days_immediately_after_change() {
    let changed_at = Utc.with_ymd_and_hms(2026, 2, 20, 0, 0, 0).unwrap();
    let reports = vec![
        fitness_report("2026-09-10", 0.1),
        fitness_report("2026-09-09", 0.1),
        fitness_report("2026-09-08", 0.1),
        fitness_report("2026-02-23", 0.8),
        fitness_report("2026-02-22", 0.8),
        fitness_report("2026-02-21", 0.8),
        fitness_report("2026-02-19", 0.5),
        fitness_report("2026-02-18", 0.5),
        fitness_report("2026-02-17", 0.5),
    ];

    assert_eq!(
        infer_fitness_result(reports, changed_at),
        Some(EvolutionResult::Improved)
    );
}

#[test]
fn fitness_backfill_reports_regression_and_requires_three_after_samples() {
    let changed_at = Utc.with_ymd_and_hms(2026, 2, 20, 0, 0, 0).unwrap();
    let before = [
        fitness_report("2026-02-19", 0.8),
        fitness_report("2026-02-18", 0.8),
        fitness_report("2026-02-17", 0.8),
    ];
    let two_after = [fitness_report("2026-02-21", 0.4), fitness_report("2026-02-22", 0.4)];
    let incomplete = before.iter().chain(&two_after).cloned().collect();
    assert_eq!(infer_fitness_result(incomplete, changed_at), None);

    let complete = before
        .into_iter()
        .chain(two_after)
        .chain([fitness_report("2026-02-23", 0.4)])
        .collect();
    assert_eq!(
        infer_fitness_result(complete, changed_at),
        Some(EvolutionResult::Regressed)
    );
}

#[async_trait]
impl EvolutionEngine for MockEngine {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn layer(&self) -> EvolutionLayer {
        EvolutionLayer::Prompt
    }

    async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult> {
        Ok(CycleResult {
            review_data: None,
            layer: EvolutionLayer::Prompt,
            proposal: Some(EvolutionProposal {
                id: input.cycle_id.clone(),
                summary: "mock proposal".to_string(),
                rationale: "mock rationale".to_string(),
                risk_level: RiskLevel::Low,
                target: crate::self_system::evolution::ChangeTarget::ConfigFile {
                    path: "evolution_config.toml".to_string(),
                },
                operation: crate::self_system::evolution::ChangeOperation::Write {
                    content: "".to_string(),
                },
            }),
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
                    previous_average: Some(0.5),
                    latest_score: Some(0.6),
                    is_declining: Some(false),
                    source: "test".to_string(),
                },
                proposal: None,
                validation: EvolutionValidation {
                    status: ValidationStatus::Improved,
                    before_score: Some(0.5),
                    after_score: Some(0.6),
                    delta: Some(0.1),
                    notes: "success".to_string(),
                },
                outcome: CycleOutcome::NoAction,
                alert: None,
                errors: Vec::new(),
            },
            evolution_log: None,
            needs_human_approval: false,
            shadow_mode: false,
            pending: None,
        })
    }
}

/// FIX-P0-40 fail-closed spy: an engine that records whether it was invoked
/// and writes a sentinel file the instant its `run_cycle` runs. Used to prove
/// that a side-effect gate denial prevents the engine (and therefore every
/// engine-internal write: config/strategy/prompt file + append_evolution)
/// from ever executing.
struct WriteSpyEngine {
    invoked: Arc<std::sync::atomic::AtomicBool>,
    sentinel_path: PathBuf,
}

#[async_trait]
impl EvolutionEngine for WriteSpyEngine {
    fn name(&self) -> &'static str {
        "write_spy"
    }

    fn layer(&self) -> EvolutionLayer {
        EvolutionLayer::Prompt
    }

    async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult> {
        // Mark invocation and emulate an engine that writes to disk during its
        // cycle (exactly the behaviour the gate must prevent on denial).
        self.invoked.store(true, std::sync::atomic::Ordering::SeqCst);
        fs::write(&self.sentinel_path, b"engine wrote this").await?;
        let mut engine = MockEngine;
        engine.run_cycle(input).await
    }
}

/// FIX-P0-40 (hard acceptance): under a Supervised policy with no runtime
/// grant, the side-effect gate must DENY the evolution commit *before* the
/// engine runs. Verifies the engine is never invoked, no sentinel file is
/// written, no evolution JSONL is appended, and the report flags `gate_denied`.
#[tokio::test]
async fn gate_denied_blocks_engine_and_all_writes() {
    let dir = tempdir().unwrap();
    let storage_root = dir.path().join("logs");
    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            JsonlRetentionPolicy::default(),
            1,
        )
        .await
        .unwrap(),
    );

    // Seed analyzer inputs so a candidate is produced and the pipeline reaches
    // the gate (otherwise it would early-return before authorize_commit).
    let now = Utc::now();
    for offset in 0..3 {
        let ts = (now - chrono::Duration::days(offset)).to_rfc3339();
        writer
            .append_decision(&crate::self_system::evolution::record::DecisionLog {
                timestamp: ts.clone(),
                experiment_id: "exp-seed".to_string(),
                trace_id: "trace-seed".to_string(),
                decision_type: DecisionType::ToolSelection,
                task_type: TaskType::ToolCall,
                risk_level: 1,
                actor: Actor::Agent,
                input_context: "ctx".to_string(),
                action_taken: "act".to_string(),
                outcome: Outcome::Failure,
                tokens_used: 1,
                latency_ms: 1,
                user_correction: Some("please do X instead".to_string()),
                config_snapshot_hash: "cfg".to_string(),
            })
            .await
            .unwrap();
        writer
            .append_memory_access(&crate::self_system::evolution::record::MemoryAccessLog {
                timestamp: ts,
                experiment_id: "exp-seed".to_string(),
                trace_id: "trace-seed".to_string(),
                action: MemoryAction::Read,
                memory_id: "m1".to_string(),
                task_context: "ctx".to_string(),
                task_type: TaskType::ToolCall,
                actor: Actor::Agent,
                was_useful: Some(false),
                useful_annotation_source: Some(AnnotationSource::AutoEvaluator),
                annotation_confidence: Some(0.8),
                tokens_consumed: 1,
            })
            .await
            .unwrap();
    }
    writer.flush().await.unwrap();

    let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.path().join("analysis")));
    let mut cfg = EvolutionConfig::default();
    cfg.runtime.mode = EvolutionMode::Auto;
    cfg.runtime.storage_dir = storage_root.to_string_lossy().to_string();
    let shared = new_shared_evolution_config(cfg);

    for offset in (0..3).rev() {
        let _ = analyzer
            .generate_daily_digest(now - chrono::Duration::days(offset))
            .await;
    }

    // Supervised + require_approval_for_medium_risk (the default) denies a
    // Medium-risk evolution commit when no runtime grant is present.
    let policy = Arc::new(SecurityPolicy {
        autonomy: crate::security::AutonomyLevel::Supervised,
        workspace_dir: dir.path().to_path_buf(),
        ..SecurityPolicy::default()
    });
    let pipeline = EvolutionPipeline::new(shared, analyzer, writer.clone(), dir.path()).with_security_policy(policy);

    let invoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sentinel_path = dir.path().join("engine_sentinel.txt");
    let mut engine = WriteSpyEngine {
        invoked: Arc::clone(&invoked),
        sentinel_path: sentinel_path.clone(),
    };

    let report = pipeline
        .run_for_layer(EvolutionTrigger::CronTick, EvolutionLayer::Prompt, &mut engine, now)
        .await
        .unwrap();

    // The seeded analyzer inputs (a declining task with user corrections over
    // three days) deterministically produce a candidate that passes the
    // evolution gate, so the pipeline reaches the side-effect gate. Assert it
    // so a regression in candidate generation can never silently turn this
    // fail-closed test into a no-op.
    assert!(
        report.selected_candidate.is_some(),
        "test setup must produce a candidate so the side-effect gate is actually exercised"
    );

    // Hard acceptance criteria for FIX-P0-40 fail-closed behaviour:
    assert!(report.gate_denied, "report must flag gate_denied on a deny decision");
    assert!(
        report.evolution_log.is_none(),
        "no evolution_log may be committed on deny"
    );
    assert!(
        !invoked.load(std::sync::atomic::Ordering::SeqCst),
        "engine run_cycle MUST NOT run when the gate denies the commit"
    );
    assert!(
        !sentinel_path.exists(),
        "engine must not write any file when the gate denies the commit"
    );
    // No evolution JSONL was appended by the pipeline either.
    let evo_root = storage_root.join("evolution");
    let mut appended = false;
    for tier in ["hot", "warm", "cold"] {
        let tier_dir = evo_root.join(tier);
        if let Ok(mut rd) = fs::read_dir(&tier_dir).await {
            while let Ok(Some(entry)) = rd.next_entry().await {
                if entry.path().extension().and_then(|v| v.to_str()) == Some("jsonl") {
                    let raw = fs::read_to_string(entry.path())
                        .await
                        .expect("test: read an evolution jsonl file");
                    if !raw.trim().is_empty() {
                        appended = true;
                    }
                }
            }
        }
    }
    assert!(!appended, "no evolution log line may be appended on a gate deny");

    // Structured gate rejection is recorded for observability.
    assert!(
        report
            .gate_rejections
            .iter()
            .any(|rejection| rejection.reason == "side_effect_gate_denied"),
        "a structured side_effect_gate_denied rejection must be recorded"
    );
}

/// An engine that behaves like the real ones in Auto mode (D-046): it
/// proposes rewriting `evolution_config.toml` and leaves the write to the
/// pipeline.
struct ProposingEngine {
    workspace: PathBuf,
}

#[async_trait]
impl EvolutionEngine for ProposingEngine {
    fn name(&self) -> &'static str {
        "proposing"
    }

    fn layer(&self) -> EvolutionLayer {
        EvolutionLayer::Prompt
    }

    async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult> {
        let target = self.workspace.join("evolution_config.toml");
        let backup = crate::self_system::evolution::rollback::RollbackManager::new(
            &self.workspace,
            &target,
            self.workspace.join(".evolution/rollback/memory"),
            5,
        )?;
        let mut result = MockEngine.run_cycle(input).await?;
        result.pending = Some(Arc::new(crate::self_system::evolution::FileChange::new(
            self.workspace.clone(),
            target,
            "changed by the engine".to_string(),
            backup,
        )));
        Ok(result)
    }
}

/// A real judge that scores every proposal `score`.
struct ScoringJudge(f64);

#[async_trait]
impl JudgeScoringModel for ScoringJudge {
    async fn score(&self, _task: &str, _result: &str, _config: &JudgeConfig) -> Result<String> {
        let score = self.0;
        Ok(format!(
            "{{\"task_completion\":{score},\"correctness\":{score},\"efficiency\":{score},\
                 \"safety_compliance\":{score},\"reasoning\":\"test verdict\"}}"
        ))
    }
}

/// A real (non-mock) judge whose provider call errors.
struct ErroringJudge;

#[async_trait]
impl JudgeScoringModel for ErroringJudge {
    async fn score(&self, _task: &str, _result: &str, _config: &JudgeConfig) -> Result<String> {
        anyhow::bail!("model judge provider call failed: connection refused; token=opaque7credential")
    }
}

/// A real (non-mock) judge that answers in prose, not the score schema.
struct ProseJudge;

#[async_trait]
impl JudgeScoringModel for ProseJudge {
    async fn score(&self, _task: &str, _result: &str, _config: &JudgeConfig) -> Result<String> {
        Ok("I think this change looks fine overall.".to_string())
    }
}

/// An Auto-mode pipeline over a workspace with enough digests for a
/// candidate. `judge_model` of `None` keeps the built-in mock scorer.
async fn auto_pipeline(dir: &Path, judge_model: Option<Arc<dyn JudgeScoringModel>>) -> EvolutionPipeline {
    auto_pipeline_batched(dir, judge_model, 1).await
}

/// `auto_pipeline` with the writer's batch size; production uses 64.
async fn auto_pipeline_batched(
    dir: &Path,
    judge_model: Option<Arc<dyn JudgeScoringModel>>,
    batch_size: usize,
) -> EvolutionPipeline {
    let storage_root = dir.join("logs");
    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            JsonlRetentionPolicy::default(),
            batch_size,
        )
        .await
        .unwrap(),
    );

    let decision = crate::self_system::evolution::record::DecisionLog {
        timestamp: Utc::now().to_rfc3339(),
        experiment_id: "exp-a".to_string(),
        trace_id: "trace-a".to_string(),
        decision_type: DecisionType::ToolSelection,
        task_type: TaskType::ToolCall,
        risk_level: 1,
        actor: Actor::Agent,
        input_context: "ctx".to_string(),
        action_taken: "act".to_string(),
        outcome: Outcome::Success,
        tokens_used: 1,
        latency_ms: 1,
        user_correction: None,
        config_snapshot_hash: "cfg".to_string(),
    };
    writer.append_decision(&decision).await.unwrap();

    let memory = crate::self_system::evolution::record::MemoryAccessLog {
        timestamp: Utc::now().to_rfc3339(),
        experiment_id: "exp-a".to_string(),
        trace_id: "trace-a".to_string(),
        action: MemoryAction::Read,
        memory_id: "m1".to_string(),
        task_context: "ctx".to_string(),
        task_type: TaskType::ToolCall,
        actor: Actor::Agent,
        was_useful: Some(true),
        useful_annotation_source: Some(AnnotationSource::AutoEvaluator),
        annotation_confidence: Some(0.8),
        tokens_consumed: 1,
    };
    writer.append_memory_access(&memory).await.unwrap();
    writer.flush().await.unwrap();

    let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.join("analysis")));
    let mut cfg = EvolutionConfig::default();
    cfg.runtime.mode = EvolutionMode::Auto;
    cfg.runtime.storage_dir = storage_root.to_string_lossy().to_string();
    let shared = new_shared_evolution_config(cfg);

    let pipeline = match judge_model {
        Some(model) => EvolutionPipeline::with_judge_model(shared, analyzer, writer, dir, model),
        None => EvolutionPipeline::new(shared, analyzer, writer, dir),
    };
    let now = Utc::now();
    for days in [2, 1, 0] {
        let _ = pipeline
            .analyzer
            .generate_daily_digest(now - chrono::Duration::days(days))
            .await;
    }
    pipeline
}

/// Run one Auto cycle whose engine proposes rewriting
/// `evolution_config.toml`, and return the report and the file afterwards.
async fn run_proposing_cycle(judge_model: Option<Arc<dyn JudgeScoringModel>>) -> (PipelineRunReport, String) {
    let dir = tempdir().unwrap();
    let pipeline = auto_pipeline(dir.path(), judge_model).await;
    let target = dir.path().join("evolution_config.toml");
    fs::write(&target, b"original").await.unwrap();
    let mut engine = ProposingEngine {
        workspace: dir.path().to_path_buf(),
    };
    let report = pipeline
        .run_for_layer(
            EvolutionTrigger::Manual,
            EvolutionLayer::Prompt,
            &mut engine,
            Utc::now(),
        )
        .await
        .unwrap_or_else(|err| {
            panic!(
                "the run failed ({err:#}) and left the target reading {:?}",
                std::fs::read_to_string(&target).unwrap_or_default()
            )
        });
    let after = fs::read_to_string(&target).await.unwrap();
    (report, after)
}

/// D-035 / D-046: a degraded (mock) judge verdict is a deferral: the
/// proposed change is not written, whatever the mock scored.
#[tokio::test]
async fn a_mock_judge_verdict_writes_nothing() {
    let (report, after) = run_proposing_cycle(None).await;

    assert!(report.judge_result.is_some());
    assert_eq!(report.commit, CommitDecision::Deferred, "{:?}", report.errors);
    assert_eq!(after, "original", "a mock verdict wrote the change");
    assert!(
        report.errors.iter().any(|error| error.contains("judge_degraded")),
        "{:?}",
        report.errors
    );
    let log = report.evolution_log.as_ref().expect("test: the cycle is recorded");
    assert_eq!(log.result, Some(EvolutionResult::Deferred));
}

/// D-046: a model judge's accepted verdict is what writes the change.
#[tokio::test]
async fn an_accepted_verdict_writes_the_change() {
    let (report, after) = run_proposing_cycle(Some(Arc::new(ScoringJudge(0.9)))).await;
    assert_eq!(report.commit, CommitDecision::Applied, "{:?}", report.errors);
    assert_eq!(report.judge_outcome, JudgeOutcome::Scored);
    assert_eq!(after, "changed by the engine");
    let log = report.evolution_log.as_ref().expect("test: the cycle is recorded");
    assert_eq!(log.result, None, "an applied change waits for the fitness backfill");
    assert_ne!(log.change_type, ChangeType::Rollback);
}

/// D-046: a model judge's score below the pass threshold writes nothing.
#[tokio::test]
async fn a_failing_score_writes_nothing() {
    let (report, after) = run_proposing_cycle(Some(Arc::new(ScoringJudge(0.1)))).await;
    assert_eq!(report.commit, CommitDecision::Rejected, "{:?}", report.errors);
    assert_eq!(after, "original");
    let log = report.evolution_log.as_ref().expect("test: the cycle is recorded");
    assert_eq!(log.result, Some(EvolutionResult::Rejected));
}

fn assert_judge_failure_writes_nothing(report: &PipelineRunReport, after: &str) {
    assert_eq!(after, "original", "an unjudged change was written: {:?}", report.errors);
    assert_eq!(report.commit, CommitDecision::Deferred, "{:?}", report.errors);
    assert_eq!(report.judge_outcome, JudgeOutcome::Failed);
    assert!(report.judge_result.is_none());
    assert!(
        report.errors.iter().any(|error| error.contains("judge_failed")),
        "{:?}",
        report.errors
    );
    let log = report.evolution_log.as_ref().expect("test: the cycle is recorded");
    assert_ne!(log.change_type, ChangeType::Rollback);
    assert_eq!(log.result, Some(EvolutionResult::Deferred));
}

/// R-PMMCg J-1 / D-046: a real judge whose provider call errors gives no
/// verdict: nothing is written and the run still reports normally.
#[tokio::test]
async fn a_judge_provider_error_writes_nothing() {
    let (report, after) = run_proposing_cycle(Some(Arc::new(ErroringJudge))).await;
    assert_judge_failure_writes_nothing(&report, &after);
    // Mutation: keep the judge's raw chain in the verdict errors.
    assert!(report.errors.iter().all(|error| !error.contains("opaque7credential")));
    assert!(report.errors.iter().any(|error| error.contains("token=[REDACTED]")));
}

/// R-PMMCg J-1 / D-046: a real judge whose reply is not the score schema.
#[tokio::test]
async fn an_unparseable_judge_reply_writes_nothing() {
    let (report, after) = run_proposing_cycle(Some(Arc::new(ProseJudge))).await;
    assert_judge_failure_writes_nothing(&report, &after);
}

/// D-046: an accepted change that cannot be written is reported as such,
/// not as applied.
#[tokio::test]
async fn an_accepted_change_that_cannot_be_written_is_reported_truthfully() {
    let dir = tempdir().unwrap();
    let pipeline = auto_pipeline(dir.path(), Some(Arc::new(ScoringJudge(0.9)))).await;
    // The target is a directory, so the atomic replace fails.
    fs::create_dir_all(dir.path().join("evolution_config.toml"))
        .await
        .unwrap();
    let mut engine = ProposingEngine {
        workspace: dir.path().to_path_buf(),
    };
    let report = pipeline
        .run_for_layer(
            EvolutionTrigger::Manual,
            EvolutionLayer::Prompt,
            &mut engine,
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(report.commit, CommitDecision::ApplyFailed, "{:?}", report.errors);
    assert!(
        report.errors.iter().any(|error| error.contains("apply_failed")),
        "{:?}",
        report.errors
    );
    let rows = persisted_evolution_rows(&dir.path().join("logs")).await;
    let row = rows
        .iter()
        .find(|row| row.experiment_id == report.experiment_id)
        .expect("test: the cycle is recorded");
    assert_eq!(row.result, Some(EvolutionResult::Deferred), "{row:?}");
}

/// A real judge that never answers (a stalled provider connection).
struct HangingJudge;

#[async_trait]
impl JudgeScoringModel for HangingJudge {
    async fn score(&self, _task: &str, _result: &str, _config: &JudgeConfig) -> Result<String> {
        std::future::pending::<()>().await;
        anyhow::bail!("unreachable: a pending future never resolves")
    }
}

/// A real judge that announces it has been called, then never answers.
struct AnnouncingHangingJudge {
    called: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl JudgeScoringModel for AnnouncingHangingJudge {
    async fn score(&self, _task: &str, _result: &str, _config: &JudgeConfig) -> Result<String> {
        self.called.notify_one();
        std::future::pending::<()>().await;
        anyhow::bail!("unreachable: a pending future never resolves")
    }
}

/// Every evolution log row the pipeline persisted under `storage_root`.
async fn persisted_evolution_rows(storage_root: &Path) -> Vec<EvolutionLog> {
    let mut rows = Vec::new();
    for tier in ["hot", "warm", "cold"] {
        let Ok(mut entries) = fs::read_dir(storage_root.join("evolution").join(tier)).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.path().extension().and_then(|v| v.to_str()) != Some("jsonl") {
                continue;
            }
            let raw = fs::read_to_string(entry.path())
                .await
                .expect("test: read an evolution jsonl file");
            rows.extend(
                raw.lines()
                    .filter_map(|line| serde_json::from_str::<EvolutionLog>(line).ok()),
            );
        }
    }
    rows
}

/// Run one Auto cycle under a paused clock with a judge that never
/// answers. The test's own bound (one hour) is far above the pipeline's
/// judge timeout, so reaching it means the run hung.
async fn run_with_hanging_judge(engine: &mut dyn EvolutionEngine, dir: &Path) -> PipelineRunReport {
    let pipeline = auto_pipeline(dir, Some(Arc::new(HangingJudge))).await;
    tokio::time::timeout(
        std::time::Duration::from_hours(1),
        pipeline.run_for_layer(EvolutionTrigger::Manual, EvolutionLayer::Prompt, engine, Utc::now()),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the judge never answered and the run hung with the target reading {:?}",
            std::fs::read_to_string(dir.join("evolution_config.toml")).unwrap_or_default()
        )
    })
    .expect("test: a hanging judge must still return a report")
}

/// D-044 / D-046: a judge that never answers is bounded by the judge
/// timeout, which is a `judge_failed` verdict: nothing is written and the
/// cycle is recorded as deferred.
#[tokio::test(start_paused = true)]
async fn a_hanging_judge_times_out_and_writes_nothing() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("evolution_config.toml");
    fs::write(&target, b"original").await.unwrap();
    let mut engine = ProposingEngine {
        workspace: dir.path().to_path_buf(),
    };
    let report = run_with_hanging_judge(&mut engine, dir.path()).await;

    assert_eq!(fs::read_to_string(&target).await.unwrap(), "original");
    assert_eq!(report.commit, CommitDecision::Deferred, "{:?}", report.errors);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("judge_failed") && error.contains("timed out")),
        "{:?}",
        report.errors
    );
    let rows = persisted_evolution_rows(&dir.path().join("logs")).await;
    assert!(
        rows.iter().any(|row| row.experiment_id == report.experiment_id
            && row.change_type != ChangeType::Rollback
            && row.result == Some(EvolutionResult::Deferred)),
        "{rows:?}"
    );
}

/// D-044 / D-046: dropping the run while the judge works (a configuration
/// swap) writes nothing, and the cycle is still recorded without the
/// caller.
#[tokio::test]
async fn cancelling_the_run_mid_judge_writes_nothing_and_records_the_cycle() {
    let dir = tempdir().unwrap();
    let called = Arc::new(tokio::sync::Notify::new());
    let pipeline = auto_pipeline(
        dir.path(),
        Some(Arc::new(AnnouncingHangingJudge {
            called: Arc::clone(&called),
        })),
    )
    .await;
    let target = dir.path().join("evolution_config.toml");
    fs::write(&target, b"original").await.unwrap();
    let mut engine = ProposingEngine {
        workspace: dir.path().to_path_buf(),
    };

    {
        let run = pipeline.run_for_layer(
            EvolutionTrigger::Manual,
            EvolutionLayer::Prompt,
            &mut engine,
            Utc::now(),
        );
        tokio::select! {
            _ = run => panic!("test: the run finished although its judge never answers"),
            () = called.notified() => {}
        }
        // Leaving this block drops the run while the judge is working.
    }

    // The record is written without the caller; allow it generous time.
    let mut rows = Vec::new();
    for _ in 0..500 {
        rows = persisted_evolution_rows(&dir.path().join("logs")).await;
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        fs::read_to_string(&target).await.unwrap(),
        "original",
        "a cancelled run wrote the unjudged change"
    );
    assert!(
        rows.iter().any(|row| row.result == Some(EvolutionResult::Deferred)),
        "the cancelled cycle was not recorded: {rows:?}"
    );
}

// ── D-046: an evolution change is written only after an accepted verdict ──

const PROMPT_ORIGINAL: &str = "system instructions v1\nbe helpful\n";
const POLICY_ORIGINAL: &str = "[retry]\nmax_attempts = 3\nbackoff = 0.5\n";

/// A real judge that records what the layer's target file held when it
/// was asked, then answers with `reply` (an error when `None`).
struct SnapshotJudge {
    target: PathBuf,
    seen: Arc<parking_lot::Mutex<Option<String>>>,
    reply: Option<f64>,
}

#[async_trait]
impl JudgeScoringModel for SnapshotJudge {
    async fn score(&self, _task: &str, result: &str, _config: &JudgeConfig) -> Result<String> {
        if self.target.file_name().is_some_and(|name| name == "p.md") {
            let projection: serde_json::Value = serde_json::from_str(result)
                .expect("the prompt judge must receive the actual bounded proposal projection");
            assert_eq!(projection["before"], PROMPT_ORIGINAL);
            assert!(projection["after"].as_str().unwrap().contains("Verify the result"));
            assert!(projection["provenance"].as_str().unwrap().contains("input_sha256"));
        }
        *self.seen.lock() = Some(std::fs::read_to_string(&self.target).unwrap_or_default());
        match self.reply {
            Some(score) => Ok(format!(
                "{{\"task_completion\":{score},\"correctness\":{score},\"efficiency\":{score},\
                     \"safety_compliance\":{score},\"reasoning\":\"test verdict\"}}"
            )),
            None => anyhow::bail!("model judge provider call failed: connection refused"),
        }
    }
}

/// The generic commit contract stays covered independently of production
/// prompt evolution, which only stages lifecycle proposals.
struct TestFileEngine {
    workspace: PathBuf,
    target: PathBuf,
    before: String,
}

#[async_trait]
impl EvolutionEngine for TestFileEngine {
    fn name(&self) -> &'static str {
        "test_file"
    }
    fn layer(&self) -> EvolutionLayer {
        EvolutionLayer::Prompt
    }
    async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult> {
        let after = format!("{}\nVerify the result before reporting completion.\n", self.before);
        let mut result = MockEngine.run_cycle(input).await?;
        if let Some(proposal) = result.proposal.as_mut() {
            proposal.target = crate::self_system::evolution::ChangeTarget::WorkspaceFile {
                path: self
                    .target
                    .strip_prefix(&self.workspace)?
                    .to_string_lossy()
                    .into_owned(),
            };
            proposal.operation = crate::self_system::evolution::ChangeOperation::Write { content: after.clone() };
        }
        result.review_data = Some(super::super::engine::ReviewData::new(serde_json::to_string(
            &serde_json::json!({
                "before": self.before, "after": after, "provenance": "input_sha256=fixture"
            }),
        )?));
        result.pending = Some(Arc::new(super::super::engine::FileChange::new(
            self.workspace.clone(),
            self.target.clone(),
            after,
            super::super::rollback::RollbackManager::new(
                &self.workspace,
                &self.target,
                self.workspace.join(".evolution/rollback/prompt"),
                10,
            )?,
        )));
        Ok(result)
    }
}

async fn pending_prompt_file(dir: &Path, _pipeline: &EvolutionPipeline) -> TestFileEngine {
    fs::write(dir.join("p.md"), PROMPT_ORIGINAL).await.unwrap();
    TestFileEngine {
        workspace: dir.into(),
        target: dir.join("p.md"),
        before: PROMPT_ORIGINAL.into(),
    }
}

async fn pending_legacy_policy_file(dir: &Path, _pipeline: &EvolutionPipeline) -> TestFileEngine {
    fs::write(dir.join("decision_policy.toml"), POLICY_ORIGINAL)
        .await
        .unwrap();
    TestFileEngine {
        workspace: dir.into(),
        target: dir.join("decision_policy.toml"),
        before: POLICY_ORIGINAL.into(),
    }
}

fn snapshot_judge(
    target: PathBuf,
    reply: Option<f64>,
) -> (Arc<SnapshotJudge>, Arc<parking_lot::Mutex<Option<String>>>) {
    let seen = Arc::new(parking_lot::Mutex::new(None));
    (
        Arc::new(SnapshotJudge {
            target,
            seen: Arc::clone(&seen),
            reply,
        }),
        seen,
    )
}

/// D-046: a generic pending prompt-file change is not on disk while the judge
/// decides, and a judge that gives no verdict leaves it unwritten.
#[tokio::test]
async fn generic_prompt_file_change_writes_nothing_before_the_verdict() {
    let dir = tempdir().unwrap();
    let (judge, seen) = snapshot_judge(dir.path().join("p.md"), None);
    let pipeline = auto_pipeline(dir.path(), Some(judge)).await;
    let mut engine = pending_prompt_file(dir.path(), &pipeline).await;
    let report = pipeline
        .run_for_layer(
            EvolutionTrigger::Manual,
            EvolutionLayer::Prompt,
            &mut engine,
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(
        seen.lock().as_deref(),
        Some(PROMPT_ORIGINAL),
        "the prompt was rewritten before the judge answered: {:?}",
        report.errors
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("p.md")).await.unwrap(),
        PROMPT_ORIGINAL
    );
}

/// D-046: an accepted verdict is what writes a generic pending file change.
#[tokio::test]
async fn generic_prompt_file_change_writes_after_an_accepted_verdict() {
    let dir = tempdir().unwrap();
    let (judge, seen) = snapshot_judge(dir.path().join("p.md"), Some(0.9));
    let pipeline = auto_pipeline(dir.path(), Some(judge)).await;
    let mut engine = pending_prompt_file(dir.path(), &pipeline).await;
    let report = pipeline
        .run_for_layer(
            EvolutionTrigger::Manual,
            EvolutionLayer::Prompt,
            &mut engine,
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(seen.lock().as_deref(), Some(PROMPT_ORIGINAL), "{:?}", report.errors);
    assert_ne!(
        fs::read_to_string(dir.path().join("p.md")).await.unwrap(),
        PROMPT_ORIGINAL,
        "an accepted verdict did not write the change: {:?}",
        report.errors
    );
}

/// D-046: a generic pending legacy-policy file is not on disk while the judge
/// decides.
#[tokio::test]
async fn generic_file_change_writes_nothing_before_the_verdict() {
    let dir = tempdir().unwrap();
    let (judge, seen) = snapshot_judge(dir.path().join("decision_policy.toml"), None);
    let pipeline = auto_pipeline(dir.path(), Some(judge)).await;
    let mut engine = pending_legacy_policy_file(dir.path(), &pipeline).await;
    let report = pipeline
        .run_for_layer(
            EvolutionTrigger::Manual,
            EvolutionLayer::Prompt,
            &mut engine,
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(
        seen.lock().as_deref(),
        Some(POLICY_ORIGINAL),
        "the policy was rewritten before the judge answered: {:?}",
        report.errors
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("decision_policy.toml"))
            .await
            .unwrap(),
        POLICY_ORIGINAL
    );
}

/// R-PMMCi F-2: at the production batch size (64) the cycle's row is on
/// disk when the run returns, for a refused and an accepted verdict.
#[tokio::test]
async fn the_cycle_row_is_on_disk_at_the_production_batch_size() {
    for reply in [None, Some(0.9)] {
        let dir = tempdir().unwrap();
        let (judge, _seen) = snapshot_judge(dir.path().join("p.md"), reply);
        let pipeline = auto_pipeline_batched(dir.path(), Some(judge), 64).await;
        let mut engine = pending_prompt_file(dir.path(), &pipeline).await;
        let report = pipeline
            .run_for_layer(
                EvolutionTrigger::Manual,
                EvolutionLayer::Prompt,
                &mut engine,
                Utc::now(),
            )
            .await
            .unwrap();
        let rows = persisted_evolution_rows(&dir.path().join("logs")).await;
        assert!(
            rows.iter().any(|row| row.experiment_id == report.experiment_id),
            "reply {reply:?}: the row is still buffered: {rows:?}"
        );
    }
}

/// R-PMMCi F-2: a run dropped while its judge works still leaves its row
/// on disk at the production batch size, and nothing written.
#[tokio::test]
async fn a_cancelled_run_row_is_on_disk_at_the_production_batch_size() {
    let dir = tempdir().unwrap();
    let called = Arc::new(tokio::sync::Notify::new());
    let pipeline = auto_pipeline_batched(
        dir.path(),
        Some(Arc::new(AnnouncingHangingJudge {
            called: Arc::clone(&called),
        })),
        64,
    )
    .await;
    let mut engine = pending_prompt_file(dir.path(), &pipeline).await;
    {
        let run = pipeline.run_for_layer(
            EvolutionTrigger::Manual,
            EvolutionLayer::Prompt,
            &mut engine,
            Utc::now(),
        );
        tokio::select! {
            _ = run => panic!("test: the run finished although its judge never answers"),
            () = called.notified() => {}
        }
    }
    let mut rows = Vec::new();
    for _ in 0..500 {
        rows = persisted_evolution_rows(&dir.path().join("logs")).await;
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!rows.is_empty(), "the cancelled cycle's row never reached disk");
    assert_eq!(
        fs::read_to_string(dir.path().join("p.md")).await.unwrap(),
        PROMPT_ORIGINAL
    );
}

/// Every evolution row on disk, read without an async runtime.
fn persisted_row_count_sync(storage_root: &Path) -> usize {
    let mut count = 0;
    for tier in ["hot", "warm", "cold"] {
        let Ok(entries) = std::fs::read_dir(storage_root.join("evolution").join(tier)) else {
            continue;
        };
        for entry in entries.flatten() {
            count += std::fs::read_to_string(entry.path())
                .unwrap_or_default()
                .lines()
                .count();
        }
    }
    count
}

/// What `daemon::run` does after aborting its supervisors and before it
/// returns: drain the evolution commit tracker (D-046).
async fn daemon_teardown(commits: &CommitTracker) {
    crate::daemon::drain_evolution_commits(commits).await;
}

/// R-PMMCi F-1 probe, as a test: a daemon-style teardown while the judge
/// works. `mode` 0 cancels the worker and awaits it, 1 aborts it, 2
/// leaves it running; then the daemon's own step runs
/// (`daemon_teardown`), `block_on` returns and the runtime is shut down
/// with main's two-second bound. Returns (prompt unchanged, rows on disk).
fn daemon_style_shutdown(mode: u8) -> (bool, usize) {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on({
            let root = root.clone();
            async move {
                let called = Arc::new(tokio::sync::Notify::new());
                let commits = CommitTracker::default();
                let pipeline = auto_pipeline_batched(
                    &root,
                    Some(Arc::new(AnnouncingHangingJudge {
                        called: Arc::clone(&called),
                    })),
                    64,
                )
                .await
                .with_commit_tracker(commits.clone());
                let mut engine = pending_prompt_file(&root, &pipeline).await;
                let token = CancellationToken::new();
                let worker_token = token.clone();
                let worker = tokio::spawn(async move {
                    tokio::select! {
                        _ = pipeline.run_for_layer(EvolutionTrigger::CronTick, EvolutionLayer::Prompt, &mut engine, Utc::now()) => {}
                        () = worker_token.cancelled() => {}
                    }
                });
                called.notified().await;
                match mode {
                    0 => {
                        token.cancel();
                        let _ = worker.await;
                    }
                    1 => worker.abort(),
                    _ => {}
                }
                daemon_teardown(&commits).await;
            }
        });
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    let unchanged = std::fs::read_to_string(root.join("p.md")).unwrap_or_default() == PROMPT_ORIGINAL;
    (unchanged, persisted_row_count_sync(&root.join("logs")))
}

/// R-PMMCi F-1 / D-046: whichever way the daemon tears the worker down,
/// a change the judge never accepted is not written, and the cycle's row
/// reaches disk before the runtime is gone.
#[test]
fn a_daemon_shutdown_mid_judge_writes_nothing_and_records_the_cycle() {
    for mode in [0u8, 1, 2] {
        for run in 0..10 {
            let (unchanged, rows) = daemon_style_shutdown(mode);
            assert!(unchanged, "mode {mode} run {run}: the unjudged change was kept");
            assert!(rows > 0, "mode {mode} run {run}: the cycle was not recorded");
        }
    }
}

/// A pending change that announces its write has started, takes a while,
/// then writes: the window a shutdown must wait out.
#[derive(Debug)]
struct SlowChange {
    target: PathBuf,
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl crate::self_system::evolution::PendingChange for SlowChange {
    async fn apply(&self) -> Result<()> {
        self.started.notify_one();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        fs::write(&self.target, b"changed by the engine").await?;
        Ok(())
    }
}

struct SlowProposingEngine {
    workspace: PathBuf,
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl EvolutionEngine for SlowProposingEngine {
    fn name(&self) -> &'static str {
        "slow_proposing"
    }

    fn layer(&self) -> EvolutionLayer {
        EvolutionLayer::Prompt
    }

    async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult> {
        let mut result = MockEngine.run_cycle(input).await?;
        result.pending = Some(Arc::new(SlowChange {
            target: self.workspace.join("evolution_config.toml"),
            started: Arc::clone(&self.started),
        }));
        Ok(result)
    }
}

/// D-046: a write an accepted verdict started is tracked: a daemon-style
/// shutdown (worker aborted, tracker drained, runtime shut down with
/// main's two-second bound) waits for it and for the cycle's row.
#[test]
fn a_daemon_shutdown_waits_for_an_accepted_write_and_its_row() {
    for run in 0..5 {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on({
            let root = root.clone();
            async move {
                let commits = CommitTracker::default();
                let pipeline = auto_pipeline_batched(&root, Some(Arc::new(ScoringJudge(0.9))), 64)
                    .await
                    .with_commit_tracker(commits.clone());
                fs::write(root.join("evolution_config.toml"), b"original")
                    .await
                    .unwrap();
                let started = Arc::new(tokio::sync::Notify::new());
                let mut engine = SlowProposingEngine {
                    workspace: root.clone(),
                    started: Arc::clone(&started),
                };
                let worker = tokio::spawn(async move {
                    let _ = pipeline
                        .run_for_layer(
                            EvolutionTrigger::CronTick,
                            EvolutionLayer::Prompt,
                            &mut engine,
                            Utc::now(),
                        )
                        .await;
                });
                started.notified().await;
                worker.abort();
                daemon_teardown(&commits).await;
            }
        });
        runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        assert_eq!(
            std::fs::read_to_string(root.join("evolution_config.toml")).unwrap_or_default(),
            "changed by the engine",
            "run {run}: the accepted write was cut off"
        );
        assert!(
            persisted_row_count_sync(&root.join("logs")) > 0,
            "run {run}: the applied cycle was not recorded"
        );
    }
}

/// R-PMMCi F-3: a verdict task cancelled after the judge accepted and
/// the write started says so: the change was written and recorded as
/// applied, not "cancelled before its verdict". The shutdown drain
/// cancels it while the run still waits, so the note reaches the report.
#[tokio::test]
async fn a_cancellation_after_the_write_started_is_reported_as_such() {
    let dir = tempdir().unwrap();
    let commits = CommitTracker::default();
    let pipeline = auto_pipeline(dir.path(), Some(Arc::new(ScoringJudge(0.9))))
        .await
        .with_commit_tracker(commits.clone());
    fs::write(dir.path().join("evolution_config.toml"), b"original")
        .await
        .unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let mut engine = SlowProposingEngine {
        workspace: dir.path().to_path_buf(),
        started: Arc::clone(&started),
    };
    let run = pipeline.run_for_layer(
        EvolutionTrigger::Manual,
        EvolutionLayer::Prompt,
        &mut engine,
        Utc::now(),
    );
    let drain = async {
        started.notified().await;
        commits.drain(std::time::Duration::from_secs(5)).await
    };
    let (report, drained) = tokio::join!(run, drain);
    let report = report.unwrap();
    assert!(drained);
    assert_eq!(report.commit, CommitDecision::Applied, "{:?}", report.errors);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("cancelled after its change was written")),
        "{:?}",
        report.errors
    );
    assert!(
        !report
            .errors
            .iter()
            .any(|error| error.contains("before the judge answered")),
        "{:?}",
        report.errors
    );
}

/// R-PMMCi F-3: a drain that lands while the judge still works is
/// reported as a cancellation before the verdict, with nothing written.
#[tokio::test]
async fn a_cancellation_before_the_verdict_is_reported_as_such() {
    let dir = tempdir().unwrap();
    let commits = CommitTracker::default();
    let called = Arc::new(tokio::sync::Notify::new());
    let pipeline = auto_pipeline(
        dir.path(),
        Some(Arc::new(AnnouncingHangingJudge {
            called: Arc::clone(&called),
        })),
    )
    .await
    .with_commit_tracker(commits.clone());
    fs::write(dir.path().join("evolution_config.toml"), b"original")
        .await
        .unwrap();
    let mut engine = ProposingEngine {
        workspace: dir.path().to_path_buf(),
    };
    let run = pipeline.run_for_layer(
        EvolutionTrigger::Manual,
        EvolutionLayer::Prompt,
        &mut engine,
        Utc::now(),
    );
    let drain = async {
        called.notified().await;
        commits.drain(std::time::Duration::from_secs(5)).await
    };
    let (report, drained) = tokio::join!(run, drain);
    let report = report.unwrap();
    assert!(drained);
    assert_eq!(report.commit, CommitDecision::Deferred, "{:?}", report.errors);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("cancelled before the judge answered")),
        "{:?}",
        report.errors
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("evolution_config.toml"))
            .await
            .unwrap(),
        "original"
    );
}

/// R-PMMCj G-1 (probe J6): while the daemon's drain waits on an accepted
/// write in flight (generation A), another run on the same tracker
/// (generation B) reaches its verdict. Once shutdown has begun, B must
/// write nothing and record its cycle as deferred.
#[test]
fn a_run_that_starts_while_the_drain_waits_writes_nothing() {
    for run in 0..10 {
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();
        let (root_a, root_b) = (dir_a.path().to_path_buf(), dir_b.path().to_path_buf());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let report_b = runtime.block_on({
            let (root_a, root_b) = (root_a.clone(), root_b.clone());
            async move {
                let commits = CommitTracker::default();
                let pipeline_a = auto_pipeline_batched(&root_a, Some(Arc::new(ScoringJudge(0.9))), 64)
                    .await
                    .with_commit_tracker(commits.clone());
                let pipeline_b = auto_pipeline_batched(&root_b, Some(Arc::new(ScoringJudge(0.9))), 64)
                    .await
                    .with_commit_tracker(commits.clone());
                fs::write(root_a.join("evolution_config.toml"), b"original")
                    .await
                    .unwrap();
                fs::write(root_b.join("evolution_config.toml"), b"original")
                    .await
                    .unwrap();
                let started = Arc::new(tokio::sync::Notify::new());
                let mut engine_a = SlowProposingEngine {
                    workspace: root_a.clone(),
                    started: Arc::clone(&started),
                };
                let worker_a = tokio::spawn(async move {
                    let _ = pipeline_a
                        .run_for_layer(
                            EvolutionTrigger::CronTick,
                            EvolutionLayer::Prompt,
                            &mut engine_a,
                            Utc::now(),
                        )
                        .await;
                });
                started.notified().await;
                // Shutdown: the supervisors are aborted and the drain begins.
                worker_a.abort();
                let drain = daemon_teardown(&commits);
                let late = async {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    let mut engine_b = ProposingEngine {
                        workspace: root_b.clone(),
                    };
                    pipeline_b
                        .run_for_layer(
                            EvolutionTrigger::CronTick,
                            EvolutionLayer::Prompt,
                            &mut engine_b,
                            Utc::now(),
                        )
                        .await
                };
                let ((), report_b) = tokio::join!(drain, late);
                report_b
            }
        });
        runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        let report_b = report_b.expect("test: the late run reports");
        assert_eq!(
            std::fs::read_to_string(root_b.join("evolution_config.toml")).unwrap_or_default(),
            "original",
            "run {run}: the late run wrote after shutdown began"
        );
        assert_eq!(
            report_b.commit,
            CommitDecision::Deferred,
            "run {run}: {:?}",
            report_b.errors
        );
        assert!(
            persisted_row_count_sync(&root_b.join("logs")) > 0,
            "run {run}: not recorded"
        );
        // A's accepted write was already running: it completes.
        assert_eq!(
            std::fs::read_to_string(root_a.join("evolution_config.toml")).unwrap_or_default(),
            "changed by the engine",
            "run {run}"
        );
    }
}

/// R-PMMCj G-6: when the change was written but its row cannot be, the
/// run still reports the commit (Applied) with a `record_failed` error,
/// not a failed run with no commit.
#[tokio::test]
async fn a_row_that_cannot_be_written_does_not_hide_an_applied_change() {
    let dir = tempdir().unwrap();
    let pipeline = auto_pipeline_batched(dir.path(), Some(Arc::new(ScoringJudge(0.9))), 64).await;
    fs::write(dir.path().join("evolution_config.toml"), b"original")
        .await
        .unwrap();
    // The evolution log file for today (and tomorrow, across midnight) is
    // a directory, so the row cannot be written.
    for day in [Utc::now(), Utc::now() + chrono::Duration::days(1)] {
        fs::create_dir_all(
            dir.path()
                .join("logs/evolution/hot")
                .join(format!("{}.jsonl", day.format("%Y-%m-%d"))),
        )
        .await
        .unwrap();
    }
    let mut engine = ProposingEngine {
        workspace: dir.path().to_path_buf(),
    };
    let report = pipeline
        .run_for_layer(
            EvolutionTrigger::Manual,
            EvolutionLayer::Prompt,
            &mut engine,
            Utc::now(),
        )
        .await
        .expect("an applied change is reported even when its row fails");
    assert_eq!(report.commit, CommitDecision::Applied, "{:?}", report.errors);
    assert!(
        report.errors.iter().any(|error| error.contains("record_failed")),
        "{:?}",
        report.errors
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("evolution_config.toml"))
            .await
            .unwrap(),
        "changed by the engine"
    );
}

/// R-PMMCj G-5: "after its verdict" only when a model judge gave one.
#[test]
fn the_cancellation_note_mentions_a_verdict_only_when_there_was_one() {
    for outcome in [JudgeOutcome::Failed, JudgeOutcome::Degraded] {
        for commit in [CommitDecision::Deferred, CommitDecision::NothingProposed] {
            let note = cancellation_note(false, outcome, commit);
            assert!(note.contains("gave no verdict"), "{outcome:?} {commit:?}: {note}");
            assert!(!note.contains("its verdict"), "{outcome:?} {commit:?}: {note}");
        }
    }
    assert_eq!(
        cancellation_note(false, JudgeOutcome::Scored, CommitDecision::Deferred),
        "after its verdict and before any write: nothing was written"
    );
    assert_eq!(
        cancellation_note(true, JudgeOutcome::Failed, CommitDecision::Deferred),
        "before the judge answered: nothing was written"
    );
}

/// Carried from R-PMMCg: a generic pending-file cycle is recorded once,
/// by the pipeline, preserving the deleted strategy engine's invariant.
#[tokio::test]
async fn generic_file_change_cycle_is_recorded_once() {
    let dir = tempdir().unwrap();
    let (judge, _seen) = snapshot_judge(dir.path().join("decision_policy.toml"), Some(0.9));
    let pipeline = auto_pipeline(dir.path(), Some(judge)).await;
    let mut engine = pending_legacy_policy_file(dir.path(), &pipeline).await;
    pipeline
        .run_for_layer(
            EvolutionTrigger::Manual,
            EvolutionLayer::Prompt,
            &mut engine,
            Utc::now(),
        )
        .await
        .unwrap();
    let rows = persisted_evolution_rows(&dir.path().join("logs")).await;
    assert_eq!(
        rows.iter().filter(|row| row.layer == EvolutionLayer::Prompt).count(),
        1,
        "{rows:?}"
    );
}

#[tokio::test]
async fn pipeline_selects_top_candidate_and_propagates_experiment_id() {
    let dir = tempdir().unwrap();
    let storage_root = dir.path().join("logs");
    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            JsonlRetentionPolicy::default(),
            1,
        )
        .await
        .unwrap(),
    );

    let decision = crate::self_system::evolution::record::DecisionLog {
        timestamp: Utc::now().to_rfc3339(),
        experiment_id: "exp-a".to_string(),
        trace_id: "trace-a".to_string(),
        decision_type: DecisionType::ToolSelection,
        task_type: TaskType::ToolCall,
        risk_level: 1,
        actor: Actor::Agent,
        input_context: "ctx".to_string(),
        action_taken: "act".to_string(),
        outcome: Outcome::Success,
        tokens_used: 1,
        latency_ms: 1,
        user_correction: None,
        config_snapshot_hash: "cfg".to_string(),
    };
    writer.append_decision(&decision).await.unwrap();

    let memory = crate::self_system::evolution::record::MemoryAccessLog {
        timestamp: Utc::now().to_rfc3339(),
        experiment_id: "exp-a".to_string(),
        trace_id: "trace-a".to_string(),
        action: MemoryAction::Read,
        memory_id: "m1".to_string(),
        task_context: "ctx".to_string(),
        task_type: TaskType::ToolCall,
        actor: Actor::Agent,
        was_useful: Some(true),
        useful_annotation_source: Some(AnnotationSource::AutoEvaluator),
        annotation_confidence: Some(0.8),
        tokens_consumed: 1,
    };
    writer.append_memory_access(&memory).await.unwrap();
    writer.flush().await.unwrap();

    let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.path().join("analysis")));
    let mut cfg = EvolutionConfig::default();
    cfg.runtime.mode = EvolutionMode::Auto;
    cfg.runtime.storage_dir = storage_root.to_string_lossy().to_string();
    let shared = new_shared_evolution_config(cfg);

    let pipeline = EvolutionPipeline::new(shared, analyzer, writer.clone(), dir.path());
    let mut engine = MockEngine;
    let now = Utc::now();

    let _ = pipeline
        .analyzer
        .generate_daily_digest(now - chrono::Duration::days(2))
        .await;
    let _ = pipeline
        .analyzer
        .generate_daily_digest(now - chrono::Duration::days(1))
        .await;
    let _ = pipeline.analyzer.generate_daily_digest(now).await;

    let mut trend = pipeline
        .analyzer
        .generate_three_day_trend(now.date_naive())
        .await
        .unwrap_or_else(|_| TrendAnalysis {
            start_date: "".to_string(),
            end_date: "".to_string(),
            digests: Vec::new(),
            noise_memories: Vec::new(),
            weakest_task_type: None,
            lowest_efficiency_config: None,
            user_correction_clusters: Vec::new(),
            candidates: Vec::new(),
        });
    if trend.candidates.is_empty() {
        let mut target = BTreeMap::new();
        target.insert("task_type".to_string(), "tool_call".to_string());
        trend.candidates.push(EvolutionCandidate {
            target,
            current_value: "x".to_string(),
            suggested_value: "y".to_string(),
            evidence_ids: vec!["trace-a".to_string()],
            priority: CandidatePriority::High,
            backfill_after_days: 3,
        });
    }
    drop(trend);

    let report = pipeline
        .run_for_layer(EvolutionTrigger::Manual, EvolutionLayer::Prompt, &mut engine, now)
        .await
        .unwrap();

    assert!(report.selected_candidate.is_some());
    assert_eq!(report.experiment_id.len(), 36);
    assert_eq!(
        report.evolution_log.as_ref().map(|item| item.experiment_id.as_str()),
        Some(report.experiment_id.as_str())
    );
}

async fn gated_pipeline(dir: &std::path::Path, policy: Arc<SecurityPolicy>) -> EvolutionPipeline {
    let storage_root = dir.join("logs");
    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            JsonlRetentionPolicy::default(),
            1,
        )
        .await
        .unwrap(),
    );
    let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.join("analysis")));
    let mut cfg = EvolutionConfig::default();
    cfg.runtime.storage_dir = storage_root.to_string_lossy().to_string();
    let shared = new_shared_evolution_config(cfg);
    EvolutionPipeline::new(shared, analyzer, writer, dir).with_security_policy(policy)
}

#[tokio::test]
async fn authorize_commit_is_noop_without_policy() {
    let dir = tempdir().unwrap();
    let storage_root = dir.path().join("logs");
    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            JsonlRetentionPolicy::default(),
            1,
        )
        .await
        .unwrap(),
    );
    let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.path().join("analysis")));
    let mut cfg = EvolutionConfig::default();
    cfg.runtime.storage_dir = storage_root.to_string_lossy().to_string();
    let shared = new_shared_evolution_config(cfg);
    // No policy installed → legacy behaviour, commit authorization is a no-op.
    let pipeline = EvolutionPipeline::new(shared, analyzer, writer, dir.path());
    assert!(pipeline.authorize_commit(&EvolutionLayer::Prompt, "exp-x").is_ok());
}

#[tokio::test]
async fn authorize_commit_blocks_under_supervised_policy_without_grant() {
    let dir = tempdir().unwrap();
    // Explicit Supervised policy requires a runtime grant for this
    // Medium-risk evolution commit.
    let policy = Arc::new(SecurityPolicy {
        autonomy: crate::security::AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    });
    let pipeline = gated_pipeline(dir.path(), policy).await;
    let reason = pipeline
        .authorize_commit(&EvolutionLayer::Prompt, "exp-blocked")
        .expect_err("supervised policy must block ungranted evolution commit");
    // The raw gate deny reason is surfaced so the caller can record it as a
    // structured GateRejection detail.
    assert!(
        reason.contains("requires runtime approval grant"),
        "unexpected deny reason: {reason}"
    );
}

#[tokio::test]
async fn authorize_commit_allows_under_full_policy() {
    let dir = tempdir().unwrap();
    // Full autonomy → no medium-risk approval requirement, commit authorized.
    let policy = Arc::new(SecurityPolicy {
        autonomy: crate::security::AutonomyLevel::Full,
        ..SecurityPolicy::default()
    });
    let pipeline = gated_pipeline(dir.path(), policy).await;
    assert!(pipeline.authorize_commit(&EvolutionLayer::Prompt, "exp-ok").is_ok());
}

#[tokio::test]
async fn backfill_results_waits_for_file_lock_and_updates_atomically() {
    let dir = tempdir().unwrap();
    let storage_root = dir.path().join("logs");
    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            JsonlRetentionPolicy::default(),
            1,
        )
        .await
        .unwrap(),
    );
    let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.path().join("analysis")));
    let mut cfg = EvolutionConfig::default();
    cfg.runtime.storage_dir = storage_root.to_string_lossy().to_string();
    let shared = new_shared_evolution_config(cfg);
    let pipeline = EvolutionPipeline::new(shared, analyzer, writer, dir.path());
    store_neutral_fitness_backfill(dir.path());

    let path = storage_root.join("evolution/hot/2026-02-20.jsonl");
    fs::create_dir_all(path.parent().unwrap()).await.unwrap();
    let line = r#"{"experiment_id":"exp-lock","timestamp":"2026-02-20T00:00:00Z","layer":"memory","change_type":"tune","before_value":"a","after_value":"b","trigger_reason":"r","data_basis":{"sample_count":1,"time_range_days":1,"key_metrics":{},"patterns_found":[]},"result":null}"#;
    fs::write(&path, format!("{line}\n")).await.unwrap();
    let source_before = fs::read_to_string(&path).await.unwrap();

    // FIX-P1-09: backfill now appends to a derived result_history.jsonl under file lock,
    // never mutating the source evolution jsonl in place.
    let history_path = storage_root.join("evolution").join("result_history.jsonl");
    fs::write(&history_path, "").await.unwrap();
    let guard = acquire_file_lock(&history_path).await.unwrap();
    let now = chrono::DateTime::parse_from_rfc3339("2026-02-24T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    let task = tokio::spawn({
        let pipeline = pipeline;
        async move { pipeline.backfill_results(now).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert!(!task.is_finished());
    drop(guard);

    let updated = task.await.unwrap().unwrap();
    assert_eq!(updated, 1);

    // Derived history holds the inferred result; source jsonl is byte-for-byte unchanged.
    let history = fs::read_to_string(&history_path).await.unwrap();
    assert!(history.contains("\"result\":\"neutral\""));
    assert!(history.contains("\"experiment_id\":\"exp-lock\""));
    let source_after = fs::read_to_string(&path).await.unwrap();
    assert_eq!(source_before, source_after);
}

#[tokio::test]
async fn backfill_results_is_idempotent_across_reruns() {
    let dir = tempdir().unwrap();
    let storage_root = dir.path().join("logs");
    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            JsonlRetentionPolicy::default(),
            1,
        )
        .await
        .unwrap(),
    );
    let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.path().join("analysis")));
    let mut cfg = EvolutionConfig::default();
    cfg.runtime.storage_dir = storage_root.to_string_lossy().to_string();
    let shared = new_shared_evolution_config(cfg);
    let pipeline = EvolutionPipeline::new(shared, analyzer, writer, dir.path());
    store_neutral_fitness_backfill(dir.path());

    let path = storage_root.join("evolution/hot/2026-02-20.jsonl");
    fs::create_dir_all(path.parent().unwrap()).await.unwrap();
    let line = r#"{"experiment_id":"exp-once","timestamp":"2026-02-20T00:00:00Z","layer":"memory","change_type":"tune","before_value":"a","after_value":"b","trigger_reason":"r","data_basis":{"sample_count":1,"time_range_days":1,"key_metrics":{},"patterns_found":[]},"result":null}"#;
    fs::write(&path, format!("{line}\n")).await.unwrap();

    let now = chrono::DateTime::parse_from_rfc3339("2026-02-24T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    let first = pipeline.backfill_results(now).await.unwrap();
    let second = pipeline.backfill_results(now).await.unwrap();
    assert_eq!(first, 1);
    assert_eq!(second, 0, "already-backfilled experiment must not be re-appended");

    let history_path = storage_root.join("evolution").join("result_history.jsonl");
    let history = fs::read_to_string(&history_path).await.unwrap();
    let count = history.lines().filter(|l| l.contains("exp-once")).count();
    assert_eq!(count, 1);
}

/// EVO-JSONL: the backfill scan skips an unreadable `*.jsonl` entry (here
/// a directory with that name) instead of failing the whole run, and
/// still backfills the readable file next to it.
#[tokio::test]
async fn backfill_results_skips_an_unreadable_jsonl_entry() {
    let dir = tempdir().unwrap();
    let storage_root = dir.path().join("logs");
    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            JsonlRetentionPolicy::default(),
            1,
        )
        .await
        .unwrap(),
    );
    let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.path().join("analysis")));
    let mut cfg = EvolutionConfig::default();
    cfg.runtime.storage_dir = storage_root.to_string_lossy().to_string();
    let shared = new_shared_evolution_config(cfg);
    let pipeline = EvolutionPipeline::new(shared, analyzer, writer, dir.path());
    store_neutral_fitness_backfill(dir.path());

    let hot = storage_root.join("evolution/hot");
    fs::create_dir_all(hot.join("2026-01-01.jsonl")).await.unwrap();
    let line = r#"{"experiment_id":"exp-readable","timestamp":"2026-02-20T00:00:00Z","layer":"memory","change_type":"tune","before_value":"a","after_value":"b","trigger_reason":"r","data_basis":{"sample_count":1,"time_range_days":1,"key_metrics":{},"patterns_found":[]},"result":null}"#;
    fs::write(hot.join("2026-02-20.jsonl"), format!("{line}\n"))
        .await
        .unwrap();

    let now = chrono::DateTime::parse_from_rfc3339("2026-02-24T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let updated = pipeline
        .backfill_results(now)
        .await
        .expect("test: an unreadable entry must not fail the backfill");
    assert_eq!(updated, 1);
}
