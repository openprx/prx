// Evolution CLI: all functions in this module print user-facing status/history output.
#![allow(clippy::print_stdout)]

use crate::config::Config;
use crate::self_system::evolution::{
    AsyncJsonlWriter, CircuitBreakerState, EvolutionAnalyzer, EvolutionConfig, EvolutionLayer, EvolutionLog,
    EvolutionMode, EvolutionPipeline, EvolutionResult, EvolutionRetentionConfig, EvolutionRuntimeConfig,
    EvolutionTrigger, JsonlRetentionPolicy, JsonlStoragePaths, MemoryAccessLog, MemoryEvolutionEngine,
    PromptEvolutionEngine, StrategyEvolutionEngine, new_shared_evolution_config,
};
use crate::{EvolutionCommands, EvolutionLayerArg};
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;

const DECISION_PROGRESS_THRESHOLD: usize = 800;
const MEMORY_PROGRESS_THRESHOLD: usize = 200;

#[derive(Debug, Serialize)]
struct StatusOutput {
    mode: EvolutionMode,
    data_progress: DataProgress,
    recent_cycles: Vec<CycleSummary>,
    circuit_breaker: CircuitBreakerState,
    layer_freeze: LayerFreezeStatus,
}

#[derive(Debug, Serialize)]
struct DataProgress {
    decision_logs: usize,
    decision_threshold: usize,
    memory_access_logs: usize,
    memory_threshold: usize,
}

#[derive(Debug, Serialize)]
struct CycleSummary {
    timestamp: String,
    layer: EvolutionLayer,
    change: String,
    result: Option<EvolutionResult>,
}

#[derive(Debug, Serialize)]
struct LayerFreezeStatus {
    memory: bool,
    prompt: bool,
    policy: bool,
}

#[derive(Debug, Serialize)]
struct HistoryOutput {
    items: Vec<EvolutionLog>,
}

