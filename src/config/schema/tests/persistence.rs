#![cfg(test)]

use super::*;
use tokio::test;

#[test]
async fn explicit_hosted_latency_ceiling_loads_without_an_upgrade_warning() {
    let _serial = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let version = crate::causal_tree::config::CAUSAL_CONFIG_SCHEMA_VERSION;
    std::fs::write(&path, format!("default_temperature = 0.7\n[auth]\ncodex_auth_json_auto_import = false\n[causal_tree]\nschema_version = {version}\nenabled = true\n[causal_tree.backend]\nkind = \"openrouter\"\nmodel = \"typesafe/jev-1.13\"\npinned_snapshot = \"typesafe/jev-1.13-20260917\"\n[causal_tree.policy]\nextra_latency_budget_ms = 500\n")).unwrap();
    let (loaded, logs) = capture_logs(|| Config::load_from_path(&path, dir.path().join("workspace")));
    let config = loaded.unwrap();
    assert!(config.causal_tree.policy.extra_latency_budget_ms_explicit);
    assert_eq!(config.causal_tree.policy.extra_latency_budget_ms, 500);
    assert!(config.decision_latency_budget_warning().is_none());
    assert!(!logs.contains("below the catalog recommendation"), "{logs}");
    assert!(
        !crate::doctor::diagnose(&config)
            .items
            .iter()
            .any(|item| item.message.contains("below the catalog recommendation"))
    );
}

#[test]
async fn schema_one_hosted_backend_warns_on_load_without_raising_the_latency_budget() {
    let _serial = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.lock().await;
    let dir = tempfile::TempDir::new().expect("test: private config directory");
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "default_temperature = 0.7\n\
             [auth]\ncodex_auth_json_auto_import = false\n\
             [causal_tree]\nschema_version = 1\nenabled = true\n\
             [causal_tree.backend]\nkind = \"openrouter\"\nmodel = \"typesafe/jev-1.13\"\n\
             pinned_snapshot = \"typesafe/jev-1.13-20260917\"\n\
             [causal_tree.policy]\nextra_latency_budget_ms = 500\n",
    )
    .expect("test: write old hosted configuration");
    let (loaded, logs) = capture_logs(|| Config::load_from_path(&path, dir.path().join("workspace")));
    let loaded = loaded.expect("test: old hosted config loads");
    assert_eq!(loaded.causal_tree.policy.extra_latency_budget_ms, 500);
    assert!(!loaded.causal_tree.policy.extra_latency_budget_ms_explicit);
    assert!(logs.contains("below the catalog recommendation"), "{logs}");
    assert!(logs.contains("500") && logs.contains("3000"), "{logs}");
}

