#![cfg(test)]

use super::*;
use tokio::test;

#[test]
async fn invalid_cost_tariff_is_rejected_without_disclosing_key() {
    for (field, invalid) in [
        ("input", -1.0),
        ("output", f64::NAN),
        ("cache_write", f64::INFINITY),
        ("cache_read", -0.5),
    ] {
        let mut config = Config::default();
        let mut price = ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        };
        match field {
            "input" => price.input = invalid,
            "output" => price.output = invalid,
            "cache_write" => price.cache_write = invalid,
            _ => price.cache_read = invalid,
        }
        config.cost.prices.insert(
            "custom:http://prx_user:placeholder@example.com/v1/model".to_string(),
            price,
        );
        let error = config
            .validate()
            .expect_err("invalid tariff must fail validation")
            .to_string();
        assert!(error.contains(field), "{error}");
        assert!(!error.contains("placeholder"), "{error}");
    }
}

// ── Defaults ─────────────────────────────────────────────

#[test]
async fn config_default_has_sane_values() {
    let c = Config::default();
    assert_eq!(c.default_provider.as_deref(), Some("openrouter"));
    assert!(c.default_model.as_deref().unwrap().contains("claude"));
    assert!((c.default_temperature - 0.7).abs() < f64::EPSILON);
    assert!(c.api_key.is_none());
    assert!(c.workspace_dir.to_string_lossy().contains("workspace"));
    assert!(c.config_path.to_string_lossy().contains("config.toml"));
}

/// PMM-H: a route, an embedding route or an agent names either an inline
/// `api_key` or a `credential_ref` (`cred-<name>`), never both, and a
/// `credential_ref` must be a reference id.
#[test]
async fn a_selection_names_an_inline_key_or_a_credential_reference() {
    let mut config = Config::default();
    config.model_routes.push(ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".into(),
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
        api_key: None,
        credential_ref: Some("cred-route-key".into()),
    });
    config.validate().expect("a reference alone is valid");

    if let Some(route) = config.model_routes.first_mut() {
        route.api_key = Some("inline-sentinel".into());
    }
    let error = config.validate().unwrap_err().to_string();
    assert!(
        error.contains("model_routes[0] sets both api_key and credential_ref"),
        "{error}"
    );
    assert!(!error.contains("inline-sentinel"), "{error}");

    if let Some(route) = config.model_routes.first_mut() {
        route.api_key = None;
        route.credential_ref = Some("openai:default".into());
    }
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("model_routes[0].credential_ref"), "{error}");
}

#[test]
async fn config_validates_model_tool_allowlists() {
    let mut config = Config::default();
    config
        .tool_tiering
        .model_allowlists
        .insert("gemma4-e2b".into(), vec!["shell".into(), "file_read".into()]);
    config.validate().expect("a named model and non-empty tools are valid");

    config
        .tool_tiering
        .model_allowlists
        .insert(" ".into(), vec!["shell".into()]);
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("model key must not be empty"), "{error}");

    config.tool_tiering.model_allowlists.remove(" ");
    config
        .tool_tiering
        .model_allowlists
        .insert("broken-model".into(), vec!["".into()]);
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("broken-model"), "{error}");
}

#[test]
async fn config_requires_delegate_wildcard_to_be_exclusive() {
    let mut config = Config::default();
    config.agents.insert(
        "worker".into(),
        agentic_delegate_config(vec!["*".into(), "shell".into()]),
    );

    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("agents.worker.allowed_tools must use '*' exclusively"));

    config
        .agents
        .insert("worker".into(), agentic_delegate_config(vec!["*".into()]));
    config.validate().unwrap();
}

