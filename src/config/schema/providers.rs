use super::{Deserialize, JsonSchema, Serialize};

// ── Delegate Agents ──────────────────────────────────────────────

/// Configuration for a delegate sub-agent used by the `delegate` tool.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct DelegateAgentConfig {
    /// Provider name (e.g. "ollama", "openrouter", "anthropic")
    pub provider: String,
    /// Model name
    pub model: String,
    /// Optional system prompt for the sub-agent
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Optional API key override
    #[serde(default)]
    pub api_key: Option<String>,
    /// A credential reference (`cred-<name>`, `prx providers credential
    /// set`) issued for this agent's provider, used instead of an inline
    /// `api_key`. The reference is resolved from the encrypted auth-profile
    /// store only when its owner is this provider; setting both is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    /// Temperature override
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Max recursion depth for nested delegation
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    /// Enable agentic sub-agent mode (multi-turn tool-call loop).
    #[serde(default)]
    pub agentic: bool,
    /// Allowlist of tool names available to the sub-agent in agentic mode.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Optional identity files directory relative to workspace root.
    #[serde(default)]
    pub identity_dir: Option<String>,
    /// Optional memory scope for spawned sessions: "shared" (default) or "isolated".
    #[serde(default)]
    pub memory_scope: Option<String>,
    /// Whether this agent is allowed to be targeted by sessions_spawn (default: true).
    #[serde(default)]
    pub spawn_enabled: Option<bool>,
}

pub(super) const fn default_max_depth() -> u32 {
    // Behavior-limits Phase 1: raised 3 -> 8. Deeper delegation chains are
    // allowed; worst case is slower/costlier, not a crash (each level is a
    // synchronous call, not a fork). Fork-bomb safety lives in `sessions_spawn`.
    8
}

pub(super) const fn default_router_alpha() -> f32 {
    0.0
}

pub(super) const fn default_router_beta() -> f32 {
    0.5
}

pub(super) const fn default_router_gamma() -> f32 {
    0.3
}

pub(super) const fn default_router_delta() -> f32 {
    0.1
}

pub(super) const fn default_router_epsilon() -> f32 {
    0.1
}

pub(super) const fn default_router_knn_min_records() -> usize {
    10
}

pub(super) const fn default_router_knn_k() -> usize {
    7
}

pub(super) const fn default_router_max_context() -> usize {
    1_000_000
}

pub(super) const fn default_router_latency() -> u32 {
    2_000
}

pub(super) const fn default_router_elo() -> f32 {
    1_000.0
}

pub(super) const fn default_automix_confidence_threshold() -> f32 {
    0.7
}

pub(super) const fn default_automix_escalation_window_ms() -> u64 {
    30_000
}

/// Adaptive escalation policy for cheap-first routing.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AutomixConfig {
    /// Escalate when confidence falls below this threshold. Default: `0.7`.
    #[serde(default = "default_automix_confidence_threshold")]
    pub confidence_threshold: f32,
    /// Tier markers that identify cheap-first models.
    #[serde(default)]
    pub cheap_model_tiers: Vec<String>,
    /// Premium model target used for escalation.
    #[serde(default)]
    pub premium_model_id: String,
    /// Premium escalation window after the cheap reply, including settlement.
    /// Default: 30,000 ms; allowed range: 1..=120,000 ms.
    #[serde(default = "default_automix_escalation_window_ms")]
    pub escalation_window_ms: u64,
}

impl Default for AutomixConfig {
    fn default() -> Self {
        Self {
            confidence_threshold: default_automix_confidence_threshold(),
            cheap_model_tiers: Vec::new(),
            premium_model_id: String::new(),
            escalation_window_ms: default_automix_escalation_window_ms(),
        }
    }
}

impl AutomixConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !(0.0..=1.0).contains(&self.confidence_threshold) {
            anyhow::bail!("automix.confidence_threshold must be in [0,1]");
        }
        anyhow::ensure!(
            (1..=120_000).contains(&self.escalation_window_ms),
            "automix.escalation_window_ms must be in [1,120000]"
        );
        Ok(())
    }
}