#[tokio::test]
async fn config_save_and_load_tmpdir() {
    let dir = std::env::temp_dir().join("openprx_test_config");
    let _ = fs::remove_dir_all(&dir).await;
    fs::create_dir_all(&dir).await.unwrap();

    let config_path = dir.join("config.toml");
    let config = Config {
        workspace_dir: dir.join("workspace"),
        config_path: config_path.clone(),
        api_key: Some("sk-roundtrip".into()),
        api_url: None,
        default_provider: Some("openrouter".into()),
        default_model: Some("test-model".into()),
        default_temperature: 0.9,
        model_settings: Default::default(),
        providers: ProvidersConfig::default(),
        observability: ObservabilityConfig::default(),
        autonomy: AutonomyConfig::default(),
        runtime: RuntimeConfig::default(),
        reliability: ReliabilityConfig::default(),
        scheduler: SchedulerConfig::default(),
        sessions_spawn: SessionsSpawnConfig::default(),
        chat: ChatConfig::default(),
        self_system: SelfSystemConfig::default(),
        skills: SkillsConfig::default(),
        skill_rag: SkillRagConfig::default(),
        plugins: crate::config::plugins::PluginsConfig::default(),
        skills_lifecycle: crate::config::skills_lifecycle::SkillsLifecycleConfig::default(),
        model_routes: Vec::new(),
        embedding_routes: Vec::new(),
        query_classification: QueryClassificationConfig::default(),
        task_routing: TaskRoutingConfig::default(),
        router: RouterConfig::default(),
        smart_group: SmartGroupConfig::default(),
        heartbeat: HeartbeatConfig::default(),
        xin: crate::xin::XinConfig::default(),
        cron: CronConfig::default(),
        channels_config: ChannelsConfig::default(),
        memory: MemoryConfig::default(),
        identity_bindings: Vec::new(),
        user_policies: Vec::new(),
        storage: StorageConfig::default(),
        tunnel: TunnelConfig::default(),
        gateway: GatewayConfig::default(),
        webhook: MemoryWebhookConfig::default(),
        composio: ComposioConfig::default(),
        mcp: McpConfig::default(),
        mcp_server: McpServerRuntimeConfig::default(),
        a2a: A2aConfig::default(),
        auth: AuthConfig::default(),
        secrets: SecretsConfig::default(),
        browser: BrowserConfig::default(),
        http_request: HttpRequestConfig::default(),
        multimodal: MultimodalConfig::default(),
        web_search: WebSearchConfig::default(),
        proxy: ProxyConfig::default(),
        agent: AgentConfig::default(),
        identity: IdentityConfig::default(),
        cost: CostConfig::default(),
        nodes: NodesConfig::default(),
        agents: HashMap::new(),
        media: MediaConfig::default(),
        causal_tree: crate::causal_tree::CausalTreeConfig::default(),
        security: SecurityConfig::default(),
        compliance: ComplianceConfig::default(),
        tool_tiering: ToolTieringConfig::default(),
    };

    config.save().await.unwrap();
    assert!(config_path.exists());

    let contents = tokio::fs::read_to_string(&config_path).await.unwrap();
    let loaded: Config = toml::from_str(&contents).unwrap();
    assert!(
        loaded
            .api_key
            .as_deref()
            .is_some_and(crate::security::SecretStore::is_encrypted)
    );
    let store = crate::security::SecretStore::new(&dir, true);
    let decrypted = store.decrypt(loaded.api_key.as_deref().unwrap()).unwrap();
    assert_eq!(decrypted, "sk-roundtrip");
    assert_eq!(loaded.default_model.as_deref(), Some("test-model"));
    assert!((loaded.default_temperature - 0.9).abs() < f64::EPSILON);

    let _ = fs::remove_dir_all(&dir).await;
}

/// Every path on the retired list must genuinely be gone from the schema.
///
/// This is the dangerous direction of the migration layer: listing a key
/// that still exists would make the loader silently discard a setting the
/// operator is relying on, which is exactly the "configuration is never
/// silently ignored" promise the strict loader exists to keep. So each
/// listed path is planted into an otherwise-valid document and must come
/// back as unrecognised.
#[tokio::test]
async fn every_retired_key_is_actually_absent_from_the_schema() {
    use crate::config::deprecations::{DEPRECATED_CONFIG_KEYS, DeprecatedShape};

    for key in DEPRECATED_CONFIG_KEYS {
        let mut document = toml::Value::try_from(Config::default()).expect("test: serialize default config");
        let planted = match key.shape {
            DeprecatedShape::Field => toml::Value::Integer(1),
            DeprecatedShape::Section => {
                let mut table = toml::map::Map::new();
                table.insert("planted".to_string(), toml::Value::Integer(1));
                toml::Value::Table(table)
            }
        };
        // The default document already has the parent tables, so an
        // "if absent" insert is enough to plant the retired leaf.
        assert!(
            insert_toml_path_if_absent(&mut document, key.path, planted),
            "test: could not plant {}",
            key.path.join(".")
        );

        let mut unknown = Vec::new();
        let _ignored: Result<Config, _> = serde_ignored::deserialize(document.into_deserializer(), |path| {
            unknown.push(path.to_string());
        });

        let dotted = key.path.join(".");
        assert!(
            unknown.iter().any(|found| found.starts_with(&dotted)),
            "{dotted} is on the retired list but the schema still accepts it; stripping it would silently \
                 discard a live setting. Reported unknown paths: {unknown:?}"
        );
    }
}