#[derive(Debug, Serialize)]
struct DigestOutput {
    date: String,
    digest: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ConfigOutput {
    source: String,
    exists: bool,
    config: EvolutionConfig,
}

#[derive(Debug, Serialize)]
struct TriggerOutput {
    config_path: String,
    layer: EvolutionLayer,
    report: crate::self_system::evolution::PipelineRunReport,
}

pub async fn handle_command(command: EvolutionCommands, as_json: bool, config: &Config) -> Result<()> {
    match command {
        EvolutionCommands::Status => handle_status(as_json, config).await,
        EvolutionCommands::History { limit } => handle_history(as_json, config, limit).await,
        EvolutionCommands::Digest { date } => handle_digest(as_json, config, date).await,
        EvolutionCommands::Config => handle_config(as_json, config).await,
        EvolutionCommands::Trigger { layer } => handle_trigger(as_json, config, layer).await,
    }
}

async fn handle_status(as_json: bool, config: &Config) -> Result<()> {
    let (cfg, _path, _exists) = load_evolution_config(config).await?;
    let storage_root = resolve_storage_root(config, &cfg.runtime);

    let decisions =
        read_all_jsonl::<crate::self_system::evolution::DecisionLog>(&storage_root.join("decisions")).await?;
    let memory = read_all_jsonl::<MemoryAccessLog>(&storage_root.join("memory_access")).await?;
    let mut evolution = read_all_jsonl::<EvolutionLog>(&storage_root.join("evolution")).await?;

    evolution.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    let recent_cycles = evolution
        .iter()
        .take(3)
        .map(|item| CycleSummary {
            timestamp: item.timestamp.clone(),
            layer: item.layer.clone(),
            change: debug_value(&item.change_type),
            result: item.result.clone(),
        })
        .collect::<Vec<_>>();

    let breaker_state = infer_circuit_state(&evolution, &cfg, Utc::now());
    let frozen = matches!(breaker_state, CircuitBreakerState::Open);

    let payload = StatusOutput {
        mode: cfg.runtime.mode,
        data_progress: DataProgress {
            decision_logs: decisions.len(),
            decision_threshold: DECISION_PROGRESS_THRESHOLD,
            memory_access_logs: memory.len(),
            memory_threshold: MEMORY_PROGRESS_THRESHOLD,
        },
        recent_cycles,
        circuit_breaker: breaker_state,
        layer_freeze: LayerFreezeStatus {
            memory: frozen,
            prompt: frozen,
            policy: frozen,
        },
    };

    if as_json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("Evolution Status");
    println!("==============");
    println!("Mode          : {:?}", payload.mode);
    println!(
        "DecisionLog   : {}/{}",
        payload.data_progress.decision_logs, payload.data_progress.decision_threshold
    );
    println!(
        "MemoryAccess  : {}/{}",
        payload.data_progress.memory_access_logs, payload.data_progress.memory_threshold
    );
    println!("CircuitBreaker: {:?}", payload.circuit_breaker);
    println!();

    println!("Recent Cycles");
    println!("-------------");
    if payload.recent_cycles.is_empty() {
        println!("(no evolution cycle logs found)");
    } else {
        println!("{:<26} {:<10} {:<12} Result", "Timestamp", "Layer", "Change");
        for row in &payload.recent_cycles {
            let result = row.result.as_ref().map_or_else(|| "unknown".to_string(), debug_value);
            println!(
                "{:<26} {:<10} {:<12} {}",
                row.timestamp,
                debug_value(&row.layer),
                row.change,
                result
            );
        }
    }

    println!();
    println!("Layer Freeze");
    println!("------------");
    println!("memory : {}", bool_flag(payload.layer_freeze.memory));
    println!("prompt : {}", bool_flag(payload.layer_freeze.prompt));
    println!("policy : {}", bool_flag(payload.layer_freeze.policy));

    Ok(())
}

async fn handle_history(as_json: bool, config: &Config, limit: usize) -> Result<()> {
    let (cfg, _, _) = load_evolution_config(config).await?;
    let storage_root = resolve_storage_root(config, &cfg.runtime);

    let mut items = read_all_jsonl::<EvolutionLog>(&storage_root.join("evolution")).await?;
    items.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    items.truncate(limit.max(1));

    let payload = HistoryOutput { items };
    if as_json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("Evolution History (limit={})", limit.max(1));
    println!("===========================");
    if payload.items.is_empty() {
        println!("(no history records found)");
        return Ok(());
    }

    println!(
        "{:<26} {:<10} {:<10} {:<10} Trigger",
        "Timestamp", "Layer", "Change", "Result"
    );
    for item in &payload.items {
        let result = item.result.as_ref().map_or_else(|| "unknown".to_string(), debug_value);
        println!(
            "{:<26} {:<10} {:<10} {:<10} {}",
            item.timestamp,
            debug_value(&item.layer),
            debug_value(&item.change_type),
            result,
            item.trigger_reason
        );
    }

    Ok(())
}

async fn handle_digest(as_json: bool, config: &Config, date: Option<String>) -> Result<()> {
    let target_date = date
        .as_deref()
        .map(parse_date)
        .transpose()?
        .unwrap_or_else(|| Utc::now().date_naive());

    let (cfg, _, _) = load_evolution_config(config).await?;
    let storage_root = resolve_storage_root(config, &cfg.runtime);
    let digest_path = storage_root
        .join("analysis")
        .join("daily")
        .join(format!("{target_date}.json"));

    let raw = fs::read_to_string(&digest_path)
        .await
        .with_context(|| format!("daily digest not found: {}", digest_path.display()))?;
    let digest_json = serde_json::from_str::<serde_json::Value>(&raw)?;

    let payload = DigestOutput {
        date: target_date.to_string(),
        digest: digest_json,
    };

    if as_json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("Daily Digest: {}", payload.date);
    println!("=================");
    println!("{}", serde_json::to_string_pretty(&payload.digest)?);
    Ok(())
}

async fn handle_config(as_json: bool, config: &Config) -> Result<()> {
    let (cfg, path, exists) = load_evolution_config(config).await?;
    let payload = ConfigOutput {
        source: path.display().to_string(),
        exists,
        config: cfg,
    };

    if as_json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("Evolution Config");
    println!("================");
    println!("source: {}", payload.source);
    println!("exists: {}", bool_flag(payload.exists));
    println!();
    println!("{}", toml::to_string_pretty(&payload.config)?);
    Ok(())
}

async fn handle_trigger(as_json: bool, config: &Config, layer: Option<EvolutionLayerArg>) -> Result<()> {
    let (cfg, cfg_path, _exists) = load_evolution_config(config).await?;
    let shared = new_shared_evolution_config(cfg.clone());

    let storage_root = resolve_storage_root(config, &cfg.runtime);
    let writer = Arc::new(
        AsyncJsonlWriter::new(
            JsonlStoragePaths::new(storage_root.clone()),
            retention_from_runtime(&cfg.runtime.retention),
            cfg.runtime.batch_size,
        )
        .await?,
    );

    let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), storage_root.join("analysis")));
    let writer_for_engine = writer.clone();

    let selected_layer = map_layer(layer);
    // FIX-P0-40: the manual `prx evolution trigger` path is a production entry
    // point that drives the same engines (config/strategy/prompt writes +
    // `append_evolution`) as the daemon scheduler. It MUST pass every commit
    // through the same `SideEffectGate`; otherwise a manual trigger fully bypasses
    // the autonomy gate the daemon enforces. Build the runtime security policy from
    // config exactly like `daemon::build_evolution_scheduler` (honouring
    // `security.audit`, FIX-P1-31) and install it on the pipeline so the engine is
    // never reached on a deny decision.
    let security_policy = Arc::new(
        crate::security::SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)
            .with_audit_config(config.security.audit.clone()),
    );
    let pipeline = EvolutionPipeline::new(shared.clone(), analyzer, writer, &config.workspace_dir)
        .with_security_policy(security_policy);

    let report = match selected_layer {
        EvolutionLayer::Memory => {
            let mut engine = MemoryEvolutionEngine::new(shared, &cfg_path, Some(writer_for_engine.clone()))?;
            pipeline
                .run_for_layer(
                    EvolutionTrigger::Manual,
                    EvolutionLayer::Memory,
                    &mut engine,
                    Utc::now(),
                )
                .await?
        }
        EvolutionLayer::Prompt => {
            let mut engine = PromptEvolutionEngine::new(shared, &config.workspace_dir, Some(writer_for_engine.clone()))
                .with_debug_raw(config.self_system.evolution_debug_raw);
            pipeline
                .run_for_layer(
                    EvolutionTrigger::Manual,
                    EvolutionLayer::Prompt,
                    &mut engine,
                    Utc::now(),
                )
                .await?
        }
        EvolutionLayer::Policy => {
            let mut engine = StrategyEvolutionEngine::new(shared, &config.workspace_dir, writer_for_engine)?;
            pipeline
                .run_for_layer(
                    EvolutionTrigger::Manual,
                    EvolutionLayer::Policy,
                    &mut engine,
                    Utc::now(),
                )
                .await?
        }
        other => anyhow::bail!("manual trigger only supports L1/L2/L3, got: {other:?}"),
    };

    // FIX-P0-40: the side-effect gate may have denied this commit (e.g. Supervised
    // autonomy with `require_approval_for_medium_risk` and no runtime grant). The
    // one-shot CLI has no interactive approval manager to issue a grant, so the
    // only correct behaviour is to surface the denial clearly and exit non-zero —
    // never silently report a "completed" trigger that actually applied nothing.
    let gate_denied = report.gate_denied;
    let gate_detail = report
        .gate_rejections
        .iter()
        .find(|rejection| rejection.reason == "side_effect_gate_denied")
        .map(|rejection| rejection.details.clone());
    // Render the configured autonomy level as its config-file string (lowercase
    // serde) so the remediation hint matches what the user would type in config.
    let autonomy_level = serde_json::to_value(config.autonomy.level)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "supervised".to_string());

    let payload = TriggerOutput {
        config_path: cfg_path.display().to_string(),
        layer: selected_layer,
        report,
    };

    if as_json {
        // Emit the full report first so programmatic callers can read
        // `gate_denied`/`gate_rejections`, then signal failure via a non-zero exit.
        println!("{}", serde_json::to_string_pretty(&payload)?);
        if gate_denied {
            return Err(gate_denied_error(&autonomy_level));
        }
        return Ok(());
    }

    println!("Manual Evolution Trigger");
    println!("========================");
    println!("config : {}", payload.config_path);
    println!("layer  : {}", debug_value(&payload.layer));
    println!("id     : {}", payload.report.experiment_id);
    println!("shadow : {}", bool_flag(payload.report.shadow_mode));
    println!("rolled : {}", bool_flag(payload.report.rolled_back));
    if !payload.report.errors.is_empty() {
        println!("errors : {}", payload.report.errors.join(" | "));
    }

    if gate_denied {
        println!();
        println!("status : BLOCKED by autonomy side-effect gate");
        if let Some(detail) = &gate_detail {
            println!("reason : {detail}");
        }
        println!("note   : no change was applied — the evolution engine never ran and nothing was written to disk.");
        println!("         Current autonomy level is `{autonomy_level}`, which gates self-modifications.");
        println!("how to allow:");
        println!("         - Raise the autonomy level to `full` in config ([autonomy] level = \"full\"),");
        println!(
            "           or disable approval-for-medium-risk ([autonomy] require_approval_for_medium_risk = false),"
        );
        println!("           then re-run `prx evolution trigger`.");
        println!("         - The daemon scheduler applies the same gate; this denial is by design (fail-closed).");
        return Err(gate_denied_error(&autonomy_level));
    }

    Ok(())
}

