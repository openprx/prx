use crate::config::Config;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Legacy (un-rotated) trace path. Retained as the historical sink name and as
/// the base used to derive the per-day rotated file names. New writes go to the
/// dated variant produced by [`dated_trace_path`].
pub const CONTROL_LADDER_TRACE_PATH: &str = "runtime/control_ladder_traces.jsonl";

/// Directory (relative to the workspace) that holds rotated trace files.
const CONTROL_LADDER_TRACE_DIR: &str = "runtime";
/// File-name prefix for rotated trace files: `control_ladder_traces.YYYY-MM-DD.jsonl`.
const CONTROL_LADDER_TRACE_PREFIX: &str = "control_ladder_traces.";
/// File-name suffix for rotated trace files.
const CONTROL_LADDER_TRACE_SUFFIX: &str = ".jsonl";
/// Retention window for rotated trace files. Files whose embedded date is older
/// than this many days are deleted by [`cleanup_old_control_ladder_traces`].
pub const CONTROL_LADDER_TRACE_RETENTION_DAYS: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlLayerTrace {
    pub level: u8,
    pub name: String,
    pub enabled: bool,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub detail: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlLadderTrace {
    pub trace_id: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub created_at: String,
    pub layers: Vec<ControlLayerTrace>,
    /// RouteDecision correlation id (d04 §10 G7). Joins this trace to the
    /// `router.route_decision` / `provider.final_outcome` timeline events.
    /// `None` for traces emitted before/without a routing decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    /// Provider that actually served the request after fallback resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_provider: Option<String>,
    /// Model that actually served the request after fallback resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_model: Option<String>,
    /// Number of provider attempts (1 = first-choice hit, >1 = fallback used).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts_count: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct ControlLadderSnapshot {
    layers: Vec<ControlLayerTrace>,
}

impl ControlLadderSnapshot {
    pub fn from_config(config: &Config) -> Self {
        let mut layers = vec![ControlLayerTrace {
            level: 0,
            name: "runtime".to_string(),
            enabled: true,
            status: "active".to_string(),
            reason: Some("l0_base_runtime".to_string()),
            detail: json!({
                "workspace": config.workspace_dir,
                "default_provider": config.default_provider,
                "default_model": config.default_model,
            }),
        }];

        layers.push(router_layer(config));
        layers.push(causal_tree_layer(config));
        layers.push(xin_layer(config));
        layers.push(self_system_layer(config));
        layers.push(task_pool_layer(config));

        Self { layers }
    }

    pub fn l0_only() -> Self {
        Self {
            layers: vec![ControlLayerTrace {
                level: 0,
                name: "runtime".to_string(),
                enabled: true,
                status: "active".to_string(),
                reason: Some("l0_base_runtime".to_string()),
                detail: json!({}),
            }],
        }
    }

    pub fn build_trace(&self, source: impl Into<String>, run_id: Option<String>) -> ControlLadderTrace {
        ControlLadderTrace {
            trace_id: Uuid::new_v4().to_string(),
            source: source.into(),
            run_id,
            created_at: Utc::now().to_rfc3339(),
            layers: self.layers.clone(),
            decision_id: None,
            final_provider: None,
            final_model: None,
            attempts_count: None,
        }
    }
}

impl Default for ControlLadderSnapshot {
    fn default() -> Self {
        Self::l0_only()
    }
}

impl ControlLadderTrace {
    /// Attach the RouteDecision / provider-execution correlation fields to this
    /// trace (d04 §10 G7). Populating these lets a `decision_id` join link the
    /// control-ladder trace to the `router.route_decision` /
    /// `provider.final_outcome` timeline events, and records which provider/model
    /// actually served the request after fallback resolution.
    #[must_use]
    pub fn with_provider_outcome(
        mut self,
        decision_id: impl Into<String>,
        final_provider: impl Into<String>,
        final_model: impl Into<String>,
        attempts_count: u8,
    ) -> Self {
        self.decision_id = Some(decision_id.into());
        self.final_provider = Some(final_provider.into());
        self.final_model = Some(final_model.into());
        self.attempts_count = Some(attempts_count);
        self
    }

    /// Set the RouteDecision correlation id on an existing trace in place
    /// (FIX-P1-13). Unlike [`Self::with_provider_outcome`] this is a `&mut`
    /// setter so the agent-turn path can stamp the `decision_id` onto a trace it
    /// is mutating incrementally, without consuming and rebuilding it.
    pub fn set_decision_id(&mut self, decision_id: impl Into<String>) {
        self.decision_id = Some(decision_id.into());
    }

    pub fn mark_active(&mut self, name: &str, reason: impl Into<String>, detail: Value) {
        self.update_layer(name, true, "active", Some(reason.into()), detail);
    }

    pub fn mark_fallback(&mut self, name: &str, reason: impl Into<String>, detail: Value) {
        self.update_layer(name, false, "fallback", Some(reason.into()), detail);
    }

    pub fn mark_skipped(&mut self, name: &str, reason: impl Into<String>, detail: Value) {
        self.update_layer(name, false, "skipped", Some(reason.into()), detail);
    }

    fn update_layer(&mut self, name: &str, enabled: bool, status: &str, reason: Option<String>, detail: Value) {
        if let Some(layer) = self.layers.iter_mut().find(|layer| layer.name == name) {
            layer.enabled = enabled;
            layer.status = status.to_string();
            layer.reason = reason;
            layer.detail = merge_detail(&layer.detail, detail);
        }
    }
}

/// Compute the rotated trace file path for `date` under `workspace_dir`:
/// `runtime/control_ladder_traces.YYYY-MM-DD.jsonl`.
#[must_use]
pub fn dated_trace_path(workspace_dir: &Path, date: DateTime<Utc>) -> PathBuf {
    let file_name = format!(
        "{CONTROL_LADDER_TRACE_PREFIX}{}{CONTROL_LADDER_TRACE_SUFFIX}",
        date.format("%Y-%m-%d")
    );
    workspace_dir.join(CONTROL_LADDER_TRACE_DIR).join(file_name)
}

/// Maximum time to wait for the trace lock before giving up.
const TRACE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Poll interval while spinning for the trace lock.
const TRACE_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// RAII guard for an exclusive cross-process lock implemented as a `.lock`
/// sidecar file (same pattern as `self_system::evolution::safety_utils`).
///
/// The lock is held for the lifetime of the guard; `Drop` removes the sidecar
/// so a crash that leaks the file only blocks for at most [`TRACE_LOCK_TIMEOUT`]
/// before the next writer reclaims it. The workspace denies `unsafe_code`, so an
/// `O_CREAT|O_EXCL` (`create_new`) sidecar is used instead of `flock(2)`.
struct TraceLockGuard {
    lock_path: PathBuf,
}

impl Drop for TraceLockGuard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.lock_path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(path = %self.lock_path.display(), error = %err, "failed to remove trace lock");
            }
        }
    }
}