/// Heuristic LLM router configuration (`[router]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RouterConfig {
    /// Similarity score weight. Phase 1 keeps this at `0.0`.
    #[serde(default = "default_router_alpha")]
    pub alpha: f32,
    /// Capability score weight.
    #[serde(default = "default_router_beta")]
    pub beta: f32,
    /// Elo score weight.
    #[serde(default = "default_router_gamma")]
    pub gamma: f32,
    /// Cost penalty coefficient.
    #[serde(default = "default_router_delta")]
    pub delta: f32,
    /// Latency penalty coefficient.
    #[serde(default = "default_router_epsilon")]
    pub epsilon: f32,
    /// Minimum history records before KNN affects routing. Default: `10`.
    #[serde(default = "default_router_knn_min_records")]
    pub knn_min_records: usize,
    /// Number of nearest neighbors considered for voting. Default: `7`.
    #[serde(default = "default_router_knn_k")]
    pub knn_k: usize,
    /// Cheap-first adaptive escalation policy.
    #[serde(default)]
    pub automix: AutomixConfig,
    /// Candidate model registry.
    #[serde(default)]
    pub models: Vec<RouterModelConfig>,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            alpha: default_router_alpha(),
            beta: default_router_beta(),
            gamma: default_router_gamma(),
            delta: default_router_delta(),
            epsilon: default_router_epsilon(),
            knn_min_records: default_router_knn_min_records(),
            knn_k: default_router_knn_k(),
            automix: AutomixConfig::default(),
            models: Vec::new(),
        }
    }
}

impl RouterConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.automix.validate()?;
        for (name, weight) in [
            ("alpha", self.alpha),
            ("beta", self.beta),
            ("gamma", self.gamma),
            ("delta", self.delta),
            ("epsilon", self.epsilon),
        ] {
            if weight < 0.0 {
                anyhow::bail!("router.{name} must be non-negative");
            }
        }
        Ok(())
    }
}

/// Static router metadata for one model candidate.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct RouterModelConfig {
    /// Model identifier without provider prefix.
    pub model_id: String,
    /// Provider identifier.
    pub provider: String,
    /// USD cost per million tokens.
    #[serde(default)]
    pub cost_per_million_tokens: f32,
    /// Maximum context window in tokens.
    #[serde(default = "default_router_max_context")]
    pub max_context: usize,
    /// Output tokens reserved for this model when planning input context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_output_tokens: Option<usize>,
    /// Average latency in milliseconds.
    #[serde(default = "default_router_latency")]
    pub latency_ms: u32,
    /// Capability categories such as `conversation`, `code`, or `analysis`.
    #[serde(default)]
    pub categories: Vec<String>,
    /// Initial Elo rating.
    #[serde(default = "default_router_elo")]
    pub elo_rating: f32,
}

/// Provider-specific runtime configuration (`[providers]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProvidersConfig {
    /// Ollama-specific request options (`[providers.ollama]`).
    #[serde(default)]
    pub ollama: OllamaProviderConfig,
    /// Operator declarations of which tool-schema dialect and which textual
    /// tool-call grammar an OpenAI-compatible endpoint accepts
    /// (`[[providers.tool_dialects]]`).
    ///
    /// Nothing is inferred from a URL: an endpoint with no entry is offered the
    /// published tool schemas unchanged (`raw`) and only its native
    /// `tool_calls` array is read (`native`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_dialects: Vec<ProviderToolDialectConfig>,
    /// Operator declarations of what a provider's models can do
    /// (`[[providers.models]]`): the curated source of the per-(provider,
    /// model) descriptor the model resolver consults.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ProviderModelConfig>,
}

/// One `[[providers.tool_dialects]]` entry: what one OpenAI-compatible
/// endpoint, or one model behind it, accepts.
///
/// The entry is the `(provider, optional model)` key plus two capability
/// fields of the per-(provider, model) descriptor that will replace it (P1-B).
/// The descriptor now reads this table as operator declarations alongside
/// `[[providers.models]]` (`tool_schema` / `tool_call` there are
/// `tool_schema_dialect` / `tool_call_dialect`); the table stays accepted,
/// and moving its rows into `[[providers.models]]` is a later migration step.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderToolDialectConfig {
    /// The provider, spelled as in `default_provider`, `fallback_providers` or
    /// `--provider`: a `custom:<url>` endpoint, or one of the catalog entries
    /// that address a server the operator runs (`vllm`, `llamacpp`,
    /// `lmstudio`, `litellm`). Every spelling of the same provider identity is
    /// the same entry: `custom:http://h:8080/v1/` and `CUSTOM:http://h:8080/v1`
    /// are one endpoint, a different port or path is another.
    pub provider: String,
    /// The model this entry applies to (compared ignoring ASCII case), or every
    /// model of the provider when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The tool-schema dialect the endpoint accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_schema: Option<ToolSchemaDialectSetting>,
    /// The textual tool-call grammar the endpoint emits. Provider-wide entries
    /// only: the response parser has no model, so a model-scoped grammar would
    /// silently apply to every model of the endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<ToolCallDialectSetting>,
}

