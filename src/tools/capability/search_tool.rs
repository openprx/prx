//! Read-only discovery under an authority issued for one live decision.
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::runtime::turn_decision::CapabilitySearchAuthority;
use crate::tools::{Tool, ToolResult};

pub(crate) const MAX_SEARCH_HITS: usize = 5;
pub(crate) const MAX_QUERY_BYTES: usize = 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

const fn default_limit() -> usize {
    MAX_SEARCH_HITS
}

/// Runtime-only invocation authority; never a field in model arguments.
#[derive(Clone)]
pub(crate) struct CapabilitySearchPort(Arc<CapabilitySearchAuthority>);

impl CapabilitySearchPort {
    pub(crate) const fn new(authority: Arc<CapabilitySearchAuthority>) -> Self {
        Self(authority)
    }
}

impl std::fmt::Debug for CapabilitySearchPort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CapabilitySearchPort")
    }
}

impl PartialEq for CapabilitySearchPort {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for CapabilitySearchPort {}

tokio::task_local! {
    static SEARCH_AUTHORITY: Option<CapabilitySearchPort>;
}

/// Installed around the actual backend invocation, after execution admission.
/// An explicit None also masks any surrounding call's authority.
pub(crate) async fn with_search_authority<F: std::future::Future>(
    authority: Option<CapabilitySearchPort>,
    future: F,
) -> F::Output {
    SEARCH_AUTHORITY.scope(authority, future).await
}

/// Registry instances are stateless and cannot retain an earlier turn.
pub(crate) struct CapabilitySearchTool;

#[async_trait]
impl Tool for CapabilitySearchTool {
    fn name(&self) -> &str {
        "capability_search"
    }

    fn description(&self) -> &str {
        "Find capabilities available to this turn. Results include exact schemas and fingerprints; execution still requires admission."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 1, "maxLength": MAX_QUERY_BYTES,
                    "description": "Search text, at most 1024 UTF-8 bytes."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_SEARCH_HITS}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, mut args: Value) -> anyhow::Result<ToolResult> {
        // The execution service adds reserved metadata after validating the
        // public schema. This tool takes authority exclusively from its port.
        if let Some(object) = args.as_object_mut() {
            crate::tools::execution::strip_runtime_only_args(object);
        }
        // Do not echo parser errors: they can contain untrusted input.
        let args: SearchArguments =
            serde_json::from_value(args).map_err(|_| anyhow::anyhow!("invalid capability search arguments"))?;
        anyhow::ensure!(
            !args.query.trim().is_empty()
                && args.query.len() <= MAX_QUERY_BYTES
                && (1..=MAX_SEARCH_HITS).contains(&args.limit),
            "capability search query or limit is outside bounds"
        );
        let authority = SEARCH_AUTHORITY
            .try_with(Clone::clone)
            .ok()
            .flatten()
            .ok_or_else(|| anyhow::anyhow!("capability search requires an admitted invocation"))?;
        let hits = authority.0.search(&args.query, args.limit)?;
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string(&json!({"hits": hits}))?,
            error: None,
        })
    }
}
