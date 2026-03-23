//! Tool Policy Pipeline — P3-1
//!
//! Multi-layer policy evaluation for tool calls. Layers are evaluated in order from
//! broadest (Global) to most specific (Tool). The most specific matching policy wins.
//! Each decision is fully explainable via `PolicyDecision`.

use crate::config::schema::ToolPolicyConfig;
use std::collections::HashMap;

// ── Policy layer hierarchy ────────────────────────────────────────────────────

/// Hierarchy of policy layers, from broadest to most specific.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyLayer {
    /// Global default (e.g. `default = "allow"`)
    Global,
    /// User profile / channel-level overrides
    Profile,
    /// Agent-level restrictions
    Agent,
    /// Tool group (e.g. `group:sessions`, `group:automation`)
    Group,
    /// Per-tool override (most specific)
    Tool,
}

impl std::fmt::Display for PolicyLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyLayer::Global => write!(f, "global"),
            PolicyLayer::Profile => write!(f, "profile"),
            PolicyLayer::Agent => write!(f, "agent"),
            PolicyLayer::Group => write!(f, "group"),
            PolicyLayer::Tool => write!(f, "tool"),
        }
    }
}

// ── Decision ──────────────────────────────────────────────────────────────────

/// The result of a policy evaluation: whether the tool is allowed, why, and which layers contributed.
#[derive(Debug, Clone)]
pub struct PolicyDecision {
    /// Whether the tool call is allowed.
    pub allowed: bool,
    /// Human-readable explanation of the decision.
    pub reason: String,
    /// The layers that were consulted during evaluation.
    pub layers_applied: Vec<PolicyLayer>,
}

impl PolicyDecision {
    fn allow(reason: impl Into<String>, layers: Vec<PolicyLayer>) -> Self {
        Self {
            allowed: true,
            reason: reason.into(),
            layers_applied: layers,
        }
    }

    fn deny(reason: impl Into<String>, layers: Vec<PolicyLayer>) -> Self {
        Self {
            allowed: false,
            reason: reason.into(),
            layers_applied: layers,
        }
    }
}

// ── Tool group membership ─────────────────────────────────────────────────────

/// Well-known tool groups.
///
/// Map a tool name to the group(s) it belongs to. A tool may belong to at most one group
/// for policy purposes.
static TOOL_GROUPS: std::sync::LazyLock<HashMap<&'static str, &'static str>> = std::sync::LazyLock::new(|| {
    let mut m = HashMap::new();
    // group:sessions
    for tool in ["sessions_list", "sessions_send", "sessions_history", "sessions_spawn"] {
        m.insert(tool, "sessions");
    }
    // group:automation
    for tool in ["cron", "schedule", "gateway"] {
        m.insert(tool, "automation");
    }
    // group:ui
    for tool in ["canvas", "browser", "browser_open"] {
        m.insert(tool, "ui");
    }
    // group:hardware
    for tool in [
        "nodes",
        "hardware_read",
        "hardware_write",
        "hardware_gpio",
        "hardware_serial",
    ] {
        m.insert(tool, "hardware");
    }
    m
});

/// Look up which group a tool belongs to (if any).
pub fn tool_group(tool_name: &str) -> Option<&'static str> {
    // Handle hardware wildcard: any tool starting with "hardware_"
    if tool_name.starts_with("hardware_") {
        return Some("hardware");
    }
    TOOL_GROUPS.get(tool_name).copied()
}

// ── Pipeline evaluation context ───────────────────────────────────────────────

/// Runtime context passed to `PolicyPipeline::evaluate`.
#[derive(Debug, Clone, Default)]
pub struct EvalContext {
    /// Channel name: "signal", "telegram", "discord", etc.
    pub channel: String,
    /// Chat type: "direct" or "group".
    pub chat_type: String,
    /// Sender identity (UUID or phone number).
    pub sender: String,
}

// ── PolicyPipeline ────────────────────────────────────────────────────────────

/// Multi-layer tool policy evaluator.
///
/// Layers are evaluated from broadest (Global) to most specific (Tool).
/// The *most specific* matching policy wins.
#[derive(Debug, Clone)]
pub struct PolicyPipeline {
    config: ToolPolicyConfig,
}

impl PolicyPipeline {
    /// Create a new pipeline from the `[security.tool_policy]` config section.
    pub fn new(config: ToolPolicyConfig) -> Self {
        Self { config }
    }

