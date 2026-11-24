//! Tools the registry's own level would filter but a Full caller may use.
//!
//! One MCP registry serves every caller, and it is built with Main's level.
//! When Main is below Full but a named agent can run at Full, a tool that
//! the allow/deny lists remove at Main's level is still loaded and tagged
//! `full_only`: callers below Full do not see it and cannot call it, a Full
//! caller does both. With no Full caller configured the registry filters
//! exactly as before and nothing here is reachable.
use super::{McpServerConfig, McpTool, RuntimeState};
use crate::config::DelegateAgentConfig;
use crate::security::AutonomyLevel;
use std::collections::HashMap;

/// Whether some configured caller other than a Main below Full can run at
/// Full: an enabled named agent whose own level is Full.
pub(crate) fn has_full_agent(agents: &HashMap<String, DelegateAgentConfig>) -> bool {
    agents
        .values()
        .any(|agent| agent.enabled && agent.autonomy == Some(AutonomyLevel::Full))
}

impl McpTool {
    /// Keep tools filtered by the allow/deny lists, tagged for Full callers
    /// (the registry has a Full caller although Main is below Full).
    #[must_use]
    pub fn with_full_caller(self, full_caller: bool) -> Self {
        self.state.write().keep_full_only = full_caller && !self.full();
        self
    }

    /// Whether the load step keeps what Main's level would filter.
    pub(super) fn keeps_full_only(&self) -> bool {
        self.state.read().keep_full_only
    }
}

impl RuntimeState {
    /// Whether the allow/deny lists remove `tool` for a caller below Full:
    /// the server's own lists and, when kept, the narrowed view of the
    /// workspace `mcp.json` lists.
    pub(super) fn tool_full_only(&self, server: &str, config: &McpServerConfig, tool: &str) -> bool {
        !McpTool::tool_allowed(self.below_full.get(server).unwrap_or(config), tool)
    }
}