#[test]
async fn config_schema_export_contains_expected_contract_shape() {
    let schema = schemars::schema_for!(Config);
    let schema_json = serde_json::to_value(&schema).expect("schema should serialize to json");

    assert_eq!(
        schema_json.get("$schema").and_then(serde_json::Value::as_str),
        Some("https://json-schema.org/draft/2020-12/schema")
    );

    let properties = schema_json
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .expect("schema should expose top-level properties");

    assert!(properties.contains_key("default_provider"));
    assert!(properties.contains_key("skills"));
    assert!(properties.contains_key("gateway"));
    assert!(properties.contains_key("channels_config"));
    assert!(!properties.contains_key("workspace_dir"));
    assert!(!properties.contains_key("config_path"));

    assert!(
        schema_json
            .get("$defs")
            .and_then(serde_json::Value::as_object)
            .is_some(),
        "schema should include reusable type definitions"
    );
}

#[test]
async fn observability_config_default() {
    let o = ObservabilityConfig::default();
    assert_eq!(o.backend, "none");
}

#[test]
async fn runtime_config_default() {
    let r = RuntimeConfig::default();
    assert_eq!(r.kind, "native");
    assert_eq!(r.docker.image, "alpine:3.20");
    assert_eq!(r.docker.network, "none");
    assert_eq!(r.docker.memory_limit_mb, Some(512));
    assert_eq!(r.docker.cpu_limit, Some(1.0));
    assert!(r.docker.read_only_rootfs);
    assert!(r.docker.mount_workspace);
}

#[test]
async fn heartbeat_config_default() {
    let h = HeartbeatConfig::default();
    assert_eq!(h.interval_minutes, 30);
}

#[test]
async fn cron_config_default() {
    let c = CronConfig::default();
    assert_eq!(c.max_run_history, 50);
}

#[test]
async fn cron_config_serde_roundtrip() {
    let c = CronConfig { max_run_history: 100 };
    let json = serde_json::to_string(&c).unwrap();
    let parsed: CronConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.max_run_history, 100);
}

#[test]
async fn config_defaults_cron_when_section_missing() {
    let toml_str = r#"
workspace_dir = "/tmp/workspace"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;

    let parsed: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(parsed.cron.max_run_history, 50);
}

#[test]
async fn nodes_config_defaults() {
    let nodes = NodesConfig::default();
    assert_eq!(nodes.request_timeout_ms, 15_000);
    assert_eq!(nodes.retry_max, 2);
    assert_eq!(nodes.server.listen_addr, "127.0.0.1:8787");
    assert_eq!(nodes.server.max_concurrent_tasks, 8);
    assert_eq!(nodes.server.task_result_ttl_ms, 3_600_000);
}

#[test]
async fn chat_config_defaults_to_empty_fullscreen_config() {
    let config = ChatConfig::default();

    assert_eq!(config.max_concurrent_visible_turns, 2);
}

#[test]
async fn scheduler_claim_lease_defaults_and_validates() {
    let scheduler = SchedulerConfig::default();
    assert_eq!(scheduler.claim_lease_secs, 90);

    let mut config = Config::default();
    config.scheduler.claim_lease_secs = 2;
    assert!(config.validate().is_err());
    config.scheduler.claim_lease_secs = 3;
    assert!(config.validate().is_ok());
}

#[test]
async fn interaction_notice_defaults_required() {
    let config = Config::default();

    assert_eq!(
        config.compliance.interaction_notice.applicability,
        InteractionNoticeApplicability::Required
    );
    assert!(config.validate().is_ok());
}

#[test]
async fn interaction_notice_not_applicable_requires_reviewed_exception() {
    let mut config = Config::default();
    config.compliance.interaction_notice.applicability = InteractionNoticeApplicability::NotApplicable;
    assert!(config.validate().is_err());

    config.compliance.interaction_notice.exception_owner = Some("compliance@example.invalid".to_string());
    assert!(config.validate().is_err());

    config.compliance.interaction_notice.exception_reviewed_at = Some("2026-Jul-18".to_string());
    assert!(config.validate().is_ok());
}

#[test]
async fn chat_config_roundtrips_without_renderer_mode() {
    let config = ChatConfig {
        max_concurrent_visible_turns: 3,
        daemon: ChatDaemonConfig::default(),
    };

    let encoded = toml::to_string(&config).unwrap();
    assert!(
        !encoded.contains("tui_mode"),
        "renderer selection is no longer a persisted config field: {encoded}"
    );
    let decoded: ChatConfig = toml::from_str(&encoded).unwrap();
    assert_eq!(decoded.max_concurrent_visible_turns, 3);

    let defaulted: ChatConfig = toml::from_str("").unwrap();
    assert_eq!(defaulted.max_concurrent_visible_turns, 2);
}