/// Acquire an exclusive cross-process lock for the trace file at `trace_path`
/// via a `<trace_path>.lock` sidecar created with `create_new` (atomic
/// `O_CREAT|O_EXCL`). Synchronous (the trace write path is sync); spins with a
/// bounded wait so concurrent writers serialize instead of interleaving partial
/// lines. A stale sidecar from a crashed writer is reclaimed after the timeout.
fn acquire_trace_lock(trace_path: &Path) -> Result<TraceLockGuard> {
    let lock_path = trace_path.with_extension("jsonl.lock");
    let start = std::time::Instant::now();
    loop {
        match OpenOptions::new().create_new(true).write(true).open(&lock_path) {
            Ok(mut file) => {
                // Best-effort marker; failure to write the body does not affect
                // the lock semantics (existence is the lock).
                let _ = file.write_all(b"lock");
                return Ok(TraceLockGuard { lock_path });
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if start.elapsed() > TRACE_LOCK_TIMEOUT {
                    // Reclaim a presumed-stale lock from a crashed writer, then
                    // retry once on the next loop iteration.
                    match std::fs::remove_file(&lock_path) {
                        Ok(()) => {
                            tracing::warn!(path = %lock_path.display(), "reclaimed stale trace lock after timeout");
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(e).with_context(|| {
                                format!("failed to reclaim stale trace lock {}", lock_path.display())
                            });
                        }
                    }
                } else {
                    std::thread::sleep(TRACE_LOCK_POLL);
                }
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to open trace lock file {}", lock_path.display()));
            }
        }
    }
}

