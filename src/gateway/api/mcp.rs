use super::AppState;
use axum::{extract::State, Json};
use serde::Serialize;

#[derive(Serialize)]
struct McpToolInfo {
    name: String,
    description: String,
}

#[derive(Serialize)]
struct McpServerInfo {
    name: String,
    url: String,
    status: String,
    tools: Vec<McpToolInfo>,
}

#[derive(Serialize)]
pub(super) struct McpServersResponse {
    servers: Vec<McpServerInfo>,
}

pub async fn get_mcp_servers(State(state): State<AppState>) -> Json<McpServersResponse> {
    let config = state.config.lock();
    let mcp = &config.mcp;

    // Collect runtime-discovered tools if available.
    let discovered = state
        .mcp_tool
        .as_ref()
        .map(|t| t.list_discovered_tools())
        .unwrap_or_default();

    let mut servers = Vec::new();
    for (name, server_config) in &mcp.servers {
        let url = match &server_config.url {
            Some(u) => u.clone(),
            None => server_config
                .command
                .clone()
                .unwrap_or_else(|| "stdio".to_string()),
        };

        let has_runtime_tools = discovered.contains_key(name);
        let status = if !mcp.enabled || !server_config.enabled {
            "disconnected"
        } else if has_runtime_tools {
            "connected"
        } else {
            "connecting"
        };

        let tools: Vec<McpToolInfo> = discovered
            .get(name)
            .map(|entries| {
                entries
                    .iter()
                    .map(|(tool_name, desc)| McpToolInfo {
                        name: tool_name.clone(),
                        description: desc.clone().unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        servers.push(McpServerInfo {
            name: name.clone(),
            url,
            status: status.to_string(),
            tools,
        });
    }

    servers.sort_by(|a, b| a.name.cmp(&b.name));

    Json(McpServersResponse { servers })
}
