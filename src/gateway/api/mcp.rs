use super::AppState;
use axum::{Json, extract::State};
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
    last_error: Option<String>,
    last_refresh_at: Option<String>,
}

#[derive(Serialize)]
pub(super) struct McpServersResponse {
    servers: Vec<McpServerInfo>,
    config_error: Option<String>,
}

pub async fn get_mcp_servers(State(state): State<AppState>) -> Json<McpServersResponse> {
    let config = state.config.load_full();
    let mcp = &config.mcp;

    // Collect runtime-discovered tools if available.
    let runtime = state.pin_turn_runtime();
    let discovered = runtime
        .mcp_tool
        .as_ref()
        .map(|t| t.list_discovered_tools())
        .unwrap_or_default();
    let runtime_info = runtime
        .mcp_tool
        .as_ref()
        .map(|tool| {
            tool.server_runtime_info()
                .into_iter()
                .map(|server| (server.name.clone(), server))
                .collect::<std::collections::HashMap<_, _>>()
        })
        .unwrap_or_default();

    let mut servers = Vec::new();
    for (name, server_config) in &mcp.servers {
        let url = server_config.url.as_ref().map_or_else(
            || server_config.command.clone().unwrap_or_else(|| "stdio".to_string()),
            |u| u.clone(),
        );

        let info = runtime_info.get(name);
        let status = info.map_or("configured", |info| info.status.as_str());

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
            last_error: info.and_then(|info| info.last_error.clone()),
            last_refresh_at: info.and_then(|info| info.last_refresh_at.clone()),
        });
    }

    servers.sort_by(|a, b| a.name.cmp(&b.name));

    let config_error = runtime.mcp_tool.as_ref().and_then(|tool| tool.config_error());
    Json(McpServersResponse { servers, config_error })
}