    /// Create a pipeline from a full `Config` reference.
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self::new(config.security.tool_policy.clone())
    }

    /// Evaluate all policy layers for `tool_name` and return a `PolicyDecision`.
    ///
    /// Evaluation order (most specific overrides broader):
    /// 1. Global default
    /// 2. Group policy (if the tool belongs to a group)
    /// 3. Per-tool policy
    ///
    /// Profile and Agent layers are reserved for future use (currently no-op).
    pub fn evaluate(&self, tool_name: &str, _ctx: &EvalContext) -> PolicyDecision {
        let mut layers = Vec::new();
        let mut current_allowed = self.config.default_allow();
        let mut reason = format!("global default: {}", if current_allowed { "allow" } else { "deny" });
        layers.push(PolicyLayer::Global);

        // Profile layer (future: check per-user/channel overrides)
        layers.push(PolicyLayer::Profile);

        // Agent layer (future: check per-agent restrictions)
        layers.push(PolicyLayer::Agent);

        // Group layer
        if let Some(group_name) = tool_group(tool_name) {
            layers.push(PolicyLayer::Group);
            if let Some(group_policy) = self.config.groups.get(group_name) {
                let allowed = policy_str_to_bool(group_policy);
                current_allowed = allowed;
                reason = format!("group:{group_name} policy: {}", group_policy);
            }
        }

        // Tool layer (most specific)
        if let Some(tool_policy) = self.config.tools.get(tool_name) {
            layers.push(PolicyLayer::Tool);
            let allowed = policy_str_to_bool(tool_policy);
            current_allowed = allowed;
            reason = format!("tool-specific policy for '{tool_name}': {}", tool_policy);
        }

        if current_allowed {
            PolicyDecision::allow(reason, layers)
        } else {
            PolicyDecision::deny(reason, layers)
        }
    }
}

/// Convert a policy string like "allow", "deny", "supervised" into a boolean.
///
/// "allow" and "supervised" both permit execution (supervised signals that extra care
/// is advised, but the tool is not blocked at the pipeline level).
/// "deny" blocks the tool.
/// Unrecognized values are treated as "deny" and logged, to prevent typos from
/// silently allowing dangerous tools.
fn policy_str_to_bool(policy: &str) -> bool {
    match policy.trim().to_ascii_lowercase().as_str() {
        "allow" | "supervised" | "yes" | "true" => true,
        "deny" | "no" | "false" => false,
        other => {
            tracing::warn!(
                policy = other,
                "unrecognized policy value (treated as deny); expected allow/deny/supervised"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::ToolPolicyConfig;
    use std::collections::HashMap;

    fn make_pipeline(default: &str, groups: HashMap<String, String>, tools: HashMap<String, String>) -> PolicyPipeline {
        PolicyPipeline::new(ToolPolicyConfig {
            default: default.to_string(),
            groups,
            tools,
        })
    }

    #[test]
    fn global_allow_permits_any_tool() {
        let p = make_pipeline("allow", Default::default(), Default::default());
        let d = p.evaluate("shell", &Default::default());
        assert!(d.allowed);
        assert!(d.layers_applied.contains(&PolicyLayer::Global));
    }

    #[test]
    fn global_deny_blocks_any_tool() {
        let p = make_pipeline("deny", Default::default(), Default::default());
        let d = p.evaluate("shell", &Default::default());
        assert!(!d.allowed);
    }

    #[test]
    fn group_policy_overrides_global() {
        let mut groups = HashMap::new();
        groups.insert("sessions".to_string(), "deny".to_string());
        let p = make_pipeline("allow", groups, Default::default());
        let d = p.evaluate("sessions_list", &Default::default());
        assert!(!d.allowed);
        assert!(d.layers_applied.contains(&PolicyLayer::Group));
    }

    #[test]
    fn tool_policy_overrides_group() {
        let mut groups = HashMap::new();
        groups.insert("sessions".to_string(), "deny".to_string());
        let mut tools = HashMap::new();
        tools.insert("sessions_spawn".to_string(), "allow".to_string());
        let p = make_pipeline("allow", groups, tools);
        // sessions_spawn: group=deny, tool=allow → allow
        let d = p.evaluate("sessions_spawn", &Default::default());
        assert!(d.allowed);
        assert!(d.layers_applied.contains(&PolicyLayer::Tool));
        // sessions_list: group=deny → deny
        let d2 = p.evaluate("sessions_list", &Default::default());
        assert!(!d2.allowed);
    }

    #[test]
    fn hardware_wildcard_group() {
        let mut groups = HashMap::new();
        groups.insert("hardware".to_string(), "deny".to_string());
        let p = make_pipeline("allow", groups, Default::default());
        let d = p.evaluate("hardware_gpio", &Default::default());
        assert!(!d.allowed);
        assert!(d.layers_applied.contains(&PolicyLayer::Group));
    }

    #[test]
    fn supervised_policy_is_treated_as_allow() {
        let mut tools = HashMap::new();
        tools.insert("shell".to_string(), "supervised".to_string());
        let p = make_pipeline("deny", Default::default(), tools);
        let d = p.evaluate("shell", &Default::default());
        assert!(d.allowed);
    }

    #[test]
    fn tool_group_known_tools() {
        assert_eq!(tool_group("sessions_list"), Some("sessions"));
        assert_eq!(tool_group("canvas"), Some("ui"));
        assert_eq!(tool_group("cron"), Some("automation"));
        assert_eq!(tool_group("nodes"), Some("hardware"));
        assert_eq!(tool_group("hardware_serial"), Some("hardware"));
        assert_eq!(tool_group("unknown_tool"), None);
    }
}
