use super::{MemoryScope, memory_key_prefix};
use crate::tools::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct AllowedToolProxy {
    pub(super) source: Arc<Vec<Box<dyn Tool>>>,
    pub(super) public_name: String,
    pub(super) memory_prefix: Option<String>,
}

impl AllowedToolProxy {
    fn find_source_tool(&self) -> Option<&dyn Tool> {
        self.source
            .iter()
            .find(|tool| tool.supports_name(&self.public_name))
            .map(|tool| tool.as_ref())
    }

    fn source_spec(&self) -> Option<crate::tools::ToolSpec> {
        self.find_source_tool()?.spec_for_name(&self.public_name)
    }

    fn rewrite_args(&self, mut args: serde_json::Value) -> serde_json::Value {
        if self.public_name == "memory_store"
            && let Some(prefix) = &self.memory_prefix
        {
            let new_key = args
                .get("key")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(|key| memory_key_prefix(prefix, key));
            if let Some(new_key) = new_key
                && let Some(map) = args.as_object_mut()
            {
                map.insert("key".to_string(), serde_json::Value::String(new_key));
            }
        }
        args
    }
}

#[async_trait]
impl Tool for AllowedToolProxy {
    fn name(&self) -> &str {
        &self.public_name
    }

    fn description(&self) -> &str {
        self.find_source_tool()
            .map(|tool| tool.description())
            .unwrap_or("Unavailable proxied tool")
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.source_spec().map(|spec| spec.parameters).unwrap_or_else(|| {
            json!({
                "type": "object",
                "description": "Unavailable proxied tool"
            })
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let Some(tool) = self.find_source_tool() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Tool '{}' is not registered.", self.public_name)),
            });
        };

        tool.execute_named(&self.public_name, self.rewrite_args(args)).await
    }

    async fn execute_with_cancellation(
        &self,
        args: serde_json::Value,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        self.execute_named_with_cancellation(&self.public_name, args, cancellation)
            .await
    }

    async fn refresh(&self) -> anyhow::Result<()> {
        if let Some(tool) = self.find_source_tool() {
            tool.refresh().await?;
        }
        Ok(())
    }

    fn supports_name(&self, name: &str) -> bool {
        self.find_source_tool().is_some_and(|tool| {
            if self.public_name == tool.name() {
                tool.supports_name(name)
            } else {
                name == self.public_name
            }
        })
    }

    fn specs(&self) -> Vec<crate::tools::ToolSpec> {
        let Some(tool) = self.find_source_tool() else {
            return vec![self.spec()];
        };
        if self.public_name == tool.name() {
            tool.specs()
        } else {
            self.source_spec().into_iter().collect()
        }
    }

    fn spec_for_name(&self, name: &str) -> Option<crate::tools::ToolSpec> {
        self.supports_name(name)
            .then(|| self.find_source_tool()?.spec_for_name(name))
            .flatten()
    }

    fn catalog_specs(&self) -> Vec<crate::tools::ToolSpec> {
        let Some(tool) = self.find_source_tool() else {
            return Vec::new();
        };
        if self.public_name == tool.name() {
            tool.catalog_specs()
        } else {
            tool.spec_for_name(&self.public_name).into_iter().collect()
        }
    }

    fn immutable_catalog_specs(&self) -> Option<&[crate::tools::ToolSpec]> {
        let tool = self.find_source_tool()?;
        if self.public_name == tool.name() {
            tool.immutable_catalog_specs()
        } else {
            None
        }
    }

    fn shared_catalog_snapshot(&self) -> Option<crate::tools::SharedToolCatalogSnapshot> {
        let tool = self.find_source_tool()?;
        let snapshot = tool.shared_catalog_snapshot()?;
        Some(if self.public_name == tool.name() {
            snapshot
        } else {
            snapshot.restrict_to(&self.public_name, false)
        })
    }

    fn catalog_binding_snapshot(&self) -> std::collections::BTreeMap<String, [u8; 32]> {
        let Some(tool) = self.find_source_tool() else {
            return Default::default();
        };
        let mut bindings = tool.catalog_binding_snapshot();
        if self.public_name != tool.name() {
            bindings.retain(|name, _| name == &self.public_name);
        }
        bindings
    }

    fn catalog_availability_is_configured(&self) -> bool {
        self.find_source_tool()
            .is_some_and(Tool::catalog_availability_is_configured)
    }

    async fn resolve_approval_operation(
        &self,
        args: &serde_json::Value,
    ) -> anyhow::Result<Option<(String, crate::acl::approval_grant::RiskLevel)>> {
        let Some(tool) = self.find_source_tool() else {
            return Ok(None);
        };
        // The root resolver has no alias name parameter; do not apply it to
        // another public operation hosted by the same backend.
        if self.public_name != tool.name() {
            return Ok(None);
        }
        if self.public_name == "memory_store" && self.memory_prefix.is_some() {
            tool.resolve_approval_operation(&self.rewrite_args(args.clone())).await
        } else {
            tool.resolve_approval_operation(args).await
        }
    }

    fn tier(&self) -> crate::tools::ToolTier {
        self.find_source_tool()
            .map_or(crate::tools::ToolTier::Standard, Tool::tier)
    }

    fn categories(&self) -> &'static [crate::tools::ToolCategory] {
        self.find_source_tool().map_or(&[], Tool::categories)
    }

    fn availability(&self) -> crate::capability::CapabilityAvailability {
        self.find_source_tool().map_or_else(
            || crate::capability::CapabilityAvailability::declared("proxied tool is not registered"),
            Tool::availability,
        )
    }

    async fn execute_named(&self, name: &str, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        anyhow::ensure!(self.supports_name(name), "proxied tool does not expose requested name");
        let Some(tool) = self.find_source_tool() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Tool '{}' is not registered.", self.public_name)),
            });
        };
        tool.execute_named(name, self.rewrite_args(args)).await
    }

    async fn execute_named_observed(
        &self,
        name: &str,
        args: serde_json::Value,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<crate::tools::traits::ToolInvocationResult> {
        anyhow::ensure!(self.supports_name(name), "proxied tool does not expose requested name");
        let Some(tool) = self.find_source_tool() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Tool '{}' is not registered.", self.public_name)),
            }
            .into());
        };
        tool.execute_named_observed(name, self.rewrite_args(args), cancellation)
            .await
    }

    async fn execute_named_with_cancellation(
        &self,
        name: &str,
        args: serde_json::Value,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        anyhow::ensure!(self.supports_name(name), "proxied tool does not expose requested name");
        let Some(tool) = self.find_source_tool() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Tool '{}' is not registered.", self.public_name)),
            });
        };
        tool.execute_named_with_cancellation(name, self.rewrite_args(args), cancellation)
            .await
    }
}

