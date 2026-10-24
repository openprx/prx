#![cfg(test)]
//! Regression coverage using the production tool registry and MCP publication.
use super::*;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogSources, SurfacePolicy};

const MCP_SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if 'id' not in message:
        continue
    method = message.get('method')
    if method == 'initialize':
        result = {'protocolVersion': '2024-11-05', 'capabilities': {'tools': {}},
                  'serverInfo': {'name': 'prx_publication_fixture', 'version': '1.0'}}
    elif method == 'tools/list':
        result = {'tools': [{'name': 'inspect', 'description': 'Read fixture evidence',
                            'inputSchema': {'type': 'object', 'properties': {}}}]}
    else:
        result = {}
    print(json.dumps({'jsonrpc': '2.0', 'id': message['id'], 'result': result}), flush=True)
"#;

#[tokio::test]
async fn cold_mcp_publication_survives_first_selected_tool_refresh() {
    let temp = tempfile::tempdir().expect("isolated workspace");
    let mut config = Config::default();
    config.workspace_dir = temp.path().join("workspace");
    config.config_path = temp.path().join("config.toml");
    config.mcp.servers.insert(
        "prx_inventory".into(),
        crate::config::McpServerConfig {
            command: Some("python3".into()),
            args: vec!["-u".into(), "-c".into(), MCP_SERVER.into()],
            ..Default::default()
        },
    );
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(&config.workspace_dir).expect("memory"));
    let result = all_tools_with_runtime_ext(
        Arc::new(config.clone()),
        crate::config::new_shared(config.clone()),
        &security,
        Arc::new(NativeRuntime::new()),
        Arc::clone(&memory),
        None,
        None,
        &config.browser,
        &config.http_request,
        &config.workspace_dir,
        &config.agents,
        None,
        &config,
    );
    let mcp = result.mcp_tool.expect("production MCP metadata owner");
    assert!(mcp.capability_snapshot().is_empty(), "cold registry");
    let service = crate::runtime::turn_publication::TurnDecisionPublication {
        generation: crate::config::new_shared(config.clone()).pin(),
        registry: Arc::new(result.tools),
        mcp_tool: Some(Arc::clone(&mcp)),
        security,
        observer: Arc::new(crate::observability::noop::NoopObserver),
        hooks: Arc::new(crate::hooks::HookManager::new(config.workspace_dir)),
        memory,
        plugin_generation: 0,
    }
    .publish(crate::causal_tree::state::TurnSurfaceKind::Interactive)
    .await
    .expect("production publication");
    // The real turn engine refreshes the selected MCP root before exposing
    // execution specs. Initial discovery must already belong to its catalog.
    mcp.refresh().await.expect("first selected refresh");
    let parity = service.check_live_parity();
    let discovered = mcp.capability_snapshot();
    // Rediscovery is a new binding even when the server returns the same
    // schema. A previously published service must still reject that change.
    let refreshed = mcp
        .execute(serde_json::json!({"action": "refresh"}))
        .await
        .expect("forced refresh");
    let stale_parity = service.check_live_parity();
    mcp.retire_servers_except(&std::collections::HashSet::new()).await;
    service
        .shutdown(std::time::Instant::now() + std::time::Duration::from_secs(2))
        .await;
    assert_eq!(discovered.len(), 1, "real stdio discovery");
    assert!(
        parity.is_ok(),
        "first MCP turn must preserve catalog parity: {parity:?}"
    );
    assert!(refreshed.success);
    assert!(
        stale_parity.is_err(),
        "changed bindings must invalidate the old publication"
    );
}

#[tokio::test]
async fn production_registry_preserves_discovered_mcp_catalog_parity() {
    let temp = tempfile::tempdir().expect("isolated workspace");
    let mut config = Config::default();
    config.workspace_dir = temp.path().join("workspace");
    config.config_path = temp.path().join("config.toml");
    config.mcp.servers.insert("prx_inventory".into(), Default::default());
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(&config.workspace_dir).expect("memory"));
    let result = all_tools_with_runtime_ext(
        Arc::new(config.clone()),
        crate::config::new_shared(config.clone()),
        &security,
        Arc::new(NativeRuntime::new()),
        memory,
        None,
        None,
        &config.browser,
        &config.http_request,
        &config.workspace_dir,
        &config.agents,
        None,
        &config,
    );
    let mcp = result.mcp_tool.expect("production MCP metadata owner");
    mcp.test_publish_inventory("prx_inventory", 2)
        .expect("publish discovery");
    let sources = CatalogSources {
        mcp: mcp.capability_snapshot(),
        ..CatalogSources::default()
    };
    let catalog = CapabilityCatalog::build(&result.tools, &sources, &SurfacePolicy::default()).expect("catalog");
    let execution = ToolExecutionService::with_security_policy(result.tools, security);
    for descriptor in execution.current_specs_for_parity() {
        if descriptor.configured {
            let entry = catalog
                .entry_for_public_name(&descriptor.spec.name)
                .expect("catalog entry");
            assert_eq!(entry.root, descriptor.backend_name, "{} backend", descriptor.spec.name);
            assert_eq!(
                entry.fingerprint,
                descriptor.spec.fingerprint(),
                "{} fingerprint",
                descriptor.spec.name
            );
        }
    }
    assert!(catalog.check_parity(&execution).is_ok());
}