/// Append a control-ladder trace as one JSON line to the per-day rotated file
/// `runtime/control_ladder_traces.YYYY-MM-DD.jsonl` (rotation key = current UTC
/// date), serializing concurrent writers with an exclusive advisory file lock so
/// lines larger than `PIPE_BUF` cannot be torn by interleaved appends.
/// Returns the path actually written to.
pub fn append_control_ladder_trace(workspace_dir: &Path, trace: &ControlLadderTrace) -> Result<PathBuf> {
    let path = dated_trace_path(workspace_dir, Utc::now());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create control ladder trace directory {}", parent.display()))?;
    }

    // Hold the advisory lock across open+write so the append is atomic w.r.t.
    // other cooperating writers. Lock is released when `_lock` drops.
    let _lock = acquire_trace_lock(&path)?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open control ladder trace {}", path.display()))?;
    let line = serde_json::to_string(trace)?;
    writeln!(file, "{line}").with_context(|| format!("failed to append control ladder trace {}", path.display()))?;
    Ok(path)
}

/// Parse the embedded `YYYY-MM-DD` date from a rotated trace file name, e.g.
/// `control_ladder_traces.2026-05-31.jsonl`. Returns `None` for names that do
/// not match the rotated pattern (including the legacy un-dated file and the
/// `.lock` sidecars).
fn parse_trace_file_date(file_name: &str) -> Option<chrono::NaiveDate> {
    let rest = file_name.strip_prefix(CONTROL_LADDER_TRACE_PREFIX)?;
    let date_str = rest.strip_suffix(CONTROL_LADDER_TRACE_SUFFIX)?;
    chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()
}

/// Delete rotated control-ladder trace files whose embedded date is older than
/// [`CONTROL_LADDER_TRACE_RETENTION_DAYS`] days relative to `now`.
///
/// Best-effort and idempotent: a missing trace directory is treated as "nothing
/// to clean". Returns the number of files removed. The legacy un-dated file and
/// any non-matching files are left untouched. Intended to be wired into a xin
/// retention task by the caller (not wired here).
pub fn cleanup_old_control_ladder_traces(workspace_dir: &Path, now: DateTime<Utc>) -> Result<usize> {
    let dir = workspace_dir.join(CONTROL_LADDER_TRACE_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read trace directory {}", dir.display()));
        }
    };

    let cutoff = now.date_naive() - chrono::Duration::days(CONTROL_LADDER_TRACE_RETENTION_DAYS);
    let mut removed = 0usize;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read entry in {}", dir.display()))?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(file_date) = parse_trace_file_date(name) else {
            continue;
        };
        if file_date < cutoff {
            let path = entry.path();
            match std::fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "failed to remove old control ladder trace");
                }
            }
        }
    }
    Ok(removed)
}

/// Pure constructor for a provider-outcome control-ladder trace (d04 §10 G7).
/// Separated from IO so the field population is unit-testable without the FS.
/// The returned trace carries the structured `decision_id` / `final_provider` /
/// `final_model` / `attempts_count` correlation fields filled in.
#[must_use]
pub fn build_provider_outcome_trace(
    decision_id: &str,
    final_provider: &str,
    final_model: &str,
    attempts_count: u8,
    status: &str,
) -> ControlLadderTrace {
    ControlLadderTrace {
        trace_id: Uuid::new_v4().to_string(),
        source: "provider".to_string(),
        run_id: None,
        created_at: Utc::now().to_rfc3339(),
        layers: vec![ControlLayerTrace {
            level: 1,
            name: "provider".to_string(),
            enabled: true,
            status: status.to_string(),
            reason: Some("provider_final_outcome".to_string()),
            detail: json!({ "attempts_count": attempts_count }),
        }],
        decision_id: None,
        final_provider: None,
        final_model: None,
        attempts_count: None,
    }
    .with_provider_outcome(decision_id, final_provider, final_model, attempts_count)
}

/// Append a control-ladder trace describing the final provider execution outcome
/// for a routing decision (d04 §10.2). A `decision_id` join links this trace to
/// the `router.route_decision` / `provider.final_outcome` timeline events.
/// Best-effort: serialization/IO failures are logged, never panicked (the chat
/// path must not fail on telemetry).
pub fn append_provider_outcome_trace(
    workspace_dir: &Path,
    decision_id: &str,
    final_provider: &str,
    final_model: &str,
    attempts_count: u8,
    status: &str,
) {
    let trace = build_provider_outcome_trace(decision_id, final_provider, final_model, attempts_count, status);
    if let Err(e) = append_control_ladder_trace(workspace_dir, &trace) {
        tracing::warn!(error = %e, "failed to append provider outcome control ladder trace");
    }
}