// ── Serde round-trip ─────────────────────────────────────

#[test]
async fn config_toml_roundtrip() {
    let config = Config {
        workspace_dir: PathBuf::from("/tmp/test/workspace"),
        config_path: PathBuf::from("/tmp/test/config.toml"),
        api_key: Some("sk-test-key".into()),
        api_url: None,
        default_provider: Some("openrouter".into()),
        default_model: Some("gpt-4o".into()),
        default_temperature: 0.5,
        model_settings: Default::default(),
        providers: ProvidersConfig::default(),
        observability: ObservabilityConfig {
            backend: "log".into(),
            ..ObservabilityConfig::default()
        },
        autonomy: AutonomyConfig {
            level: AutonomyLevel::Full,
            workspace_only: false,
            forbidden_paths: vec!["/secret".into()],
            max_cost_per_day_cents: 1000,
            scopes: ScopeConfig::default(),
            ..AutonomyConfig::default()
        },
        runtime: RuntimeConfig {
            kind: "docker".into(),
            ..RuntimeConfig::default()
        },
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
        heartbeat: HeartbeatConfig {
            interval_minutes: 15,
            ..HeartbeatConfig::default()
        },
        cron: CronConfig::default(),
        xin: crate::xin::XinConfig::default(),
        channels_config: ChannelsConfig {
            cli: true,
            telegram: Some(TelegramConfig {
                bot_token: "123:ABC".into(),
                allowed_users: vec!["user1".into()],
                stream_mode: StreamMode::default(),
                draft_update_interval_ms: default_draft_update_interval_ms(),
                interrupt_on_new_message: false,
                mention_only: false,
                group_reply_mode: None,
            }),
            discord: None,
            slack: None,
            mattermost: None,
            webhook: None,
            imessage: None,
            matrix: None,
            signal: None,
            whatsapp: None,
            wacli: None,
            linq: None,
            nextcloud_talk: None,
            email: None,
            irc: None,
            lark: None,
            dingtalk: None,
            qq: None,
        },
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

    let toml_str = toml::to_string_pretty(&config).unwrap();
    let parsed: Config = toml::from_str(&toml_str).unwrap();

    assert_eq!(parsed.api_key, config.api_key);
    assert_eq!(parsed.default_provider, config.default_provider);
    assert_eq!(parsed.default_model, config.default_model);
    assert!((parsed.default_temperature - config.default_temperature).abs() < f64::EPSILON);
    assert_eq!(parsed.observability.backend, "log");
    assert_eq!(parsed.autonomy.level, AutonomyLevel::Full);
    assert!(!parsed.autonomy.workspace_only);
    assert_eq!(parsed.runtime.kind, "docker");
    assert_eq!(parsed.heartbeat.interval_minutes, 15);
    assert!(parsed.channels_config.telegram.is_some());
    assert_eq!(parsed.channels_config.telegram.unwrap().bot_token, "123:ABC");
}

/// D-044: the evolution judge call is bounded; the bound is validated so
/// it can be neither zero (every verdict deferred) nor effectively
/// unbounded.
#[test]
async fn evolution_judge_timeout_is_validated() {
    let with_timeout = |secs: u64| {
        let raw = format!(
            "workspace_dir = \"/tmp/ws\"\nconfig_path = \"/tmp/config.toml\"\ndefault_temperature = 0.7\n\
                 [self_system]\nevolution_judge_timeout_secs = {secs}\n"
        );
        toml::from_str::<Config>(&raw).expect("test: config parses")
    };
    for secs in [0, 3601] {
        let error = with_timeout(secs)
            .validate()
            .expect_err("an out-of-range judge timeout is a configuration error");
        assert!(
            format!("{error:#}").contains("self_system.evolution_judge_timeout_secs"),
            "{secs}: {error:#}"
        );
    }
    for secs in [1, 180, 3600] {
        with_timeout(secs).validate().expect("test: an in-range timeout loads");
    }
}

#[test]
async fn config_minimal_toml_uses_defaults() {
    let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
    let parsed: Config = toml::from_str(minimal).unwrap();
    assert!(parsed.api_key.is_none());
    assert!(parsed.default_provider.is_none());
    assert_eq!(parsed.observability.backend, "none");
    assert_eq!(parsed.autonomy.level, AutonomyLevel::Full);
    assert_eq!(parsed.runtime.kind, "native");
    assert!(parsed.channels_config.cli);
    assert!(!parsed.memory.acl_enabled);
    assert_eq!(parsed.memory.archive_after_days, 7);
    assert_eq!(parsed.memory.purge_after_days, 30);
    assert_eq!(parsed.memory.conversation_retention_days, 3);
    assert_eq!(parsed.memory.daily_retention_days, 7);
    assert_eq!(parsed.self_system.evolution_interval_hours, 24);
    assert!(parsed.self_system.fitness.auto_run);
    assert_eq!(parsed.self_system.fitness.timezone, "UTC");
    assert_eq!(parsed.self_system.fitness.run_at, "00:10");
    assert_eq!(parsed.self_system.fitness.max_backfill_days, 7);
}

#[test]
async fn config_parses_identity_bindings_and_user_policies() {
    let raw = r#"
default_temperature = 0.7

[[identity_bindings]]
user_id = "ak"
channel = "signal"
channel_account = "d26c8bda-58c5-4eb4-9997-0b011129fd58"

[[user_policies]]
user_id = "ak"
role = "owner"
projects = []
visibility_ceiling = "public"
blocked_patterns = []
"#;

    let parsed: Config = toml::from_str(raw).unwrap();
    assert_eq!(parsed.identity_bindings.len(), 1);
    assert_eq!(parsed.identity_bindings[0].user_id, "ak");
    assert_eq!(parsed.identity_bindings[0].channel, "signal");
    assert_eq!(parsed.user_policies.len(), 1);
    assert_eq!(parsed.user_policies[0].role, "owner");
    assert_eq!(parsed.user_policies[0].visibility_ceiling, "public");
}

#[test]
async fn ollama_provider_num_ctx_deserializes() {
    let raw = r#"
default_temperature = 0.7

[providers.ollama]
num_ctx = 32768
"#;

    let parsed: Config = toml::from_str(raw).unwrap();
    assert_eq!(parsed.providers.ollama.num_ctx, Some(32_768));
}

#[test]
async fn runtime_reasoning_enabled_deserializes() {
    let raw = r#"
default_temperature = 0.7

[runtime]
reasoning_enabled = false
"#;

    let parsed: Config = toml::from_str(raw).unwrap();
    assert_eq!(parsed.runtime.reasoning_enabled, Some(false));
}

#[test]
async fn hosted_latency_warning_does_not_change_the_configured_ceiling() {
    let mut config = Config::default();
    config.causal_tree.backend.kind =
        crate::causal_tree::config::BackendSelection::Fixed(crate::decision::BackendKind::OpenRouter);
    config.causal_tree.backend.model = "typesafe/jev-1.13".to_owned();
    let warning = config
        .decision_latency_budget_warning()
        .expect("test: below catalog recommendation");
    assert!(warning.contains("500") && warning.contains("3000"), "{warning}");
    assert_eq!(config.causal_tree.policy.extra_latency_budget_ms, 500);
    config.causal_tree.policy.extra_latency_budget_ms = 3_000;
    assert!(config.decision_latency_budget_warning().is_none());
    config.causal_tree.policy.extra_latency_budget_ms = 500;
    config.causal_tree.enabled = false;
    assert!(config.decision_latency_budget_warning().is_none());
}

#[test]
async fn pre_upgrade_config_inherits_local_vector_recall_and_says_so() {
    // A configuration written before the default flipped has no
    // `embedding_provider` key at all: upgrading the binary alone turns
    // vector recall on, so the load has to be able to say that out loud.
    let dir = tempfile::TempDir::new().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
default_temperature = 0.7

[memory]
backend = "sqlite"
auto_save = true
"#,
    )
    .unwrap();

    let loaded = Config::load_from_path(&config_path, dir.path().join("workspace")).unwrap();
    assert_eq!(loaded.memory.embedding_provider, "local");
    assert!(!loaded.memory.embedding_provider_explicit);
    let notice = loaded
        .embedding_default_upgrade_notice()
        .expect("an inherited default must be reported");
    assert!(notice.contains("embedding_provider not set"));
    assert!(notice.contains("local vector recall"));
    assert!(notice.contains(r#"memory.embedding_provider = "none""#));
}

#[test]
async fn router_model_config_omitted_max_context_defaults_to_1m() {
    let raw = r#"
model_id = "custom-modern"
provider = "openrouter"
"#;
    let parsed: RouterModelConfig = toml::from_str(raw).unwrap();
    assert_eq!(parsed.max_context, 1_000_000);
}

// ══════════════════════════════════════════════════════════
// SECURITY CHECKLIST TESTS — Gateway config
// ══════════════════════════════════════════════════════════

#[test]
async fn checklist_gateway_default_requires_pairing() {
    let g = GatewayConfig::default();
    assert!(g.require_pairing, "Pairing must be required by default");
}

#[test]
async fn checklist_gateway_default_blocks_public_bind() {
    let g = GatewayConfig::default();
    assert!(!g.allow_public_bind, "Public bind must be blocked by default");
}

#[test]
async fn checklist_gateway_default_no_tokens() {
    let g = GatewayConfig::default();
    assert!(g.paired_tokens.is_empty(), "No pre-paired tokens by default");
    assert_eq!(g.pair_rate_limit_per_minute, 10);
    assert_eq!(g.webhook_rate_limit_per_minute, 60);
    assert_eq!(g.api_rate_limit_per_minute, 60);
    assert!(!g.trust_forwarded_headers);
    assert_eq!(g.rate_limit_max_keys, 10_000);
    assert_eq!(g.idempotency_ttl_secs, 300);
    assert_eq!(g.idempotency_max_keys, 10_000);
    assert_eq!(g.channel_webhook_dedup_ttl_secs, 86_400);
}

#[test]
async fn checklist_gateway_cli_default_host_is_localhost() {
    // The CLI default for --host is 127.0.0.1 (checked in main.rs)
    // Here we verify the config default matches
    let c = Config::default();
    assert!(c.gateway.require_pairing, "Config default must require pairing");
    assert!(!c.gateway.allow_public_bind, "Config default must block public bind");
}

#[test]
async fn checklist_gateway_serde_roundtrip() {
    let g = GatewayConfig {
        port: 16830,
        host: "127.0.0.1".into(),
        require_pairing: true,
        allow_public_bind: false,
        paired_tokens: vec!["zc_test_token".into()],
        pair_rate_limit_per_minute: 12,
        webhook_rate_limit_per_minute: 80,
        api_rate_limit_per_minute: 90,
        trust_forwarded_headers: true,
        rate_limit_max_keys: 2048,
        idempotency_ttl_secs: 600,
        idempotency_max_keys: 4096,
        channel_webhook_dedup_ttl_secs: 7200,
    };
    let toml_str = toml::to_string(&g).unwrap();
    let parsed: GatewayConfig = toml::from_str(&toml_str).unwrap();
    assert!(parsed.require_pairing);
    assert!(!parsed.allow_public_bind);
    assert_eq!(parsed.paired_tokens, vec!["zc_test_token"]);
    assert_eq!(parsed.pair_rate_limit_per_minute, 12);
    assert_eq!(parsed.webhook_rate_limit_per_minute, 80);
    assert_eq!(parsed.api_rate_limit_per_minute, 90);
    assert!(parsed.trust_forwarded_headers);
    assert_eq!(parsed.rate_limit_max_keys, 2048);
    assert_eq!(parsed.idempotency_ttl_secs, 600);
    assert_eq!(parsed.idempotency_max_keys, 4096);
    assert_eq!(parsed.channel_webhook_dedup_ttl_secs, 7200);
}

#[test]
async fn checklist_gateway_backward_compat_no_gateway_section() {
    // Old configs without [gateway] should get secure defaults
    let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
    let parsed: Config = toml::from_str(minimal).unwrap();
    assert!(
        parsed.gateway.require_pairing,
        "Missing [gateway] must default to require_pairing=true"
    );
    assert!(
        !parsed.gateway.allow_public_bind,
        "Missing [gateway] must default to allow_public_bind=false"
    );
}

// ══════════════════════════════════════════════════════════
// COMPOSIO CONFIG TESTS
// ══════════════════════════════════════════════════════════

#[test]
async fn composio_config_default_unconfigured() {
    let c = ComposioConfig::default();
    assert!(!c.configured());
    assert!(c.api_key.is_none(), "No API key by default");
    assert_eq!(c.entity_id, "default");
}

#[test]
async fn composio_config_serde_roundtrip() {
    let c = ComposioConfig {
        api_key: Some("comp-key-123".into()),
        entity_id: "user42".into(),
    };
    let toml_str = toml::to_string(&c).unwrap();
    let parsed: ComposioConfig = toml::from_str(&toml_str).unwrap();
    assert!(parsed.configured());
    assert_eq!(parsed.api_key.as_deref(), Some("comp-key-123"));
    assert_eq!(parsed.entity_id, "user42");
}

#[test]
async fn composio_config_backward_compat_missing_section() {
    let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
    let parsed: Config = toml::from_str(minimal).unwrap();
    assert!(!parsed.composio.configured());
    assert!(parsed.composio.api_key.is_none());
}

#[test]
async fn auth_config_defaults_enable_codex_import() {
    let auth = AuthConfig::default();
    assert!(auth.codex_auth_json_auto_import);
    assert!(!auth.codex_auth_json_path.as_os_str().is_empty());
}

#[test]
async fn auth_config_serde_roundtrip() {
    let auth = AuthConfig {
        codex_auth_json_auto_import: false,
        codex_auth_json_path: PathBuf::from("/tmp/custom-auth.json"),
    };
    let toml_str = toml::to_string(&auth).unwrap();
    let parsed: AuthConfig = toml::from_str(&toml_str).unwrap();
    assert!(!parsed.codex_auth_json_auto_import);
    assert_eq!(parsed.codex_auth_json_path, PathBuf::from("/tmp/custom-auth.json"));
}

#[test]
async fn browser_config_default_minimal() {
    let b = BrowserConfig::default();
    assert!(b.allowed_domains.is_empty(), "expected an empty value");
    assert!(b.computer_use.api_key.is_none());
}

#[test]
async fn browser_config_serde_roundtrip() {
    let b = BrowserConfig {
        allowed_domains: vec!["example.com".into(), "docs.example.com".into()],
        computer_use: BrowserComputerUseConfig {
            api_key: Some("test-token".into()),
        },
    };
    let toml_str = toml::to_string(&b).unwrap();
    let parsed: BrowserConfig = toml::from_str(&toml_str).unwrap();
    assert_eq!(parsed.allowed_domains.len(), 2);
    assert_eq!(parsed.allowed_domains[0], "example.com");
    assert_eq!(parsed.computer_use.api_key.as_deref(), Some("test-token"));
}

#[test]
async fn browser_config_backward_compat_missing_section() {
    let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
    let parsed: Config = toml::from_str(minimal).unwrap();
    assert!(parsed.browser.allowed_domains.is_empty(), "expected an empty value");
}

/// D-101 F-5 ruling: a read-only command on a machine with no
/// configuration sees the built-in defaults and writes nothing: no file
/// and no directory under the (temporary) home.
#[test]
async fn read_only_defaults_without_a_config_create_nothing() {
    let _env_guard = env_override_lock().await;
    let temp_home = tempfile::TempDir::new().expect("test: temp home");
    let original_home = std::env::var("HOME").ok();
    let original_workspace = std::env::var("OPENPRX_WORKSPACE").ok();
    let original_config_dir = std::env::var("OPENPRX_CONFIG_DIR").ok();
    test_set_env("HOME", temp_home.path());
    test_remove_env("OPENPRX_WORKSPACE");
    test_remove_env("OPENPRX_CONFIG_DIR");

    let config = Config::load_existing_or_defaults_read_only(None).await;

    let created = std::fs::read_dir(temp_home.path())
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(home) = original_home {
        test_set_env("HOME", home);
    } else {
        test_remove_env("HOME");
    }
    if let Some(workspace) = original_workspace {
        test_set_env("OPENPRX_WORKSPACE", workspace);
    }
    if let Some(config_dir) = original_config_dir {
        test_set_env("OPENPRX_CONFIG_DIR", config_dir);
    }
    let config = config.expect("test: defaults without a config");
    assert!(
        config.config_path.starts_with(temp_home.path()),
        "{}",
        config.config_path.display()
    );
    assert!(!config.config_path.exists());
    assert!(created.is_empty(), "a read-only load created {created:?}");
}

#[test]
async fn test_router_config_rejects_invalid_threshold() {
    let mut config = RouterConfig::default();
    config.automix.confidence_threshold = 1.5;
    config.automix.premium_model_id = "openai/model-premium".to_string();
    assert!(config.validate().is_err());
}

#[test]
async fn test_router_config_rejects_negative_weight() {
    let mut config = RouterConfig::default();
    config.alpha = -0.1;
    assert!(config.validate().is_err());
}

#[test]
async fn test_router_config_allows_empty_premium_model_without_automix() {
    let mut config = RouterConfig::default();
    config.automix.premium_model_id = String::new();
    assert!(config.validate().is_ok());
}

#[test]
async fn gateway_config_default_values() {
    let g = GatewayConfig::default();
    assert_eq!(g.port, 16830);
    assert_eq!(g.host, "127.0.0.1");
    assert!(g.require_pairing);
    assert!(!g.allow_public_bind);
    assert!(g.paired_tokens.is_empty(), "expected an empty value");
    assert!(!g.trust_forwarded_headers);
    assert_eq!(g.rate_limit_max_keys, 10_000);
    assert_eq!(g.idempotency_max_keys, 10_000);
}

#[test]
async fn heartbeat_defaults_include_active_hours_and_prompt() {
    let hb = HeartbeatConfig::default();
    assert_eq!(hb.active_hours, vec![8, 23]);
    assert_eq!(hb.prompt, "Check HEARTBEAT.md and follow instructions.");
}

// ── Config file permission hardening (Unix only) ───────────────

#[cfg(unix)]
#[test]
async fn new_config_file_has_restricted_permissions() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");

    // Create a config and save it
    let mut config = Config::default();
    config.config_path = config_path.clone();
    config.save().await.unwrap();

    let meta = fs::metadata(&config_path).await.unwrap();
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "New config file should be owner-only (0600), got {mode:o}");
}

