//! Unit tests for provider-safe MCP tool aliases.

use super::*;

#[test]
fn parse_alias_name_resolves_server_and_tool() {
    let mut cfg = McpConfig::default();
    cfg.servers.insert(
        "qmd".into(),
        McpServerConfig {
            tool_name_prefix: "mcp".into(),
            ..McpServerConfig::default()
        },
    );

    let mut state = RuntimeState {
        effective_config: cfg,
        ..Default::default()
    };
    state.discovered_tools = HashMap::from([(
        "qmd".into(),
        HashMap::from([("search".into(), DiscoveredToolMeta::default())]),
    )]);
    state.capability_specs = McpTool::capability_specs_from_state(&state);
    let parsed = alias::resolve(&state, "mcp__qmd__search");
    assert_eq!(parsed, Some(("qmd".into(), "search".into())));
}

#[test]
fn parse_alias_name_no_match() {
    assert!(alias::resolve(&RuntimeState::default(), "unrelated_name").is_none());
}

#[test]
fn parse_alias_name_default_prefix() {
    let mut cfg = McpConfig::default();
    cfg.servers.insert(
        "myserver".into(),
        McpServerConfig {
            tool_name_prefix: "mcp".into(),
            ..McpServerConfig::default()
        },
    );
    let mut state = RuntimeState {
        effective_config: cfg,
        ..Default::default()
    };
    state.discovered_tools = HashMap::from([(
        "myserver".into(),
        HashMap::from([("run_query".into(), DiscoveredToolMeta::default())]),
    )]);
    state.capability_specs = McpTool::capability_specs_from_state(&state);
    let parsed = alias::resolve(&state, "mcp__myserver__run_query");
    assert_eq!(parsed, Some(("myserver".into(), "run_query".into())));
}

#[test]
fn alias_name_format() {
    let table = alias::alias_table([("server1", "mcp", vec!["search"])]);
    assert_eq!(table["server1"]["search"], "mcp__server1__search");
}