fn router_layer(config: &Config) -> ControlLayerTrace {
    let feature_enabled = cfg!(feature = "llm-router");
    let requested = config.router.enabled;
    let enabled = feature_enabled && requested;
    ControlLayerTrace {
        level: 1,
        name: "router".to_string(),
        enabled,
        status: if enabled { "configured" } else { "fallback" }.to_string(),
        reason: if enabled {
            Some("router_enabled".to_string())
        } else if !feature_enabled {
            Some("feature_disabled".to_string())
        } else {
            Some("config_disabled".to_string())
        },
        detail: json!({
            "config_enabled": requested,
            "feature_enabled": feature_enabled,
            "model_routes": config.model_routes.len(),
            "candidate_models": config.router.models.len(),
            "knn_enabled": config.router.knn_enabled,
            "automix_enabled": config.router.automix.enabled,
        }),
    }
}

fn causal_tree_layer(config: &Config) -> ControlLayerTrace {
    let feature_enabled = cfg!(feature = "llm-router");
    let requested = config.causal_tree.enabled;
    let enabled = feature_enabled && requested;
    // Honest semantics: this trace is built from `&Config` only — it has no access
    // to the runtime `AppContext`, and `loop_::run` never calls `build_trace`. So
    // it can only declare the *config intent*, not prove that CTE is actually
    // attached and running. The previous `status = "configured"` over-claimed a
    // runtime attachment that this layer cannot observe. We therefore report
    // `config_declared` and point at the real runtime evidence: the observer's
    // `CteRun` events emitted by `CausalTreeEngine::run` (only fired when the
    // AgentLoop profile attached CTE and a turn actually ran the pipeline). The two
    // are complementary — config declaration here, runtime proof in the event
    // stream — neither pretends to be the other.
    ControlLayerTrace {
        level: 2,
        name: "causal_tree".to_string(),
        enabled,
        status: if enabled { "config_declared" } else { "fallback" }.to_string(),
        reason: if enabled {
            Some("causal_tree_enabled".to_string())
        } else if !feature_enabled {
            Some("feature_disabled".to_string())
        } else {
            Some("config_disabled".to_string())
        },
        detail: json!({
            "config_enabled": requested,
            "feature_enabled": feature_enabled,
            // Make the evidence boundary explicit: this layer is a config snapshot,
            // not a runtime-attached proof. Runtime attachment/run is evidenced by
            // observer `CteRun` events, not by this trace.
            "attachment": "config_snapshot",
            "runtime_evidence": "observer:CteRun",
            "experimental": true,
            "max_branches": config.causal_tree.policy.max_branches,
            "default_side_effect_mode": config.causal_tree.policy.default_side_effect_mode,
        }),
    }
}

fn xin_layer(config: &Config) -> ControlLayerTrace {
    let enabled = config.modules.scheduler && config.xin.enabled;
    ControlLayerTrace {
        level: 3,
        name: "xin".to_string(),
        enabled,
        status: if enabled { "configured" } else { "fallback" }.to_string(),
        reason: if enabled {
            Some("xin_enabled".to_string())
        } else if !config.modules.scheduler {
            Some("scheduler_module_disabled".to_string())
        } else {
            Some("config_disabled".to_string())
        },
        detail: json!({
            "scheduler_module": config.modules.scheduler,
            "xin_enabled": config.xin.enabled,
            "builtin_tasks": config.xin.builtin_tasks,
            "evolution_integration": config.xin.evolution_integration,
            "max_concurrent": config.xin.max_concurrent,
        }),
    }
}

fn self_system_layer(config: &Config) -> ControlLayerTrace {
    let fitness_enabled = config.self_system.enabled;
    let evolution_enabled = config.modules.scheduler && config.self_system.evolution_enabled;
    let enabled = fitness_enabled || evolution_enabled;
    ControlLayerTrace {
        level: 4,
        name: "self_system".to_string(),
        enabled,
        status: if enabled { "configured" } else { "fallback" }.to_string(),
        reason: if enabled {
            Some("self_system_enabled".to_string())
        } else if config.self_system.evolution_enabled && !config.modules.scheduler {
            Some("scheduler_module_disabled".to_string())
        } else {
            Some("config_disabled".to_string())
        },
        detail: json!({
            "fitness_enabled": fitness_enabled,
            "evolution_enabled": config.self_system.evolution_enabled,
            "scheduler_module": config.modules.scheduler,
            "evolution_runtime_enabled": evolution_enabled,
        }),
    }
}