#[cfg(unix)]
#[test]
async fn world_readable_config_is_detectable() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");

    // Create a config file with intentionally loose permissions
    std::fs::write(&config_path, "# test config").unwrap();
    std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let meta = std::fs::metadata(&config_path).unwrap();
    let mode = meta.permissions().mode();
    assert!(
        mode & 0o004 != 0,
        "Test setup: file should be world-readable (mode {mode:o})"
    );
}

#[test]
async fn config_validate_rejects_empty_sqlite_read_pool() {
    let mut config = Config::default();
    assert_eq!(
        config.memory.sqlite_read_pool_size, None,
        "unset means the size is derived from the available CPUs"
    );
    config.memory.sqlite_read_pool_size = Some(0);
    let error = config
        .validate()
        .expect_err("a reader pool with no connections must be rejected");
    assert!(error.to_string().contains("memory.sqlite_read_pool_size"));
}

#[test]
async fn idle_hang_thresholds_validate_and_stay_distinct_from_the_long_task_warning() {
    let defaults = Config::default();
    assert_eq!(defaults.runtime.idle_hang_secs, None, "unset means the 1800s default");
    defaults.validate().expect("defaults must validate");

    // 0 disables idle detection.
    let mut disabled = Config::default();
    disabled.runtime.idle_hang_secs = Some(0);
    disabled.validate().expect("0 is the documented disable switch");

    // Too short to tell a wedged turn from a quiet tool call.
    let mut too_short = Config::default();
    too_short.runtime.idle_hang_secs = Some(5);
    let error = too_short
        .validate()
        .expect_err("an idle window this short would kill healthy work");
    assert!(error.to_string().contains("runtime.idle_hang_secs"), "{error}");

    // The two mechanisms are configured independently, and the shipped
    // defaults keep the warning strictly ahead of the termination.
    assert!(
        crate::agent::idle::DEFAULT_IDLE_HANG_SECS > crate::runtime::registry::DEFAULT_LONG_TASK_WARN_SECS,
        "an operator must always be warned before anything is terminated"
    );
    let mut warn_only = Config::default();
    warn_only.runtime.long_task_warn_secs = Some(0);
    warn_only.runtime.idle_hang_secs = Some(120);
    warn_only
        .validate()
        .expect("silencing the warning must not affect hang detection");
}