/// FIX-P0-40: build the error returned when the side-effect gate denied a manual
/// evolution trigger. Surfacing an `Err` from the command handler gives the
/// process a non-zero exit code so scripts and callers can detect that no
/// self-modification was applied, instead of treating a denied trigger as success.
fn gate_denied_error(autonomy_level: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "evolution trigger blocked by autonomy side-effect gate (autonomy level `{autonomy_level}`); \
         no change was applied. Raise autonomy to `full` or set \
         `require_approval_for_medium_risk = false` to allow manual evolution."
    )
}

const fn map_layer(layer: Option<EvolutionLayerArg>) -> EvolutionLayer {
    match layer {
        Some(EvolutionLayerArg::L1) | None => EvolutionLayer::Memory,
        Some(EvolutionLayerArg::L2) => EvolutionLayer::Prompt,
        Some(EvolutionLayerArg::L3) => EvolutionLayer::Policy,
    }
}

const fn retention_from_runtime(retention: &EvolutionRetentionConfig) -> JsonlRetentionPolicy {
    JsonlRetentionPolicy {
        hot_days: retention.hot_days,
        warm_days: retention.warm_days,
        cold_days: retention.cold_days,
    }
}

async fn load_evolution_config(config: &Config) -> Result<(EvolutionConfig, PathBuf, bool)> {
    let path = discover_evolution_config_path(config);
    let exists = fs::metadata(&path).await.is_ok();
    if exists {
        let cfg = EvolutionConfig::load_from_path(&path).await?;
        Ok((cfg, path, true))
    } else {
        Ok((EvolutionConfig::default(), path, false))
    }
}