/// A config written against an older prx must keep loading. Every key the
/// unbounded-runtime work removed is absorbed with a warning, and the
/// surviving keys in the same tables must still take effect — a migration
/// layer that quietly dropped its neighbours would be worse than the bail.
#[tokio::test]
async fn config_load_ignores_removed_keys_and_keeps_the_rest() {
    let dir = std::env::temp_dir().join(format!("openprx_test_deprecated_keys_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).await.unwrap();

    let config_path = dir.join("config.toml");
    fs::write(
        &config_path,
        r#"
default_temperature = 0.7
default_model = "kept"

[runtime]
idle_hang_max_total_secs = 86400

[agent]
max_tool_iterations = 150
parallel_tools = true
read_only_tool_timeout_secs = 30
concurrency_rollout_stage = "stage_a"
concurrency_rollout_channels = ["telegram"]
concurrency_rollback_timeout_rate_threshold = 0.2

[agents.researcher]
provider = "openrouter"
model = "kept-agent-model"
max_iterations = 25

[sessions_spawn]
max_concurrent = 64
max_spawn_depth = 8
max_children_per_agent = 32
process_memory_strategy = "isolated_private"

[autonomy]
level = "full"
workspace_only = false
forbidden_paths = []
max_actions_per_hour = 4294967295
max_cost_per_day_cents = 4294967295

[gateway]
port = 3120
request_timeout_secs = 60

[memory]
backend = "sqlite"
auto_save = true

[memory.events]
record_tool_events = true
retention_days = 14

[security.resources]
max_memory_mb = 512
max_cpu_time_seconds = 300
max_subprocesses = 10

[security.audit]
max_size_mb = 42
"#,
    )
    .await
    .unwrap();

    let loaded = Config::load_from_path(&config_path, dir.join("workspace"))
        .expect("a config carrying removed keys must still load");

    // Neighbouring keys in every touched table survived the strip.
    assert_eq!(loaded.default_model.as_deref(), Some("kept"));
    assert_eq!(loaded.agents["researcher"].model, "kept-agent-model");
    assert_eq!(loaded.sessions_spawn.process_memory_strategy, "isolated_private");
    assert_eq!(loaded.autonomy.max_cost_per_day_cents, 4_294_967_295);
    assert_eq!(loaded.gateway.port, 3120);
    assert!(loaded.memory.events.record_tool_events);
    assert_eq!(loaded.security.audit.max_size_mb, 42);

    let _ = fs::remove_dir_all(&dir).await;
}

/// A whole retired table, not just a leaf, must be absorbed — including the
/// case where stripping it leaves its parent empty.
#[tokio::test]
async fn config_load_ignores_a_removed_section_on_its_own() {
    let dir = std::env::temp_dir().join(format!("openprx_test_deprecated_section_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).await.unwrap();

    let config_path = dir.join("config.toml");
    fs::write(
        &config_path,
        r#"
default_temperature = 0.7
default_model = "section-only"

[security]
[security.resources]
max_memory_mb = 512
max_cpu_time_seconds = 300
max_subprocesses = 10
memory_monitoring = true
"#,
    )
    .await
    .unwrap();

    let loaded = Config::load_from_path(&config_path, dir.join("workspace"))
        .expect("a lone removed section must not stop the load");
    assert_eq!(loaded.default_model.as_deref(), Some("section-only"));
    assert_eq!(loaded.security.audit.log_path, default_audit_log_path());

    let _ = fs::remove_dir_all(&dir).await;
}

/// The leniency above is scoped to the published list. A misspelling that
/// merely *resembles* a retired key must still stop the load, or the
/// migration layer would have thrown away the strictness it was added to
/// protect.
#[tokio::test]
async fn config_load_still_rejects_a_misspelled_key() {
    let dir = std::env::temp_dir().join(format!("openprx_test_typo_key_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).await.unwrap();

    let config_path = dir.join("config.toml");
    fs::write(
        &config_path,
        r#"
default_temperature = 0.7

[autonomy]
level = "full"
workspace_only = false
forbidden_paths = []
max_cost_per_day_cents = 500
max_actions_per_hourr = 20
"#,
    )
    .await
    .unwrap();

    let error = Config::load_from_path(&config_path, dir.join("workspace")).expect_err("a typo must still be refused");
    let message = error.to_string();
    assert!(
        message.contains("Unknown configuration path"),
        "a typo must be reported as unknown, got: {message}"
    );
    assert!(
        message.contains("autonomy.max_actions_per_hourr"),
        "the message must name the offending path, got: {message}"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// A retired key in a `config.d` fragment is the shape real deployments
/// actually have, and the fragment merge must not reintroduce it after the
/// main file has been cleaned.
#[tokio::test]
async fn config_load_ignores_removed_keys_inside_config_d_fragments() {
    let dir = std::env::temp_dir().join(format!("openprx_test_deprecated_fragment_{}", uuid::Uuid::new_v4()));
    let config_dir = dir.join("config.d");
    fs::create_dir_all(&config_dir).await.unwrap();

    fs::write(
        dir.join("config.toml"),
        "default_temperature = 0.7\ndefault_model = \"base\"\n",
    )
    .await
    .unwrap();
    fs::write(
        config_dir.join("security.toml"),
        r#"
[autonomy]
level = "full"
workspace_only = false
forbidden_paths = []
max_actions_per_hour = 4294967295
max_cost_per_day_cents = 4294967295

[security]
[security.resources]
max_memory_mb = 512
"#,
    )
    .await
    .unwrap();

    let loaded = Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
        .expect("a fragment carrying removed keys must still load");
    assert_eq!(loaded.autonomy.max_cost_per_day_cents, 4_294_967_295);

    let _ = fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn config_split_and_reload_roundtrip_matches_original() {
    let _env_guard = env_override_lock().await;
    let dir = std::env::temp_dir().join(format!("openprx_test_split_reload_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).await.unwrap();

    let mut config = Config::default();
    config.workspace_dir = dir.join("workspace");
    config.config_path = dir.join("config.toml");
    config.default_model = Some("roundtrip-model".into());
    config.channels_config.telegram = Some(TelegramConfig {
        bot_token: "roundtrip-token".into(),
        allowed_users: vec!["zeroclaw_user".into()],
        stream_mode: StreamMode::default(),
        draft_update_interval_ms: default_draft_update_interval_ms(),
        interrupt_on_new_message: false,
        mention_only: false,
        group_reply_mode: None,
    });
    config.memory.backend = "markdown".into();
    config.storage.provider.config.provider = "postgres".into();
    config.storage.provider.config.db_url = Some("postgres://user:pw@host/db".into());
    // Keep test deterministic even if process env mutates proxy defaults elsewhere.
    config.proxy = ProxyConfig::default();
    config.security.audit.max_size_mb = 120;
    config.autonomy.max_cost_per_day_cents = 42;
    config.agents.insert(
        "worker".into(),
        DelegateAgentConfig {
            provider: "openrouter".into(),
            model: "test-model".into(),
            system_prompt: Some("delegate".into()),
            api_key: Some("agent-secret".into()),
            credential_ref: None,
            temperature: Some(0.2),
            max_depth: 4,
            agentic: true,
            allowed_tools: vec!["shell".into()],
            identity_dir: None,
            memory_scope: None,
            spawn_enabled: Some(true),
        },
    );

    let expected = serde_json::to_value(&config).unwrap();
    crate::config::files::write_split_config(&config, false).await.unwrap();

    let reloaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).unwrap();
    let actual = serde_json::to_value(&reloaded).unwrap();
    assert_eq!(actual, expected);

    let _ = fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn config_split_preserves_unmanaged_fragments() {
    let dir = tempfile::TempDir::new().unwrap();
    let config_dir = dir.path().join("config.d");
    fs::create_dir_all(&config_dir).await.unwrap();

    let mut config = Config::default();
    config.workspace_dir = dir.path().join("workspace");
    config.config_path = dir.path().join("config.toml");
    config.default_model = Some("preserve-fragments".into());
    config.memory.backend = "markdown".into();

    fs::write(config_dir.join("99-local.toml"), "default_temperature = 0.9\n")
        .await
        .unwrap();

    crate::config::files::write_split_config(&config, false).await.unwrap();

    assert!(config_dir.join("99-local.toml").exists());
}

#[tokio::test]
async fn config_merge_refuses_unmanaged_fragments() {
    let dir = tempfile::TempDir::new().unwrap();
    let config_dir = dir.path().join("config.d");
    fs::create_dir_all(&config_dir).await.unwrap();

    let mut config = Config::default();
    config.workspace_dir = dir.path().join("workspace");
    config.config_path = dir.path().join("config.toml");
    config.default_model = Some("merge-guard".into());
    config.save().await.unwrap();

    fs::write(config_dir.join("99-local.toml"), "default_temperature = 0.9\n")
        .await
        .unwrap();

    let error = crate::config::files::merge_split_config(&config).await.unwrap_err();
    assert!(
        error.to_string().contains("unmanaged config fragments"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn config_save_encrypts_nested_credentials() {
    let dir = std::env::temp_dir().join(format!("openprx_test_nested_credentials_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).await.unwrap();

    let mut config = Config::default();
    config.workspace_dir = dir.join("workspace");
    config.config_path = dir.join("config.toml");
    config.api_key = Some("root-credential".into());
    config.composio.api_key = Some("composio-credential".into());
    config.browser.computer_use.api_key = Some("browser-credential".into());
    config.web_search.brave_api_key = Some("brave-credential".into());
    config.storage.provider.config.db_url = Some("postgres://user:pw@host/db".into());

    config.agents.insert(
        "worker".into(),
        DelegateAgentConfig {
            provider: "openrouter".into(),
            model: "model-test".into(),
            system_prompt: None,
            api_key: Some("agent-credential".into()),
            credential_ref: None,
            temperature: None,
            max_depth: 3,
            agentic: false,
            allowed_tools: Vec::new(),
            identity_dir: None,
            memory_scope: None,
            spawn_enabled: None,
        },
    );

    config.save().await.unwrap();

    let contents = tokio::fs::read_to_string(config.config_path.clone()).await.unwrap();
    let stored: Config = toml::from_str(&contents).unwrap();
    let store = crate::security::SecretStore::new(&dir, true);

    let root_encrypted = stored.api_key.as_deref().unwrap();
    assert!(crate::security::SecretStore::is_encrypted(root_encrypted));
    assert_eq!(store.decrypt(root_encrypted).unwrap(), "root-credential");

    let composio_encrypted = stored.composio.api_key.as_deref().unwrap();
    assert!(crate::security::SecretStore::is_encrypted(composio_encrypted));
    assert_eq!(store.decrypt(composio_encrypted).unwrap(), "composio-credential");

    let browser_encrypted = stored.browser.computer_use.api_key.as_deref().unwrap();
    assert!(crate::security::SecretStore::is_encrypted(browser_encrypted));
    assert_eq!(store.decrypt(browser_encrypted).unwrap(), "browser-credential");

    let web_search_encrypted = stored.web_search.brave_api_key.as_deref().unwrap();
    assert!(crate::security::SecretStore::is_encrypted(web_search_encrypted));
    assert_eq!(store.decrypt(web_search_encrypted).unwrap(), "brave-credential");

    let worker = stored.agents.get("worker").unwrap();
    let worker_encrypted = worker.api_key.as_deref().unwrap();
    assert!(crate::security::SecretStore::is_encrypted(worker_encrypted));
    assert_eq!(store.decrypt(worker_encrypted).unwrap(), "agent-credential");

    let storage_db_url = stored.storage.provider.config.db_url.as_deref().unwrap();
    assert!(crate::security::SecretStore::is_encrypted(storage_db_url));
    assert_eq!(store.decrypt(storage_db_url).unwrap(), "postgres://user:pw@host/db");

    let _ = fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn config_save_atomic_cleanup() {
    let dir = std::env::temp_dir().join(format!("openprx_test_config_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).await.unwrap();

    let config_path = dir.join("config.toml");
    let mut config = Config::default();
    config.workspace_dir = dir.join("workspace");
    config.config_path = config_path.clone();
    config.default_model = Some("model-a".into());
    config.save().await.unwrap();
    assert!(config_path.exists());

    config.default_model = Some("model-b".into());
    config.save().await.unwrap();

    let contents = tokio::fs::read_to_string(&config_path).await.unwrap();
    assert!(contents.contains("model-b"));

    let names: Vec<String> = ReadDirStream::new(fs::read_dir(&dir).await.unwrap())
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect()
        .await;
    assert!(!names.iter().any(|name| name.contains(".tmp-")));
    assert!(!names.iter().any(|name| name.ends_with(".bak")));

    let _ = fs::remove_dir_all(&dir).await;
}

// ---- FIX-P2-10: write_toml_string_atomic file lock -------------------

#[test]
async fn write_toml_string_atomic_writes_under_lock() {
    let dir = tempfile::TempDir::new().unwrap();
    let target = dir.path().join("config.toml");
    write_toml_string_atomic(&target, "key = 1\n")
        .await
        .expect("atomic write");
    let read_back = tokio::fs::read_to_string(&target).await.unwrap();
    assert_eq!(read_back, "key = 1\n");
}

#[test]
async fn write_toml_string_atomic_repeated_last_writer_wins() {
    // Sequential locked writes succeed and leave no leftover temp file.
    let dir = tempfile::TempDir::new().unwrap();
    let target = dir.path().join("config.toml");
    for i in 0..4 {
        write_toml_string_atomic(&target, &format!("n = {i}\n"))
            .await
            .expect("atomic write");
    }
    let read_back = tokio::fs::read_to_string(&target).await.unwrap();
    assert_eq!(read_back, "n = 3\n");
    // No `.tmp-*` temp file should linger in the directory.
    let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        assert!(!name.contains(".tmp-"), "leftover temp file lingered: {name}");
    }
}

/// D-050 item 1: an unknown value of a runtime-interpreted enumeration is
/// refused when the configuration loads, never read as a fallback.
#[test]
async fn unknown_enumeration_values_are_refused_at_load() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let path = dir.path().join("config.toml");
    for (body, needle) in [
        ("[autonomy.scopes]\ndefault = \"allwo\"\n", "autonomy.scopes.default"),
        (
            "[[user_policies]]\nuser_id = \"u1\"\nrole = \"admin\"\n",
            "user_policies[0].role",
        ),
        (
            "[[user_policies]]\nuser_id = \"u1\"\nrole = \"member\"\nvisibility_ceiling = \"everyone\"\n",
            "user_policies[0].visibility_ceiling",
        ),
        ("[tunnel]\nprovider = \"\"\n", "tunnel.provider"),
        ("[tunnel]\nprovider = \"None\"\n", "tunnel.provider"),
    ] {
        std::fs::write(&path, format!("default_temperature = 0.7\n\n{body}")).expect("test: write config");
        let error = Config::load_from_path(&path, dir.path().join("workspace"))
            .expect_err("test: an unknown value must not load");
        let rendered = format!("{error:#}");
        assert!(rendered.contains(needle), "{body}: {rendered}");
    }
    std::fs::write(
            &path,
            "default_temperature = 0.7\n\n[[user_policies]]\nuser_id = \"u1\"\nrole = \" Owner\"\nvisibility_ceiling = \"PUBLIC\"\n",
        )
        .expect("test: write config");
    Config::load_from_path(&path, dir.path().join("workspace")).expect("test: known values in any case load");
}

#[test]
async fn retired_model_load_keeps_selection_and_logs_published_replacement() {
    let _serial = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "default_temperature = 0.7\ndefault_provider = \"openai\"\ndefault_model = \"gpt-4o\"\n[auth]\ncodex_auth_json_auto_import = false\n").unwrap();
    let (config, logs) = capture_logs(|| Config::load_from_path(&path, dir.path().join("workspace")));
    assert_eq!(config.unwrap().default_model.as_deref(), Some("gpt-4o"));
    assert!(logs.contains("retired") && logs.contains("openai/gpt-5.6"), "{logs}");
}

/// INT-2B AUDIT-SIGN (D-056 DC-2), RED before: `security.audit.sign_events`
/// was never implemented and loaded silently. After: it is accepted at
/// load with a deprecation warning naming it, and ignored.
#[test]
async fn a_stored_sign_events_setting_loads_with_a_deprecation_warning() {
    let _serial = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.lock().await;
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let path = dir.path().join("config.toml");
    for value in ["true", "false"] {
        std::fs::write(
            &path,
            format!("default_temperature = 0.7\n\n[security.audit]\nmax_size_mb = 7\nsign_events = {value}\n"),
        )
        .expect("test: write config");
        let (loaded, logs) = capture_logs(|| Config::load_from_path(&path, dir.path().join("workspace")));
        let loaded = loaded.expect("test: a stored sign_events still loads");
        assert_eq!(loaded.security.audit.max_size_mb, 7);
        assert!(
            logs.contains("security.audit.sign_events") && logs.contains("Ignoring removed configuration key"),
            "sign_events = {value} must be reported as removed: {logs}"
        );
    }
}

/// `[agent] tool_dispatcher` never reached the runtime, which picks the
/// wire form from the provider's capabilities. A stored value loads with a
/// deprecation warning and is ignored.
#[test]
async fn a_stored_tool_dispatcher_setting_loads_with_a_deprecation_warning() {
    let _serial = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.lock().await;
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "default_temperature = 0.7\n\n[agent]\ncompact_context = true\ntool_dispatcher = \"xml\"\n",
    )
    .expect("test: write config");
    let (loaded, logs) = capture_logs(|| Config::load_from_path(&path, dir.path().join("workspace")));
    let loaded = loaded.expect("test: a stored tool_dispatcher still loads");
    assert!(loaded.agent.compact_context);
    assert!(
        logs.contains("agent.tool_dispatcher") && logs.contains("Ignoring removed configuration key"),
        "tool_dispatcher must be reported as removed: {logs}"
    );
}

/// INT-2B a43 (R-C2Kf S-4): a `[[providers.*]]` declaration table is read
/// from `config.toml` or from a managed `config.d/` fragment, and a
/// structured save writes it back to `config.toml`, because no managed
/// fragment owns `[providers]`. An unmanaged fragment is not read.
#[test]
async fn a_provider_table_in_a_managed_fragment_moves_to_config_toml_on_save() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "default_temperature = 0.7\n").expect("test: write config");
    let fragments = dir.path().join("config.d");
    std::fs::create_dir_all(&fragments).expect("test: config.d");
    let table = "[[providers.tool_dialects]]\nprovider = \"vllm\"\ntool_schema = \"raw\"\n";
    std::fs::write(fragments.join("cost.toml"), table).expect("test: managed fragment");
    std::fs::write(
        fragments.join("extra.toml"),
        "[[providers.models]]\nprovider = \"vllm\"\n",
    )
    .expect("test: unmanaged fragment");

    let config = Config::load_from_path(&path, dir.path().join("workspace")).expect("test: load");
    assert_eq!(config.providers.tool_dialects.len(), 1, "a managed fragment is read");
    assert!(config.providers.models.is_empty(), "an unmanaged fragment is not read");

    config.save().await.expect("test: structured save");
    let main = std::fs::read_to_string(&path).expect("test: read config.toml");
    assert!(main.contains("tool_dialects"), "the table moves to config.toml: {main}");
    let cost = std::fs::read_to_string(fragments.join("cost.toml")).unwrap_or_default();
    assert!(
        !cost.contains("tool_dialects"),
        "the fragment no longer declares it: {cost}"
    );
    let reloaded = Config::load_from_path(&path, dir.path().join("workspace")).expect("test: reload");
    assert_eq!(reloaded.providers.tool_dialects.len(), 1);
}
