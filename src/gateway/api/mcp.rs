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

    let mut servers = Vec::new();
    for (name, server_config) in &mcp.servers {
        let url = match &server_config.url {
            Some(u) => u.clone(),
            None => server_config
                .command
                .clone()
                .unwrap_or_else(|| "stdio".to_string()),
        };
        let status = if server_config.enabled && mcp.enabled {
            "connected"
        } else {
            "disconnected"
        };
        servers.push(McpServerInfo {
            name: name.clone(),
            url,
            status: status.to_string(),
            tools: Vec::new(), // Runtime tool discovery not available from config alone
        });
    }

    servers.sort_by(|a, b| a.name.cmp(&b.name));

    Json(McpServersResponse { servers })
}