fn task_pool_layer(config: &Config) -> ControlLayerTrace {
    let enabled = config.sessions_spawn.max_concurrent > 0
        && config.sessions_spawn.max_spawn_depth > 0
        && config.sessions_spawn.max_children_per_agent > 0;
    ControlLayerTrace {
        level: 5,
        name: "task_pool".to_string(),
        enabled,
        status: if enabled { "configured" } else { "fallback" }.to_string(),
        reason: if enabled {
            Some("sessions_spawn_enabled".to_string())
        } else {
            Some("capacity_disabled".to_string())
        },
        detail: json!({
            "default_mode": config.sessions_spawn.default_mode,
            "memory_strategy": config.sessions_spawn.process_memory_strategy,
            "max_concurrent": config.sessions_spawn.max_concurrent,
            "max_spawn_depth": config.sessions_spawn.max_spawn_depth,
            "max_children_per_agent": config.sessions_spawn.max_children_per_agent,
        }),
    }
}

fn merge_detail(existing: &Value, update: Value) -> Value {
    match (existing, update) {
        (Value::Object(existing), Value::Object(update)) => {
            let mut merged = existing.clone();
            for (key, value) in update {
                merged.insert(key, value);
            }
            Value::Object(merged)
        }
        (_, update) => update,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn layer<'a>(trace: &'a ControlLadderTrace, name: &str) -> &'a ControlLayerTrace {
        trace.layers.iter().find(|layer| layer.name == name).unwrap()
    }

    #[test]
    fn snapshot_records_independent_disabled_layers_with_fallback_reasons() {
        let mut config = Config::default();
        config.modules.scheduler = false;
        config.router.enabled = false;
        config.causal_tree.enabled = false;
        config.xin.enabled = true;
        config.self_system.evolution_enabled = true;

        let trace = ControlLadderSnapshot::from_config(&config).build_trace("test", Some("run-1".to_string()));

        assert_eq!(layer(&trace, "runtime").status, "active");
        assert_eq!(layer(&trace, "router").status, "fallback");
        assert_eq!(layer(&trace, "causal_tree").status, "fallback");
        assert_eq!(
            layer(&trace, "xin").reason.as_deref(),
            Some("scheduler_module_disabled")
        );
        assert_eq!(
            layer(&trace, "self_system").reason.as_deref(),
            Some("scheduler_module_disabled")
        );
        assert_eq!(layer(&trace, "task_pool").status, "configured");
    }

    /// When CTE is enabled (and the feature is compiled in), the layer reports the
    /// honest `config_declared` status plus an explicit evidence boundary
    /// (`attachment = config_snapshot`, `runtime_evidence = observer:CteRun`) — it
    /// must not over-claim a runtime attachment it cannot observe.
    #[cfg(feature = "llm-router")]
    #[test]
    fn causal_tree_layer_declares_config_snapshot_when_enabled() {
        let mut config = Config::default();
        config.causal_tree.enabled = true;

        let trace = ControlLadderSnapshot::from_config(&config).build_trace("test", None);
        let cte = layer(&trace, "causal_tree");

        assert_eq!(
            cte.status, "config_declared",
            "must declare config intent, not 'configured'"
        );
        assert!(cte.enabled);
        assert_eq!(
            cte.detail.get("attachment").and_then(Value::as_str),
            Some("config_snapshot"),
            "attachment evidence boundary must be explicit"
        );
        assert_eq!(
            cte.detail.get("runtime_evidence").and_then(Value::as_str),
            Some("observer:CteRun"),
            "runtime proof is the observer CteRun event stream"
        );
        assert_eq!(cte.detail.get("experimental").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn trace_layer_updates_preserve_static_detail_and_record_runtime_fallback() {
        let config = Config::default();
        let mut trace = ControlLadderSnapshot::from_config(&config).build_trace("test", None);

        trace.mark_fallback("router", "router_no_candidate", json!({"message_id": "m1"}));

        let router = layer(&trace, "router");
        assert_eq!(router.status, "fallback");
        assert_eq!(router.reason.as_deref(), Some("router_no_candidate"));
        assert_eq!(router.detail.get("message_id").and_then(Value::as_str), Some("m1"));
        assert!(router.detail.get("model_routes").is_some());
    }

    #[test]
    fn append_trace_writes_jsonl_under_workspace_runtime_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let trace = ControlLadderSnapshot::l0_only().build_trace("test", Some("run-1".to_string()));

        let path = append_control_ladder_trace(tmp.path(), &trace).unwrap();

        // Rotation: the written file carries today's UTC date, lives in runtime/,
        // and matches the per-day path computed for the same instant.
        assert_eq!(path, dated_trace_path(tmp.path(), Utc::now()));
        assert_eq!(path.parent().unwrap(), tmp.path().join("runtime"));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("control_ladder_traces."));
        assert!(name.ends_with(".jsonl"));
        assert!(parse_trace_file_date(name).is_some());

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: ControlLadderTrace = serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(parsed.source, "test");
        assert_eq!(parsed.run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn dated_trace_path_uses_date_in_filename() {
        let tmp = tempfile::TempDir::new().unwrap();
        let date = DateTime::parse_from_rfc3339("2026-05-31T12:34:56Z")
            .expect("test: valid rfc3339")
            .with_timezone(&Utc);
        let path = dated_trace_path(tmp.path(), date);
        assert_eq!(
            path,
            tmp.path()
                .join("runtime")
                .join("control_ladder_traces.2026-05-31.jsonl")
        );
        assert_eq!(
            parse_trace_file_date("control_ladder_traces.2026-05-31.jsonl"),
            chrono::NaiveDate::from_ymd_opt(2026, 5, 31)
        );
        // Non-matching names (legacy file, lock sidecar) parse to None.
        assert!(parse_trace_file_date("control_ladder_traces.jsonl").is_none());
        assert!(parse_trace_file_date("control_ladder_traces.2026-05-31.jsonl.lock").is_none());
        assert!(parse_trace_file_date("control_ladder_traces.not-a-date.jsonl").is_none());
    }

    #[test]
    fn cleanup_removes_traces_older_than_retention_window() {
        let tmp = tempfile::TempDir::new().unwrap();
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();

        let now = DateTime::parse_from_rfc3339("2026-05-31T00:00:00Z")
            .expect("test: valid rfc3339")
            .with_timezone(&Utc);

        // 40 days old -> removed; 5 days old -> kept; today -> kept.
        let old = (now - chrono::Duration::days(40)).format("%Y-%m-%d").to_string();
        let recent = (now - chrono::Duration::days(5)).format("%Y-%m-%d").to_string();
        let today = now.format("%Y-%m-%d").to_string();
        // Exactly at the 30-day boundary -> kept (cutoff is strict `<`).
        let boundary = (now - chrono::Duration::days(30)).format("%Y-%m-%d").to_string();

        for d in [&old, &recent, &today, &boundary] {
            let p = runtime.join(format!("control_ladder_traces.{d}.jsonl"));
            std::fs::write(&p, b"{}\n").unwrap();
        }
        // Unrelated files must survive cleanup.
        std::fs::write(runtime.join("control_ladder_traces.jsonl"), b"legacy\n").unwrap();
        std::fs::write(runtime.join("unrelated.txt"), b"x\n").unwrap();

        let removed = cleanup_old_control_ladder_traces(tmp.path(), now).unwrap();
        assert_eq!(removed, 1, "only the 40-day-old trace should be removed");

        assert!(!runtime.join(format!("control_ladder_traces.{old}.jsonl")).exists());
        assert!(runtime.join(format!("control_ladder_traces.{recent}.jsonl")).exists());
        assert!(runtime.join(format!("control_ladder_traces.{today}.jsonl")).exists());
        assert!(runtime.join(format!("control_ladder_traces.{boundary}.jsonl")).exists());
        assert!(runtime.join("control_ladder_traces.jsonl").exists());
        assert!(runtime.join("unrelated.txt").exists());
    }

    #[test]
    fn cleanup_on_missing_dir_is_noop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let removed = cleanup_old_control_ladder_traces(tmp.path(), Utc::now()).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn concurrent_appends_do_not_tear_lines() {
        use std::sync::Arc;
        let tmp = Arc::new(tempfile::TempDir::new().unwrap());
        let workspace = tmp.path().to_path_buf();

        // Build a trace whose serialized line is comfortably larger than PIPE_BUF
        // (4096 on Linux) so a missing lock would tear interleaved writes.
        let big = "x".repeat(8192);
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let workspace = workspace.clone();
                let big = big.clone();
                std::thread::spawn(move || {
                    let mut trace = ControlLadderSnapshot::l0_only().build_trace("test", Some(format!("run-{i}")));
                    trace.mark_active("runtime", "concurrency", json!({ "pad": big }));
                    for _ in 0..20 {
                        append_control_ladder_trace(&workspace, &trace).expect("test: append succeeds");
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("test: thread joins");
        }

        let path = dated_trace_path(&workspace, Utc::now());
        let content = std::fs::read_to_string(&path).expect("test: trace file readable");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 8 * 20, "every append produced exactly one line");
        for line in lines {
            // No torn line: each line must be valid, complete JSON.
            let parsed: ControlLadderTrace = serde_json::from_str(line).expect("test: each line is intact JSON");
            assert_eq!(parsed.source, "test");
        }
    }

    // d04 §10 G7: the provider-outcome trace must actually CARRY the structured
    // decision_id / final_provider / final_model / attempts_count fields (not
    // merely define them), and they must survive JSON serialization so a
    // `decision_id` join links the routing decision to the served provider.
    #[test]
    fn provider_outcome_trace_populates_correlation_fields() {
        let trace = build_provider_outcome_trace("dec-12345", "anthropic", "claude-sonnet-4", 3, "fallback_success");
        assert_eq!(trace.decision_id.as_deref(), Some("dec-12345"));
        assert_eq!(trace.final_provider.as_deref(), Some("anthropic"));
        assert_eq!(trace.final_model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(trace.attempts_count, Some(3));

        let json = serde_json::to_string(&trace).expect("test: trace serializes");
        assert!(json.contains("\"decision_id\":\"dec-12345\""));
        assert!(json.contains("\"final_provider\":\"anthropic\""));
        assert!(json.contains("\"final_model\":\"claude-sonnet-4\""));
        assert!(json.contains("\"attempts_count\":3"));
        let parsed: ControlLadderTrace = serde_json::from_str(&json).expect("test: trace round-trips");
        assert_eq!(parsed.decision_id.as_deref(), Some("dec-12345"));
        assert_eq!(parsed.final_model.as_deref(), Some("claude-sonnet-4"));

        // with_provider_outcome must also work as a standalone builder on an
        // existing trace.
        let enriched = ControlLadderSnapshot::l0_only()
            .build_trace("provider", None)
            .with_provider_outcome("dec-x", "openai", "gpt-4o", 1);
        assert_eq!(enriched.decision_id.as_deref(), Some("dec-x"));
        assert_eq!(enriched.attempts_count, Some(1));
    }

    // FIX-P1-13: the agent.turn path stamps a RouteDecision id onto a trace it is
    // mutating incrementally via the &mut setter (it cannot consume + rebuild the
    // trace). The setter must populate decision_id so the trace joins the routing
    // timeline, and the id must survive JSON serialization.
    #[test]
    fn set_decision_id_stamps_correlation_id_in_place() {
        let mut trace = ControlLadderSnapshot::l0_only().build_trace("agent.turn", Some("turn-1".to_string()));
        assert_eq!(trace.decision_id, None, "fresh trace has no decision id");

        trace.set_decision_id("dec-agent-42");
        assert_eq!(trace.decision_id.as_deref(), Some("dec-agent-42"));
        // run_id (the per-turn id, #26) and decision_id (#25) coexist on the trace.
        assert_eq!(trace.run_id.as_deref(), Some("turn-1"));

        let json = serde_json::to_string(&trace).expect("test: trace serializes");
        assert!(json.contains("\"decision_id\":\"dec-agent-42\""));
        assert!(json.contains("\"run_id\":\"turn-1\""));
        let parsed: ControlLadderTrace = serde_json::from_str(&json).expect("test: trace round-trips");
        assert_eq!(parsed.decision_id.as_deref(), Some("dec-agent-42"));
        assert_eq!(parsed.run_id.as_deref(), Some("turn-1"));
    }
}
