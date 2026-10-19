#![cfg(test)]

use super::*;

#[test]
fn session_worker_secret_follows_the_resolved_config_dir_instead_of_home() {
    let _lock = crate::config::schema::process_config_dir_test_lock();
    let scoped = std::path::PathBuf::from("/tmp/openprx-secret-scope");
    let previous = crate::config::schema::swap_process_config_dir_for_tests(Some(scoped.clone()));
    let scoped_path = session_secret_path();
    crate::config::schema::swap_process_config_dir_for_tests(None);
    let unscoped_path = session_secret_path();
    crate::config::schema::swap_process_config_dir_for_tests(previous);

    assert_eq!(
        scoped_path,
        Some(scoped.join("keys").join("session_worker.secret")),
        "a scoped run must keep its session-worker secret inside the resolved config dir"
    );
    assert_eq!(
        unscoped_path,
        crate::config::default_openprx_dir().map(|dir| dir.join("keys").join("session_worker.secret")),
        "without a published config dir the default ~/.openprx location is kept"
    );
}

#[tokio::test]
async fn wildcard_tool_inheritance_preserves_dynamic_specs_and_execution() {
    let source: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(DynamicCapabilityTool)]);
    let allowed = vec!["*".to_string()];
    let tools = resolve_tools_for_agent(source, "worker", MemoryScope::Shared, Some(&allowed), &[]);

    assert_eq!(tools.len(), 1);
    let proxy = tools.first().expect("wildcard inherits root tool");
    assert_eq!(proxy.name(), "capability_call");
    assert!(proxy.supports_name("capability__dynamic"));
    assert!(
        proxy.specs().iter().any(|spec| spec.name == "capability__dynamic"),
        "dynamic MCP/WASM-style aliases must survive inheritance"
    );
    let result = proxy
        .execute_named("capability__dynamic", json!({"value": 1}))
        .await
        .expect("dynamic alias executes");
    assert_eq!(result.output, "capability__dynamic");
}

#[tokio::test]
async fn explicit_dynamic_alias_is_discovered_before_child_selection() {
    let refreshed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let source: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(RefreshableDynamicCapabilityTool {
        refreshed: Arc::clone(&refreshed),
    })]);
    let allowed = vec!["refreshable__alias".to_string()];

    refresh_tools_for_explicit_allowlist(&source, &allowed)
        .await
        .expect("dynamic discovery");
    let selected = resolve_tools_for_agent(source, "worker", MemoryScope::Shared, Some(&allowed), &[]);

    assert!(refreshed.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].name(), "refreshable__alias");
    assert_eq!(selected[0].specs()[0].name, "refreshable__alias");
}

#[test]
fn restricted_spawn_allowlist_inherits_transcript_recovery_dependency() {
    let source: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
        Box::new(DynamicCapabilityTool),
        Box::new(crate::tools::TranscriptHistoryLookupTool::new(Arc::new(
            crate::memory::NoneMemory::new(),
        ))),
    ]);
    let allowed = vec!["capability_call".to_string()];

    let tools = resolve_tools_for_agent(source, "worker", MemoryScope::Shared, Some(&allowed), &[]);
    let names = tools.iter().map(|tool| tool.name()).collect::<Vec<_>>();
    assert_eq!(
        names,
        vec!["capability_call", crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME]
    );
}

/// D-098: a registry that was bound and is gone (its configuration was
/// replaced) refuses the spawn, instead of running the sub-agent
/// silently without tools. A tool that never had one runs as before.
#[tokio::test]
async fn a_spawn_whose_registry_is_gone_is_refused() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    assert!(matches!(tool.tools_handle().get(), Ok(None)));
    {
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
        assert!(tool.tools_handle().set(&registry));
        assert!(matches!(tool.tools_handle().get(), Ok(Some(_))));
    }
    assert!(
        tool.tools_handle().get().is_err(),
        "a released registry must not read as unbound"
    );

    tool.set_default_recipient(Some("test-recipient".to_string())).await;
    let outcome = execute_admitted_spawn(
        &tool,
        json!({
            "task": "a task for a replaced configuration",
            "_zc_scope_trusted": true,
            "_zc_scope": {
                "sender": "operator",
                "channel": "silent",
                "chat_type": "direct",
                "chat_id": "chat-1"
            }
        }),
    )
    .await;
    let text = match outcome {
        Ok(result) => {
            assert!(!result.success, "the spawn was accepted: {:?}", result.output);
            result.error.unwrap_or_default()
        }
        Err(error) => error.to_string(),
    };
    assert!(text.contains("tool registry is gone"), "{text}");
}

fn router_model(provider: &str, model_id: &str, max_context: usize) -> crate::config::RouterModelConfig {
    crate::config::RouterModelConfig {
        provider: provider.to_string(),
        model_id: model_id.to_string(),
        cost_per_million_tokens: 1.0,
        max_context,
        reserved_output_tokens: None,
        latency_ms: 1_000,
        categories: vec!["code".to_string()],
        elo_rating: 1_000.0,
    }
}

fn child_compaction_config(models: Vec<crate::config::RouterModelConfig>) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.router.models = models;
    config
}

#[test]
fn sessions_spawn_child_model_route_200k_beats_1m_parent() {
    let config = child_compaction_config(vec![
        router_model("anthropic", "claude-opus-4-8", 1_000_000),
        router_model("openrouter", "small-child", 200_000),
    ]);
    let catalog = providers::selection::catalog_for(&config);

    let resolved = resolve_pinned_child_compaction(&config, &catalog, "openrouter", "small-child");

    assert_eq!(resolved.config.max_context_tokens, 200_000);
    assert_eq!(
        resolved.max_context_source,
        crate::router::context::ContextWindowSource::RouterModelConfig
    );
}