#[test]
async fn smart_group_config_defaults_and_validation() {
    let cfg = SmartGroupConfig::default();
    assert!(cfg.classifier_provider.is_empty(), "expected an empty value");
    assert!(cfg.classifier_model.is_empty(), "expected an empty value");
    assert_eq!(cfg.classifier_timeout_secs, 5);
    assert_eq!(cfg.classifier_max_context_messages, 3);
    cfg.validate().expect("default smart_group config is valid");

    // Out-of-range temperature and zero timeout are rejected.
    let mut bad = SmartGroupConfig::default();
    bad.classifier_temperature = 3.0;
    assert!(bad.validate().is_err());
    let mut bad_timeout = SmartGroupConfig::default();
    bad_timeout.classifier_timeout_secs = 0;
    assert!(bad_timeout.validate().is_err());
}

#[test]
async fn smart_group_config_parses_from_toml() {
    let toml = r#"
default_temperature = 0.7

[smart_group]
classifier_provider = "ollama"
classifier_model = "qwen2.5:3b"
classifier_timeout_secs = 8
"#;
    let cfg: Config = toml::from_str(toml).expect("parse smart_group toml");
    assert_eq!(cfg.smart_group.classifier_provider, "ollama");
    assert_eq!(cfg.smart_group.classifier_model, "qwen2.5:3b");
    assert_eq!(cfg.smart_group.classifier_timeout_secs, 8);
}

/// INT-2B a37 (R-C2Kf S-3), RED before: the unused-dialect warning was
/// emitted on every `validate()`, so a configuration validated twice
/// warned twice. After: once per distinct entry set, and the text says
/// the warning can be ignored when the provider is given on the command
/// line.
#[test]
async fn an_unused_tool_dialect_entry_is_warned_about_once() {
    let _serial = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.lock().await;
    let mut config = Config {
        default_provider: Some("custom:http://127.0.0.1:1/v1".to_string()),
        ..Config::default()
    };
    // A unique endpoint, so no other test has already seen this entry set.
    config.providers.tool_dialects.push(ProviderToolDialectConfig {
        provider: format!("custom:http://127.0.0.1:9/a37-{}", uuid::Uuid::new_v4().simple()),
        model: None,
        tool_schema: Some(ToolSchemaDialectSetting::NoRootCombinators),
        tool_call: None,
    });
    let (outcome, logs) = capture_logs(|| config.validate().and_then(|()| config.validate()));
    outcome.expect("test: an unused entry is a warning, not an error");
    let warnings = logs.matches("nothing in this configuration uses").count();
    assert_eq!(warnings, 1, "one warning per entry set: {logs}");
    assert!(logs.contains("command line"), "{logs}");
}
