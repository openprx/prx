use super::{Deserialize, JsonSchema, Serialize};

// ── Model routing ────────────────────────────────────────────────

/// Route a task hint to a specific provider + model.
///
/// ```toml
/// [[model_routes]]
/// hint = "reasoning"
/// provider = "openrouter"
/// model = "anthropic/claude-opus-4-20250514"
///
/// [[model_routes]]
/// hint = "fast"
/// provider = "groq"
/// model = "llama-3.3-70b-versatile"
/// ```
///
/// Usage: pass `hint:reasoning` as the model parameter to route the request.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct ModelRouteConfig {
    /// Route-selected level; explicit route parameters override explicit model parameters.
    #[serde(default, flatten)]
    pub settings: crate::config::model_settings::ModelSettings,
    /// Task hint name (e.g. "reasoning", "fast", "code", "summarize")
    pub hint: String,
    /// Provider to route to (must match a known provider name)
    pub provider: String,
    /// Model to use with that provider
    pub model: String,
    /// Optional API key override for this route's provider
    #[serde(default)]
    pub api_key: Option<String>,
    /// A credential reference (`cred-<name>`, `prx providers credential
    /// set`) issued for this route's provider, used instead of an inline
    /// `api_key`. The reference is resolved from the encrypted auth-profile
    /// store only when its owner is this provider; setting both is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
}

// ── Embedding routing ───────────────────────────────────────────

/// Route an embedding hint to a specific provider + model.
///
/// ```toml
/// [[embedding_routes]]
/// hint = "semantic"
/// provider = "openai"
/// model = "text-embedding-3-small"
/// dimensions = 1536
///
/// [memory]
/// embedding_model = "hint:semantic"
/// ```
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct EmbeddingRouteConfig {
    /// Route hint name (e.g. "semantic", "archive", "faq")
    pub hint: String,
    /// Embedding provider (`none`, `openai`, or `custom:<url>`)
    pub provider: String,
    /// Embedding model to use with that provider
    pub model: String,
    /// Optional embedding dimension override for this route
    #[serde(default)]
    pub dimensions: Option<usize>,
    /// Optional API key override for this route's provider
    #[serde(default)]
    pub api_key: Option<String>,
    /// A credential reference (`cred-<name>`, `prx providers credential
    /// set`) issued for this embedding route's provider, used instead of an inline
    /// `api_key`. The reference is resolved from the encrypted auth-profile
    /// store only when its owner is this provider; setting both is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
}

// ── Query Classification ─────────────────────────────────────────

/// Automatic query classification — classifies user messages by keyword/pattern
/// and routes to the appropriate model hint. Disabled by default.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct QueryClassificationConfig {
    /// Classification rules evaluated in priority order.
    #[serde(default)]
    pub rules: Vec<ClassificationRule>,
}

/// A single classification rule mapping message patterns to a model hint.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct ClassificationRule {
    /// Must match a `[[model_routes]]` hint value.
    pub hint: String,
    /// Case-insensitive substring matches.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Case-sensitive literal matches (for "```", "fn ", etc.).
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Only match if message length >= N chars.
    #[serde(default)]
    pub min_length: Option<usize>,
    /// Only match if message length <= N chars.
    #[serde(default)]
    pub max_length: Option<usize>,
    /// Higher priority rules are checked first.
    #[serde(default)]
    pub priority: i32,
}

/// Task-routing default and rule intent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TaskRoutingIntentConfig {
    #[default]
    Simple,
    Delegate,
    Stream,
}

/// Lightweight pre-LLM task routing.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct TaskRoutingConfig {
    /// Fallback intent when no rule matches.
    #[serde(default)]
    pub default_intent: TaskRoutingIntentConfig,
    /// Rules evaluated in descending priority order.
    #[serde(default)]
    pub rules: Vec<TaskRoutingRule>,
}

/// A single task-routing rule.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct TaskRoutingRule {
    /// Case-insensitive substring matches.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Route intent to apply when a keyword matches.
    #[serde(default)]
    pub intent: TaskRoutingIntentConfig,
    /// Optional model hint/raw model for non-delegated requests.
    #[serde(default)]
    pub model_hint: Option<String>,
    /// Optional raw sub-agent model override for delegated tasks.
    #[serde(default)]
    pub sub_agent_model: Option<String>,
    /// Higher priority rules are checked first.
    #[serde(default)]
    pub priority: i32,
}