fn discover_evolution_config_path(config: &Config) -> PathBuf {
    if let Some(raw) = config.self_system.evolution_config_path.as_deref() {
        let p = PathBuf::from(raw);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }

    let candidates = [
        config.workspace_dir.join("evolution_config.toml"),
        PathBuf::from("evolution_config.toml"),
        PathBuf::from("config/evolution_config.toml"),
    ];

    for path in &candidates {
        if path.exists() {
            return path.clone();
        }
    }

    candidates[0].clone()
}

fn resolve_storage_root(config: &Config, runtime: &EvolutionRuntimeConfig) -> PathBuf {
    let root = Path::new(&runtime.storage_dir);
    if root.is_absolute() {
        root.to_path_buf()
    } else {
        config.workspace_dir.join(root)
    }
}

fn infer_circuit_state(
    evolution_logs: &[EvolutionLog],
    cfg: &EvolutionConfig,
    now: DateTime<Utc>,
) -> CircuitBreakerState {
    let threshold = cfg.rollback.circuit_breaker_threshold.max(1) as usize;
    let cooldown_secs = cfg.rollback.cooldown_after_rollback_hours.max(1) * 3600;

    let failures = evolution_logs
        .iter()
        .take_while(|item| {
            matches!(
                item.result,
                Some(EvolutionResult::Regressed | EvolutionResult::Rejected)
            )
        })
        .take(threshold)
        .collect::<Vec<_>>();

    if failures.len() < threshold {
        return CircuitBreakerState::Closed;
    }

    let opened_at = failures
        .last()
        .and_then(|item| parse_ts(&item.timestamp))
        .unwrap_or(now);

    if now.signed_duration_since(opened_at).num_seconds() < cooldown_secs as i64 {
        CircuitBreakerState::Open
    } else {
        CircuitBreakerState::HalfOpen
    }
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|item| item.with_timezone(&Utc))
}