/// Build the tool registry one sub-agent sees.
///
/// `channel_excluded` is the spawning channel surface's hidden set and is
/// applied last, after the agent's own allowlist: the boundary between a
/// conversation partner and the operations surface is not something an agent
/// configuration — or a `mode: "process"` manifest — may re-open. Names an
/// operator put back through `[tool_tiering] always_include` never reach this
/// list, because it is derived from `for_channel_surface()`, which keeps them.
pub(crate) fn resolve_tools_for_agent(
    source: Arc<Vec<Box<dyn Tool>>>,
    agent_name: &str,
    memory_scope: MemoryScope,
    allowed_tools: Option<&[String]>,
    channel_excluded: &[String],
) -> Arc<Vec<Box<dyn Tool>>> {
    let allowlist = allowed_tools.map(|items| {
        items
            .iter()
            .map(|item| item.trim())
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>()
    });
    let inherit_all = allowlist
        .as_ref()
        .is_none_or(|items| items.len() == 1 && items.first().is_some_and(|name| *name == "*"));

    let mut selected_names = Vec::new();
    if !inherit_all && let Some(list) = allowlist {
        for name in list {
            if !selected_names.contains(&name) {
                selected_names.push(name);
            }
        }
    } else {
        for tool in source.iter() {
            let name = tool.name();
            if !selected_names.contains(&name) {
                selected_names.push(name);
            }
        }
    }
    if !selected_names.contains(&crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME)
        && source
            .iter()
            .any(|tool| tool.supports_name(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME))
    {
        selected_names.push(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME);
    }
    if !channel_excluded.is_empty() {
        selected_names.retain(|name| !channel_excluded.iter().any(|hidden| hidden == name));
    }

    let memory_prefix = if memory_scope == MemoryScope::Isolated {
        Some(agent_name.to_string())
    } else {
        None
    };

    let resolved = selected_names
        .into_iter()
        .map(|name| {
            Box::new(AllowedToolProxy {
                source: source.clone(),
                public_name: name.to_string(),
                memory_prefix: memory_prefix.clone(),
            }) as Box<dyn Tool>
        })
        .collect::<Vec<_>>();

    Arc::new(resolved)
}

pub(crate) async fn refresh_tools_for_explicit_allowlist(
    source: &Arc<Vec<Box<dyn Tool>>>,
    allowed_tools: &[String],
) -> anyhow::Result<()> {
    let normalized = allowed_tools
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if normalized.is_empty() || normalized.as_slice() == ["*"] {
        return Ok(());
    }
    if normalized.contains(&"*") {
        anyhow::bail!("allowed_tools must use '*' exclusively");
    }
    if normalized
        .iter()
        .all(|name| source.iter().any(|tool| tool.supports_name(name)))
    {
        return Ok(());
    }

    for tool in source.iter() {
        tool.refresh().await?;
    }
    let unresolved = normalized
        .into_iter()
        .filter(|name| !source.iter().any(|tool| tool.supports_name(name)))
        .collect::<Vec<_>>();
    if !unresolved.is_empty() {
        anyhow::bail!(
            "Allowed tools are not registered after dynamic discovery: {}",
            unresolved.join(", ")
        );
    }
    Ok(())
}