#[test]
fn sessions_spawn_process_manifest_carries_compaction_and_owner_isolation() {
    let config = child_compaction_config(vec![
        router_model("anthropic", "claude-opus-4-8", 1_000_000),
        router_model("openrouter", "small-child", 200_000),
    ]);
    let catalog = providers::selection::catalog_for(&config);
    let resolved = resolve_pinned_child_compaction(&config, &catalog, "openrouter", "small-child");
    let temp = tempfile::TempDir::new().expect("tempdir");
    let lineage = SpawnLineage {
        owner_id: None,
        owner_isolated: true,
        topic_id: None,
        parent_task_id: None,
        source_message_event_id: None,
    };
    let scope = SpawnScope {
        sender: "alice".to_string(),
        channel: "telegram".to_string(),
        chat_type: "direct".to_string(),
        chat_id: "chat-1".to_string(),
        owner_id: None,
        owner_isolated: false,
        topic_id: None,
        parent_task_id: None,
        source_message_event_id: None,
        config_generation_id: Some(17),
        config_source_revision: Some("revision-17".to_string()),
    };

    let (manifest, _, _) = build_session_worker_manifest(
        "run-child",
        "task",
        "openrouter",
        "small-child",
        None,
        None,
        None,
        None,
        false,
        false,
        0.2,
        temp.path().join("worker"),
        temp.path().join("brain.db"),
        temp.path().display().to_string(),
        PROCESS_MEMORY_STRATEGY_SHARED,
        "sqlite",
        temp.path().join("memory").join("brain.db"),
        temp.path().join("worker").join("brain.db"),
        temp.path().join("config"),
        &"0".repeat(64),
        None,
        Some("Use tools before reporting completion."),
        MemoryEventRecording::default(),
        &[],
        None,
        Some(&scope),
        &lineage,
        0,
        "sessions_spawn:test",
        None,
        &resolved.config,
        &[],
        None,
        None,
    )
    .expect("manifest");

    let manifest_config = manifest.compaction_config.clone().expect("manifest compaction config");
    assert_eq!(manifest_config.max_context_tokens, 200_000);
    assert_eq!(manifest.model, "small-child");
    assert_eq!(
        manifest.system_prompt.as_deref(),
        Some("Use tools before reporting completion.")
    );
    assert!(
        manifest.channel_excluded_tools.is_empty(),
        "an operator-rooted spawn hides nothing"
    );
    assert_ne!(manifest_config.max_context_tokens, 1_000_000);
    assert_eq!(manifest.runtime_config_generation_id, Some(17));
    assert_eq!(manifest.runtime_config_source_revision.as_deref(), Some("revision-17"));
    assert!(manifest.owner_isolated);
}

#[test]
fn sessions_spawn_process_manifest_carries_the_channel_exclusions_under_the_seal() {
    // `mode: "process"` used to be a one-word way out of the channel
    // boundary: the manifest had no field for it, so the worker started
    // with the whole operations surface back. The list travels in the
    // manifest now, and the manifest is what the capability seals.
    let config = crate::config::Config::default();
    let catalog = providers::selection::catalog_for(&config);
    let resolved = resolve_pinned_child_compaction(&config, &catalog, "openrouter", "small-child");
    let temp = tempfile::TempDir::new().expect("tempdir");
    let lineage = SpawnLineage {
        owner_id: None,
        owner_isolated: false,
        topic_id: None,
        parent_task_id: None,
        source_message_event_id: None,
    };
    let hidden = crate::config::ToolTieringConfig::default()
        .for_channel_surface()
        .always_exclude;
    assert!(
        hidden.iter().any(|name| name == "gateway"),
        "fixture validity: the default channel surface hides `gateway`"
    );

    let build = |excluded: &[String]| {
        build_session_worker_manifest(
            "run-child",
            "task",
            "openrouter",
            "small-child",
            None,
            None,
            None,
            None,
            false,
            false,
            0.2,
            temp.path().join("worker"),
            temp.path().join("brain.db"),
            temp.path().display().to_string(),
            PROCESS_MEMORY_STRATEGY_SHARED,
            "sqlite",
            temp.path().join("memory").join("brain.db"),
            temp.path().join("worker").join("brain.db"),
            temp.path().join("config"),
            &"0".repeat(64),
            None,
            None,
            MemoryEventRecording::default(),
            &[],
            None,
            None,
            &lineage,
            0,
            "sessions_spawn:test",
            None,
            &resolved.config,
            excluded,
            None,
            None,
        )
        .expect("manifest")
    };

    let (bounded, sealed, _) = build(&hidden);
    assert_eq!(
        bounded.channel_excluded_tools, hidden,
        "the spawning surface's hidden set has to cross the process boundary"
    );
    let (open, open_seal, _) = build(&[]);
    assert!(open.channel_excluded_tools.is_empty(), "expected an empty value");
    assert_ne!(
        sealed, open_seal,
        "the exclusions are covered by the capability seal, so they cannot be stripped in transit"
    );
}

#[test]
fn parse_spawn_scope_preserves_config_generation() {
    let scope = parse_spawn_scope(&json!({
        "_zc_scope_trusted": true,
        "_zc_scope": {
            "sender": "alice",
            "channel": "telegram",
            "chat_type": "direct",
            "chat_id": "chat-1",
            "config_generation_id": 17,
            "config_source_revision": "revision-17"
        }
    }))
    .expect("trusted scope should parse");

    assert_eq!(scope.config_generation_id, Some(17));
    assert_eq!(scope.config_source_revision.as_deref(), Some("revision-17"));
}