/// One `[[providers.models]]` entry: an operator declaration of what one
/// model of a provider — or every model of it — can do.
///
/// It is the curated source of the per-(provider, model) descriptor the
/// resolver consults. Every field but `provider` is optional and declares
/// nothing when omitted. A declaration settles what discovery left unknown;
/// it never lifts what the provider's adapter cannot execute, and negative
/// catalog evidence still wins over it.
///
/// `tool_schema_dialect` and `tool_call_dialect` are the same declarations as
/// `[[providers.tool_dialects]]` `tool_schema` / `tool_call`, which stays
/// accepted; one `(provider, model)` may declare a dialect in only one of the
/// two tables.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderModelConfig {
    /// The provider, spelled as in `default_provider` (any alias, or a
    /// `custom:<url>` endpoint). Every spelling of one provider identity is the
    /// same provider.
    pub provider: String,
    /// The model (compared ignoring ASCII case), or every model of the
    /// provider when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The complete list of operations the model executes; every operation not
    /// listed is unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operations: Option<Vec<ModelOperationSetting>>,
    /// Whether the model accepts native tool calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_tools: Option<bool>,
    /// Whether the model accepts image input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_input: Option<bool>,
    /// Whether the model streams responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    /// Whether the model honours a JSON-schema response format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<bool>,
    /// The tool-schema dialect the endpoint accepts for this model. Only for a
    /// provider that reads dialect declarations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_schema_dialect: Option<ToolSchemaDialectSetting>,
    /// The textual tool-call grammar the endpoint emits. Entries without
    /// `model` only: the response parser cannot tell models apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_dialect: Option<ToolCallDialectSetting>,
    /// The model's context window in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

impl ProviderModelConfig {
    /// Whether the entry declares anything at all.
    #[must_use]
    pub const fn declares_anything(&self) -> bool {
        self.operations.is_some()
            || self.native_tools.is_some()
            || self.vision_input.is_some()
            || self.streaming.is_some()
            || self.structured_output.is_some()
            || self.tool_schema_dialect.is_some()
            || self.tool_call_dialect.is_some()
            || self.context_window.is_some()
    }
}

/// The operations a `[[providers.models]]` entry may list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelOperationSetting {
    Chat,
    Embedding,
    DecisionEndpoint,
    Rerank,
    ImageGeneration,
    AudioGeneration,
    VideoGeneration,
}

/// The tool-schema dialects an operator may declare for an endpoint.
///
/// `gemini` and `anthropic` are deliberately absent: they name the fixed
/// rewrite of a provider family, which the constructed provider already
/// reports, and declaring one for a single model of an endpoint would make
/// argument validation looser than the document that model was shown.
/// Adding a stripping dialect here would make the model-less
/// `OpenAiCompatibleProvider::tool_schema_dialect` (still read by callers
/// that do not know the model) answer `Raw` for endpoints that mix it with
/// undeclared models; callers that know the model are exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolSchemaDialectSetting {
    /// The published document, byte for byte (the default).
    Raw,
    /// The endpoint rejects `allOf`/`anyOf`/`oneOf` at the root of a tool
    /// schema: a root `allOf` of plain object fragments is merged losslessly,
    /// and a request carrying any other root combinator fails before transport.
    NoRootCombinators,
}

/// The tool-call grammars an operator may declare for an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallDialectSetting {
    /// Only the native `tool_calls` array is a tool call (the default).
    Native,
    /// Additionally read `<function=NAME>{json}</function>` from the content,
    /// for a tool name the request registered.
    XmlFunctionTags,
}

/// Ollama-specific request options (`[providers.ollama]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct OllamaProviderConfig {
    /// Explicit context window sent as Ollama `options.num_ctx`.
    #[serde(default)]
    pub num_ctx: Option<usize>,
}