fn parse_date(raw: &str) -> Result<NaiveDate> {
    Ok(NaiveDate::parse_from_str(raw, "%Y-%m-%d")?)
}

async fn read_all_jsonl<T>(base: &Path) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned,
{
    let mut out = Vec::new();
    for tier in ["hot", "warm", "cold"] {
        let dir = base.join(tier);
        if fs::metadata(&dir).await.is_err() {
            continue;
        }

        let mut rd = fs::read_dir(&dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|v| v.to_str()) != Some("jsonl") {
                continue;
            }

            let raw = fs::read_to_string(path).await?;
            for line in raw.lines().filter(|line| !line.trim().is_empty()) {
                if let Ok(parsed) = serde_json::from_str::<T>(line) {
                    out.push(parsed);
                }
            }
        }
    }
    Ok(out)
}

const fn bool_flag(v: bool) -> &'static str {
    if v { "yes" } else { "no" }
}

fn debug_value<T: Debug>(v: &T) -> String {
    format!("{v:?}").to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_system::evolution::DecisionLog;
    use crate::self_system::evolution::record::{
        Actor, AnnotationSource, DecisionType, MemoryAction, Outcome, TaskType,
    };
    use chrono::Duration as ChronoDuration;
    use tempfile::TempDir;

    /// Build a minimal `Config` whose workspace points into `tmp`. The default
    /// autonomy level is `Supervised` with `require_approval_for_medium_risk`,
    /// which is exactly the policy that must DENY a manual evolution trigger.
    fn supervised_config(tmp: &TempDir) -> Config {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).expect("test: create workspace dir");
        config
    }

    /// Seed decision + memory-access logs into the same storage root the manual
    /// trigger resolves, so the analyzer deterministically produces a candidate
    /// and the pipeline actually reaches the side-effect gate.
    async fn seed_candidate_inputs(storage_root: &Path, now: DateTime<Utc>) {
        let writer = Arc::new(
            AsyncJsonlWriter::new(
                JsonlStoragePaths::new(storage_root.to_path_buf()),
                JsonlRetentionPolicy::default(),
                1,
            )
            .await
            .expect("test: build writer"),
        );
        for offset in 0..3 {
            let ts = (now - ChronoDuration::days(offset)).to_rfc3339();
            writer
                .append_decision(&DecisionLog {
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
                .expect("test: append decision");
            writer
                .append_memory_access(&MemoryAccessLog {
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
                .expect("test: append memory access");
        }
        writer.flush().await.expect("test: flush writer");
    }

    /// Count evolution JSONL lines written under `storage_root/evolution/*`.
    /// FIX-P0-40: a denied manual trigger must leave this at zero.
    async fn evolution_log_lines(storage_root: &Path) -> usize {
        let evo_root = storage_root.join("evolution");
        let mut lines = 0usize;
        for tier in ["hot", "warm", "cold"] {
            let tier_dir = evo_root.join(tier);
            if let Ok(mut rd) = fs::read_dir(&tier_dir).await {
                while let Ok(Some(entry)) = rd.next_entry().await {
                    if entry.path().extension().and_then(|v| v.to_str()) == Some("jsonl") {
                        let raw = fs::read_to_string(entry.path()).await.unwrap_or_default();
                        lines += raw.lines().filter(|line| !line.trim().is_empty()).count();
                    }
                }
            }
        }
        lines
    }

    /// FIX-P0-40 (manual path, hard acceptance): `prx evolution trigger` under the
    /// default Supervised autonomy must be DENIED by the side-effect gate, return a
    /// non-zero (Err) result, and never append an evolution log to disk. This
    /// proves the manual entry point is gated identically to the daemon scheduler
    /// and cannot bypass the autonomy gate.
    #[tokio::test]
    async fn manual_trigger_supervised_is_denied_and_writes_nothing() {
        let tmp = TempDir::new().expect("test: tempdir");
        let config = supervised_config(&tmp);

        // The manual trigger resolves storage as workspace_dir/<runtime.storage_dir>
        // (default "self/evolution"). Seed inputs there so a candidate is produced.
        let storage_root = config.workspace_dir.join("self/evolution");
        let now = Utc::now();
        seed_candidate_inputs(&storage_root, now).await;

        // Pre-compute digests so the three-day trend yields a candidate.
        let writer = Arc::new(
            AsyncJsonlWriter::new(
                JsonlStoragePaths::new(storage_root.clone()),
                JsonlRetentionPolicy::default(),
                1,
            )
            .await
            .expect("test: writer"),
        );
        let analyzer = EvolutionAnalyzer::new(writer, storage_root.join("analysis"));
        for offset in (0..3).rev() {
            let _ = analyzer.generate_daily_digest(now - ChronoDuration::days(offset)).await;
        }

        // Sanity: default autonomy is Supervised (the gating policy under test).
        assert_eq!(
            config.autonomy.level,
            crate::security::AutonomyLevel::Supervised,
            "test relies on default Supervised autonomy"
        );

        let result = handle_trigger(false, &config, Some(EvolutionLayerArg::L1)).await;

        // The manual trigger must fail (non-zero exit) because the gate denied it.
        let err = result.expect_err("supervised manual trigger must be denied by the side-effect gate");
        let msg = err.to_string();
        assert!(
            msg.contains("side-effect gate") || msg.contains("blocked"),
            "deny error must explain the gate blocked the trigger: {msg}"
        );

        // Fail-closed: no evolution log line may have been written.
        assert_eq!(
            evolution_log_lines(&storage_root).await,
            0,
            "a denied manual trigger must not append any evolution log line"
        );
    }

    /// FIX-P0-40: with the gate disabled (autonomy `Full`), the same manual
    /// trigger is allowed to proceed (it returns Ok), proving the deny in the test
    /// above is caused by the gate and not by a setup error that fails every path.
    #[tokio::test]
    async fn manual_trigger_full_autonomy_is_allowed() {
        let tmp = TempDir::new().expect("test: tempdir");
        let mut config = supervised_config(&tmp);
        config.autonomy.level = crate::security::AutonomyLevel::Full;

        let storage_root = config.workspace_dir.join("self/evolution");
        let now = Utc::now();
        seed_candidate_inputs(&storage_root, now).await;

        let result = handle_trigger(false, &config, Some(EvolutionLayerArg::L1)).await;

        // Full autonomy does not gate medium-risk commits, so the trigger runs to
        // completion (Ok) regardless of whether a candidate was ultimately applied.
        assert!(
            result.is_ok(),
            "Full autonomy must not be blocked by the side-effect gate: {:?}",
            result.err()
        );
    }
}
