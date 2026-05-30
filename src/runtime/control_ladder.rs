use crate::config::Config;
use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const CONTROL_LADDER_TRACE_PATH: &str = "runtime/control_ladder_traces.jsonl";

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

pub fn append_control_ladder_trace(workspace_dir: &Path, trace: &ControlLadderTrace) -> Result<PathBuf> {
    let path = workspace_dir.join(CONTROL_LADDER_TRACE_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create control ladder trace directory {}", parent.display()))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open control ladder trace {}", path.display()))?;
    let line = serde_json::to_string(trace)?;
    writeln!(file, "{line}").with_context(|| format!("failed to append control ladder trace {}", path.display()))?;
    Ok(path)
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
    ControlLayerTrace {
        level: 2,
        name: "causal_tree".to_string(),
        enabled,
        status: if enabled { "configured" } else { "fallback" }.to_string(),
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

        assert_eq!(path, tmp.path().join(CONTROL_LADDER_TRACE_PATH));
        let content = std::fs::read_to_string(path).unwrap();
        let parsed: ControlLadderTrace = serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(parsed.source, "test");
        assert_eq!(parsed.run_id.as_deref(), Some("run-1"));
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
}
