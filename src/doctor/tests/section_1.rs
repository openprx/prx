
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn doctor_names_subscription_billing_without_an_unpriced_refusal() {
        let dir = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = dir.path().join("workspace");
        config.config_path = dir.path().join("config.toml");
        config.default_provider = Some("kimi-code".into());
        config.default_model = Some("kimi-for-coding".into());
        let mut items = Vec::new();
        check_model_selection(&config, &mut items);
        assert!(items.iter().any(|item| {
            item.severity == Severity::Ok && item.message == "subscription-billed, no per-token cost recorded"
        }));
        assert!(
            items
                .iter()
                .all(|item| !item.message.contains("G6 budget admission refuses"))
        );

        config.default_provider = Some("openrouter".into());
        config.default_model = Some("deepseek/deepseek-v3.2".into());
        items.clear();
        check_model_selection(&config, &mut items);
        assert!(items.iter().any(|item| {
            item.severity == Severity::Warn && item.message.contains(r#"[cost.prices."deepseek/deepseek-v3.2"]"#)
        }));
    }

    #[test]
    fn doctor_warns_when_a_cost_price_key_matches_no_configured_model() {
        let mut config = Config::default();
        config.default_provider = Some("custom:http://127.0.0.1:9/v1".into());
        config.default_model = Some("moonshotai/kimi-k2.5".into());
        config.cost.prices.insert(
            "custom:http://127.0.0.1:9/v1/moonshotai/kimi-k2.5".into(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 2.0,
                cache_write: 1.0,
                cache_read: 0.5,
            },
        );
        let mut items = Vec::new();
        check_model_selection(&config, &mut items);
        assert!(items.iter().any(|item| {
            item.severity == Severity::Warn
                && item.category == "cost"
                && item.message.contains("expected key is \"moonshotai/kimi-k2.5\"")
        }));

        config.cost.prices.clear();
        config.cost.prices.insert(
            "moonshotai/kimi-k2.5".into(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 2.0,
                cache_write: 1.0,
                cache_read: 0.5,
            },
        );
        items.clear();
        check_model_selection(&config, &mut items);
        assert!(items.iter().all(|item| item.category != "cost"));
    }

    #[test]
    fn doctor_reports_invalid_saved_model_settings_as_repairable_warnings() {
        let mut config = Config::default();
        config.default_provider = Some("openai".into());
        config.default_model = Some("gpt-5.6".into());
        config.model_settings.insert(
            "openai/gpt-5.6".into(),
            serde_json::from_value(serde_json::json!({"level":"future"})).unwrap(),
        );
        let mut items = Vec::new();
        check_model_selection(&config, &mut items);
        assert!(items.iter().any(|item| item.message.contains("saved level")));
    }

    #[test]
    fn absent_catalog_lifecycle_is_informational_without_claiming_model_health() {
        let config = Config {
            default_provider: Some("openai".into()),
            default_model: Some("prx-unlisted-model".into()),
            ..Default::default()
        };
        let mut items = Vec::new();
        check_model_selection(&config, &mut items);
        let note = items
            .iter()
            .find(|item| item.message.contains("deleted or custom"))
            .unwrap();
        assert_eq!(note.severity, Severity::Ok);
        assert_eq!(note.state, DiagnosticState::Unknown);
    }

    #[cfg(feature = "llm-router")]
    #[test]
    fn doctor_surfaces_an_empty_router_registry_beyond_the_startup_log() {
        let mut config = Config::default();
        config.default_provider = Some("openai".to_owned());
        config.default_model = Some("gpt-5.6".to_owned());
        let mut items = Vec::new();
        check_router_candidates(&config, &mut items);
        assert_eq!(items.len(), 1);
        assert!(
            items
                .iter()
                .any(|item| item.message.contains("no reachable candidates"))
        );
        assert_eq!(config.default_model.as_deref(), Some("gpt-5.6"));

        config.default_provider = Some("openrouter".to_owned());
        config.default_model = Some("anthropic/claude-sonnet-5".to_owned());
        items.clear();
        check_router_candidates(&config, &mut items);
        assert!(
            items.is_empty(),
            "the published OpenRouter candidate must actually be reachable"
        );
    }

    #[cfg(feature = "llm-router")]
    #[test]
    fn doctor_requires_a_reachable_model_in_a_non_default_provider_route() {
        let mut config = Config {
            default_provider: Some("openai".into()),
            default_model: Some("gpt-5.6".into()),
            ..Config::default()
        };
        config.model_routes.push(crate::config::ModelRouteConfig {
            hint: "quick".into(),
            provider: "openrouter".into(),
            model: "prx-absent-router-model".into(),
            api_key: None,
            credential_ref: None,
            settings: Default::default(),
        });
        let mut items = Vec::new();
        check_router_candidates(&config, &mut items);
        assert_eq!(items.len(), 1);
        assert!(
            items
                .iter()
                .any(|item| item.message.contains("no reachable candidates"))
        );
        config.model_routes.first_mut().unwrap().model = "anthropic/claude-sonnet-5".into();
        items.clear();
        check_router_candidates(&config, &mut items);
        assert!(items.is_empty());
    }

    /// D-094 C-1, C-5: the doctor names a refused decision backend (which
    /// runs builtin-only) and every decision credential nothing references,
    /// by id, never by value.
    #[test]
    fn the_doctor_reports_a_refused_decision_backend_and_orphaned_credentials() {
        let temp = TempDir::new().unwrap();
        let mut config = Config {
            config_path: temp.path().join("config.toml"),
            workspace_dir: temp.path().join("workspace"),
            ..Config::default()
        };
        config.decision_engine.enabled = true;
        config.decision_engine.backend.get_or_insert_with(Default::default).kind =
            crate::config::schema::decision_engine::BackendSelection::Fixed(
                crate::config::schema::decision_engine::BackendKind::CompatibleHttp,
            );
        config
            .decision_engine
            .backend
            .get_or_insert_with(Default::default)
            .endpoint = "http://127.0.0.1:8900/v1".to_string();
        config
            .decision_engine
            .backend
            .get_or_insert_with(Default::default)
            .model = "undeclared-model".to_string();
        let orphan = crate::config::manage::credentials::CredentialReferences::for_config(&config)
            .unwrap()
            .set(
                "custom:http://127.0.0.1:8900/v1",
                "decision-compatible-http-0badc0de",
                "sk-DOCTOR-ORPHAN-SENTINEL-01",
            )
            .unwrap();
        let mut items = Vec::new();
        check_decision_plane(&config, &mut items);
        let text = items
            .iter()
            .map(|item| item.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(items.len(), 2, "{text}");
        assert!(text.contains("builtin-only") && text.contains(&orphan.id), "{text}");
        assert!(text.contains("prx providers credential remove <id>"), "{text}");
        // Disabled, the credential store is not opened: a held lock does not
        // delay the doctor, and nothing is reported from the store.
        config.decision_engine.enabled = false;
        std::fs::write(temp.path().join("auth-profiles.lock"), "pid=1\n").unwrap();
        let started = std::time::Instant::now();
        let mut items = Vec::new();
        check_decision_plane(&config, &mut items);
        assert!(
            items.is_empty(),
            "{:?}",
            items.iter().map(|item| &item.message).collect::<Vec<_>>()
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(!text.contains("SENTINEL"), "{text}");
    }

    #[test]
    fn memory_diagnostics_flag_vector_recall_inherited_from_the_flipped_default() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        config.memory.embedding_provider = "local".to_string();
        config.memory.embedding_provider_explicit = false;

        let mut items = Vec::new();
        check_memory_diagnostics(&config, &mut items);
        let notice = items
            .iter()
            .find(|item| item.message.contains("embedding_provider not set"))
            .expect("doctor must report an inherited local vector recall default");
        assert_eq!(notice.severity, Severity::Warn);
        assert_eq!(notice.category, "embedding");

        config.memory.embedding_provider_explicit = true;
        let mut items = Vec::new();
        check_memory_diagnostics(&config, &mut items);
        assert!(
            !items
                .iter()
                .any(|item| item.message.contains("embedding_provider not set")),
            "an explicit provider must not be reported as an inherited default"
        );
    }

    /// D-088 C-7: `prx doctor` names each blocked workspace MCP server with
    /// the one-line opt-in, instead of leaving it to a log line.
    #[test]
    fn doctor_reports_a_workspace_mcp_server_blocked_by_the_opt_in() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.autonomy.level = crate::security::AutonomyLevel::Supervised;
        config.workspace_dir = temp.path().to_path_buf();
        std::fs::write(
            temp.path().join("mcp.json"),
            r#"{"mcpServers": {"legacy": {"command": "npx", "args": ["some-mcp"]}}}"#,
        )
        .unwrap();
        let mut items = Vec::new();
        check_workspace_mcp_servers(&config, &mut items);
        let blocked = items
            .iter()
            .find(|item| {
                item.message
                    .contains("workspace server 'legacy' (stdio) is not started")
            })
            .expect("doctor must report the blocked workspace server");
        assert_eq!(blocked.severity, Severity::Warn);
        assert!(
            blocked.message.contains("workspace_servers = \"stdio\""),
            "{}",
            blocked.message
        );

        std::fs::remove_file(temp.path().join("mcp.json")).unwrap();
        let mut items = Vec::new();
        check_workspace_mcp_servers(&config, &mut items);
        assert!(items.iter().all(|item| item.severity != Severity::Warn));
    }

    /// D-095 N-4: an operator `working_dir` inside the workspace is named.
    #[test]
    fn doctor_warns_about_a_working_dir_inside_the_workspace() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        config.mcp.servers.insert(
            "inside".into(),
            crate::config::McpServerConfig {
                command: Some("npx".into()),
                working_dir: Some(temp.path().join("tools").display().to_string()),
                ..crate::config::McpServerConfig::default()
            },
        );
        let mut items = Vec::new();
        check_workspace_mcp_servers(&config, &mut items);
        assert!(
            items
                .iter()
                .any(|item| item.severity == Severity::Warn && item.message.contains("inside the agent's workspace")),
            "{:?}",
            items.iter().map(|item| &item.message).collect::<Vec<_>>()
        );
    }

    /// RED before PMME-h (D-095 L-3): a `pending` configuration record was
    /// listed by `prx config status` but not by `prx doctor`.
    #[test]
    fn doctor_reports_an_unfinished_configuration_change() {
        let temp = TempDir::new().unwrap();
        let config = Config {
            config_path: temp.path().join("config.toml"),
            workspace_dir: temp.path().join("workspace"),
            ..Config::default()
        };
        let record_dir = temp
            .path()
            .join(".config-history")
            .join("0190a000-0000-7000-8000-000000000001");
        std::fs::create_dir_all(&record_dir).unwrap();
        let summary = crate::config::manage::ConfigChangeSummary::between(Some(&config), &config).unwrap();
        let now = chrono::Utc::now();
        let record = crate::config::manage::HistoryRecord {
            version: 1,
            id: "0190a000-0000-7000-8000-000000000001".into(),
            created_at: now,
            updated_at: now,
            actor: crate::config::manage::ConfigActor::Cli,
            status: crate::config::manage::RecordStatus::Pending,
            base_revision: None,
            committed_revision: None,
            restores: None,
            summary,
            active_generation: None,
            restart_required: Vec::new(),
            reason: None,
        };
        std::fs::write(record_dir.join("record.json"), serde_json::to_vec(&record).unwrap()).unwrap();

        let mut items = Vec::new();
        check_unfinished_config_changes(&config, &mut items);
        let pending = items
            .iter()
            .find(|item| item.message.contains("0190a000-0000-7000-8000-000000000001"))
            .unwrap_or_else(|| {
                panic!(
                    "doctor must report the pending record: {:?}",
                    items.iter().map(|item| &item.message).collect::<Vec<_>>()
                )
            });
        assert_eq!(pending.severity, Severity::Warn);
        assert!(pending.message.contains("prx config rollback"), "{}", pending.message);
    }

    /// SP-PMM H-4: a `pending` history record is reported with its
    /// recovery commands and never applied (loading the configuration and
    /// running the checks leave the files and the record as they were); a
    /// persisted selection the evidence does not verify is listed by name.
    #[tokio::test]
    async fn a_pending_record_is_reported_never_applied_and_unverified_selections_are_listed() {
        let temp = TempDir::new().unwrap();
        let mut config = Config {
            config_path: temp.path().join("config.toml"),
            workspace_dir: temp.path().join("workspace"),
            default_provider: Some("openai".into()),
            default_model: Some("gpt-4o".into()),
            ..Config::default()
        };
        config.model_routes.push(crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "fast".into(),
            provider: "openai".into(),
            model: "pmmh-unlisted-route-model".into(),
            api_key: None,
            credential_ref: None,
        });
        config.save().await.unwrap();
        let history = crate::config::manage::ConfigHistory::for_config(&config.config_path).unwrap();
        let mut other = config.clone();
        other.default_temperature = 0.1;
        let before = crate::config::files::current_before_image(&config.config_path).unwrap();
        let record = history
            .create_pending(
                crate::config::manage::history::NewRecord {
                    actor: crate::config::manage::ConfigActor::Cli,
                    base_revision: None,
                    restores: None,
                    summary: crate::config::manage::ConfigChangeSummary::between(Some(&config), &other).unwrap(),
                },
                &before,
            )
            .unwrap();
        let config_bytes = std::fs::read(&config.config_path).unwrap();
        let record_bytes = std::fs::read(history.dir().join(&record.id).join("record.json")).unwrap();

        let loaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).unwrap();
        let mut items = Vec::new();
        check_unfinished_config_changes(&loaded, &mut items);
        check_persisted_selections(&loaded, &mut items);

        let pending = items
            .iter()
            .find(|item| item.message.contains(&record.id))
            .expect("the pending record is reported");
        assert!(
            pending.message.contains("is pending") && pending.message.contains("never applied"),
            "{}",
            pending.message
        );
        let unverified = items
            .iter()
            .find(|item| item.message.contains("model route `hint:fast`"))
            .expect("the unverified route is reported");
        assert_eq!(unverified.severity, Severity::Warn);
        assert!(
            unverified.message.contains("pmmh-unlisted-route-model"),
            "{}",
            unverified.message
        );
        assert_eq!(
            std::fs::read(&config.config_path).unwrap(),
            config_bytes,
            "a pending record was applied"
        );
        assert_eq!(
            std::fs::read(history.dir().join(&record.id).join("record.json")).unwrap(),
            record_bytes,
            "the pending record was rewritten"
        );
        assert_eq!(
            history.get(&record.id).unwrap().status,
            crate::config::manage::RecordStatus::Pending
        );
    }

    /// D-104 C-1 ruling: `prx doctor` names two embedding routes sharing a
    /// hint, with the fix.
    #[test]
    fn doctor_reports_duplicate_embedding_route_hints() {
        let route = |model: &str| crate::config::EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "openai".into(),
            model: model.into(),
            dimensions: None,
            api_key: None,
            credential_ref: None,
        };
        let mut config = Config::default();
        config.embedding_routes = vec![route("text-embedding-3-small"), route("text-embedding-3-large")];
        let mut items = Vec::new();
        check_persisted_selections(&config, &mut items);
        let item = items
            .iter()
            .find(|item| item.message.contains("both use hint 'semantic'"))
            .expect("the duplicate is reported");
        assert_eq!(item.severity, Severity::Warn);
        assert!(
            item.message.contains("Give each route its own hint"),
            "{}",
            item.message
        );
    }

    /// HOOKS-ENV: `prx doctor` names blocked workspace hooks with the
    /// one-line opt-in, and says when they run.
    #[test]
    fn doctor_reports_blocked_workspace_hooks_with_the_opt_in() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        config.security.workspace_hooks = crate::config::WorkspaceHooks::Off;
        std::fs::write(
            temp.path().join("hooks.json"),
            r#"{"hooks": {"turn_complete": [{"command": "true"}]}}"#,
        )
        .unwrap();
        let mut items = Vec::new();
        check_workspace_hooks(&config, &mut items);
        let blocked = items
            .iter()
            .find(|item| item.category == "hooks")
            .expect("test: a hooks item");
        assert_eq!(blocked.severity, Severity::Warn);
        assert!(
            blocked.message.contains(crate::hooks::WORKSPACE_HOOKS_OPT_IN),
            "{}",
            blocked.message
        );

        config.security.workspace_hooks = crate::config::WorkspaceHooks::On;
        let mut items = Vec::new();
        check_workspace_hooks(&config, &mut items);
        assert!(items.iter().all(|item| item.severity != Severity::Warn));
    }

    #[test]
    fn deployed_binary_check_accepts_the_runtime_path() {
        let temp = TempDir::new().unwrap();
        let deployed = temp.path().join("prx");
        std::fs::write(&deployed, b"binary").unwrap();
        let mut items = Vec::new();

        check_deployed_binary_paths(&mut items, Some(deployed.clone()), Ok(deployed));

        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|item| item.severity == Severity::Ok));
        assert!(
            items
                .iter()
                .any(|item| item.message == "doctor is running from deployed binary")
        );
    }

    #[test]
    fn deployed_binary_check_reports_missing_path_lookup_without_linux_hardcode() {
        let mut items = Vec::new();

        check_deployed_binary_paths(&mut items, None, Ok(PathBuf::from("/opt/homebrew/bin/prx")));

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].severity, Severity::Error);
        assert_eq!(items[0].message, "deployed prx binary not found on PATH");
        assert!(items.iter().all(|item| !item.message.contains("/home/ck")));
    }

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        #[allow(unsafe_code)]
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let original = std::env::var(key).ok();
            if key == "HOME" {
                crate::config::schema::follow_test_home_env(value.map(std::ffi::OsStr::new));
            }
            // SAFETY: test-only env manipulation; tests that use this must be serialized.
            unsafe {
                match value {
                    Some(next) => std::env::set_var(key, next),
                    None => std::env::remove_var(key),
                }
            }
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            if self.key == "HOME" {
                crate::config::schema::follow_test_home_env(self.original.as_deref().map(std::ffi::OsStr::new));
            }
            // SAFETY: test-only env manipulation; restoring previous value.
            unsafe {
                if let Some(original) = self.original.as_deref() {
                    std::env::set_var(self.key, original);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    fn doctor_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .expect("doctor env lock poisoned")
    }

    #[test]
    fn provider_validation_checks_custom_url_shape() {
        let config = Config::default();
        let validate = |provider: &str| {
            provider_validation_error(
                &config,
                provider,
                crate::providers::selection::default_model(&config),
                crate::providers::selection::credential_for(&config, provider),
            )
        };
        assert!(validate("openrouter").is_none());
        assert!(validate("custom:https://example.com").is_none());
        assert!(validate("anthropic-custom:https://example.com").is_none());

        let invalid_custom = validate("custom:").unwrap_or_default();
        assert!(invalid_custom.contains("requires a URL"));

        let invalid_unknown = validate("totally-fake").unwrap_or_default();
        assert!(invalid_unknown.contains("Unknown provider"));
    }

    #[test]
    fn diag_item_icons() {
        assert_eq!(DiagItem::ok("t", "m").icon(), "✅");
        assert_eq!(DiagItem::warn("t", "m").icon(), "⚠️ ");
        assert_eq!(DiagItem::error("t", "m").icon(), "❌");
    }

    #[tokio::test]
    async fn diagnostic_states_are_emitted_by_real_checks() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.config_path = temp.path().join("config.toml");
        config.workspace_dir = temp.path().to_path_buf();
        std::fs::write(&config.config_path, "configured").unwrap();

        let mut items = Vec::new();
        check_memory_diagnostics(&config, &mut items);
        check_runtime_readiness(&config, &mut items);
        check_environment(&mut items);
        check_daemon_state(&config, &mut items);
        check_runtime_memory_health(&config, &mut items);

        let states: std::collections::HashSet<_> = items.iter().map(|item| item.state).collect();
        for expected in [
            DiagnosticState::Declared,
            DiagnosticState::Configured,
            DiagnosticState::Ready,
            DiagnosticState::Healthy,
            DiagnosticState::Unknown,
        ] {
            assert!(states.contains(&expected), "missing diagnostic state {expected:?}");
        }
    }

    #[test]
    fn doctor_reports_legacy_fitness_counts_and_repair_command() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().join("workspace");
        std::fs::create_dir_all(config.workspace_dir.join("memory")).unwrap();
        let connection = rusqlite::Connection::open(config.workspace_dir.join("memory/brain.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE memories (key TEXT NOT NULL, content TEXT NOT NULL);
                 INSERT INTO memories VALUES ('self/fitness/daily/2026-09-10', 'one');
                 INSERT INTO memories VALUES ('self/fitness/daily/2026-09-11', 'two');
                 INSERT INTO memories VALUES ('keep/key', 'keep');",
            )
            .unwrap();
        std::fs::write(
            config.workspace_dir.join("MEMORY.md"),
            "- [t] **self/fitness/daily/2026-09-10**: legacy\n- [t] **keep/key**: keep\n",
        )
        .unwrap();
        std::fs::write(
            config.workspace_dir.join("MEMORY_SNAPSHOT.md"),
            "### 🔑 `self/fitness/daily/2026-09-10`\n\nlegacy\n\n---\n",
        )
        .unwrap();
        let mut items = Vec::new();

        check_legacy_fitness_telemetry(&config, &mut items);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, Severity::Warn);
        assert!(items[0].message.contains("database=2"));
        assert!(items[0].message.contains("MEMORY.md=1"));
        assert!(items[0].message.contains("MEMORY_SNAPSHOT.md=1"));
        assert!(items[0].message.contains("prx fitness migrate-legacy --apply"));
    }

    #[test]
    fn doctor_clean_fitness_probe_is_read_only_on_missing_database() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let db_path = config.workspace_dir.join("memory/brain.db");
        let mut items = Vec::new();

        check_legacy_fitness_telemetry(&config, &mut items);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, Severity::Ok);
        assert!(items[0].message.contains("absent"));
        assert!(!db_path.exists());
    }

    #[test]
    fn classify_model_probe_error_marks_unsupported_as_skipped() {
        let outcome = classify_model_probe_error("Provider 'copilot' does not support live model discovery yet");
        assert_eq!(outcome, ModelProbeVerdict::Skipped);
    }

    #[test]
    fn classify_model_probe_error_marks_auth_and_plan_issues() {
        let auth_outcome = classify_model_probe_error("OpenAI API error (401): unauthorized");
        assert_eq!(auth_outcome, ModelProbeVerdict::AuthOrAccess);

        let plan_outcome = classify_model_probe_error("Z.AI API error (429): plan does not include requested model");
        assert_eq!(plan_outcome, ModelProbeVerdict::AuthOrAccess);
    }

    #[test]
    fn config_validation_catches_bad_temperature() {
        let mut config = Config::default();
        config.default_temperature = 5.0;
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let temp_item = items.iter().find(|i| i.message.contains("temperature"));
        assert!(temp_item.is_some());
        assert_eq!(temp_item.unwrap().severity, Severity::Error);
    }

    #[test]
    fn config_validation_accepts_valid_temperature() {
        let mut config = Config::default();
        config.default_temperature = 0.7;
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let temp_item = items.iter().find(|i| i.message.contains("temperature"));
        assert!(temp_item.is_some());
        assert_eq!(temp_item.unwrap().severity, Severity::Ok);
    }

    #[test]
    fn config_validation_does_not_warn_for_explicit_unrestricted_autonomy() {
        let mut config = Config::default();
        config.autonomy.workspace_only = false;
        config.autonomy.forbidden_paths.clear();
        config.autonomy.max_cost_per_day_cents = Some(u32::MAX);
        let mut items = Vec::new();

        check_config_semantics(&config, &mut items);

        assert!(
            !items
                .iter()
                .any(|item| item.message.contains("unrestricted autonomy profile"))
        );
    }

    #[test]
    fn config_validation_acknowledged_unrestricted_autonomy_is_quiet() {
        let mut config = Config::default();
        config.autonomy.workspace_only = false;
        config.autonomy.forbidden_paths.clear();
        config.autonomy.max_cost_per_day_cents = Some(u32::MAX);
        config.autonomy.acknowledge_unrestricted_profile = true;
        let mut items = Vec::new();

        check_config_semantics(&config, &mut items);

        assert!(
            !items
                .iter()
                .any(|item| item.message.contains("explicit unrestricted autonomy profile"))
        );
    }

    #[test]
    fn config_validation_warns_for_explicit_delegate_wildcard() {
        let mut config = Config::default();
        config.agents.insert(
            "worker".into(),
            crate::config::DelegateAgentConfig {
                messaging: crate::config::schema::AgentMessaging::default(),
                approval_wait_secs: 600,
                limits: crate::config::AgentLimits::default(),
                provider: "openrouter".into(),
                model: "model-test".into(),
                system_prompt: None,
                api_key: None,
                credential_ref: None,
                temperature: None,
                max_depth: 3,
                agentic: true,
                allowed_tools: vec!["*".into()],
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
                enabled: true,
                autonomy: None,
            },
        );
        let mut items = Vec::new();

        check_config_semantics(&config, &mut items);

        let item = items
            .iter()
            .find(|item| item.message.contains("explicitly inherits all eligible parent tools"))
            .expect("delegate wildcard warning");
        assert_eq!(item.severity, Severity::Warn);
    }

    #[test]
    fn config_validation_warns_no_channels() {
        let config = Config::default();
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let ch_item = items.iter().find(|i| i.message.contains("channel"));
        assert!(ch_item.is_some());
        assert_eq!(ch_item.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn wacli_is_reported_as_a_configured_channel() {
        let mut config = Config::default();
        config.channels_config.cli = false;
        config.channels_config.wacli = Some(crate::config::WacliConfig::default());

        assert_eq!(configured_channel_names(&config), vec!["wacli"]);
        let mut items = Vec::new();
        check_channel_runtime(&config, &mut items);
        assert!(
            items
                .iter()
                .any(|item| item.message.contains("configured channels: wacli"))
        );
    }

    #[test]
    fn daemon_state_reports_stale_scheduler_as_unhealthy() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.config_path = temp.path().join("config.toml");

        let stale = (Utc::now() - chrono::Duration::seconds(SCHEDULER_STALE_SECONDS + 60)).to_rfc3339();
        let state = serde_json::json!({
            "pid": std::process::id(),
            "updated_at": Utc::now().to_rfc3339(),
            "components": {
                "scheduler": {
                    "status": "ok",
                    "updated_at": stale,
                    "last_ok": stale,
                    "last_error": null,
                    "restart_count": 0
                }
            }
        });
        std::fs::write(
            crate::daemon::state_file_path(&config),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();

        let mut items = Vec::new();
        check_daemon_state(&config, &mut items);

        let scheduler_item = items
            .iter()
            .find(|item| item.message.contains("scheduler unhealthy"))
            .expect("stale always-on scheduler should be unhealthy");
        assert_eq!(scheduler_item.severity, Severity::Error);
    }

    #[test]
    fn config_validation_catches_unknown_provider() {
        let mut config = Config::default();
        config.default_provider = Some("totally-fake".into());
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let prov_item = items.iter().find(|i| i.message.contains("default provider"));
        assert!(prov_item.is_some());
        assert_eq!(prov_item.unwrap().severity, Severity::Error);
    }

    #[test]
    fn config_validation_catches_malformed_custom_provider() {
        let mut config = Config::default();
        config.default_provider = Some("custom:".into());
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        let prov_item = items
            .iter()
            .find(|item| item.message.contains("default provider \"custom:\" is invalid"));
        assert!(prov_item.is_some());
        assert_eq!(prov_item.unwrap().severity, Severity::Error);
    }

    #[test]
    fn config_validation_accepts_custom_provider() {
        let mut config = Config::default();
        config.default_provider = Some("custom:https://my-api.com".into());
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let prov_item = items.iter().find(|i| i.message.contains("is valid"));
        assert!(prov_item.is_some());
        assert_eq!(prov_item.unwrap().severity, Severity::Ok);
    }

    #[test]
    fn config_validation_accepts_active_auth_profile_credential() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "").unwrap();
        let store = crate::auth::profiles::AuthProfilesStore::new(tmp.path(), false);
        store
            .upsert_profile(
                crate::auth::profiles::AuthProfile::new_token("moonshot", "default", "moonshot-test-key".to_string()),
                true,
            )
            .unwrap();

        let mut config = Config::default();
        config.config_path = tmp.path().join("config.toml");
        config.default_provider = Some("moonshot".into());
        config.api_key = None;
        config.secrets.encrypt = false;

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        let auth_item = items
            .iter()
            .find(|item| item.message.contains("auth profile credential configured"));
        assert!(
            auth_item.is_some(),
            "items: {:?}",
            items.iter().map(|i| &i.message).collect::<Vec<_>>()
        );
        assert_eq!(auth_item.unwrap().severity, Severity::Ok);
        assert!(
            !items.iter().any(|item| item.message.contains("no api_key")),
            "active auth profile should suppress missing api_key warning"
        );
    }

    #[test]
    fn config_validation_maps_kimi_alias_to_moonshot_auth_profile() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "").unwrap();
        let store = crate::auth::profiles::AuthProfilesStore::new(tmp.path(), false);
        store
            .upsert_profile(
                crate::auth::profiles::AuthProfile::new_token("moonshot", "default", "moonshot-test-key".to_string()),
                true,
            )
            .unwrap();

        let mut config = Config::default();
        config.config_path = tmp.path().join("config.toml");
        config.default_provider = Some("kimi".into());
        config.api_key = None;
        config.secrets.encrypt = false;

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        let auth_item = items
            .iter()
            .find(|item| item.message.contains("auth profile credential configured"));
        assert!(
            auth_item.is_some(),
            "items: {:?}",
            items.iter().map(|i| &i.message).collect::<Vec<_>>()
        );
        assert_eq!(auth_item.unwrap().severity, Severity::Ok);
    }

    #[test]
    fn config_validation_warns_bad_fallback() {
        let mut config = Config::default();
        config.reliability.fallback_providers = vec!["fake-provider".into()];
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let fb_item = items.iter().find(|i| i.message.contains("fallback provider"));
        assert!(fb_item.is_some());
        assert_eq!(fb_item.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn config_validation_warns_bad_custom_fallback() {
        let mut config = Config::default();
        config.reliability.fallback_providers = vec!["custom:".into()];
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        let fb_item = items
            .iter()
            .find(|item| item.message.contains("fallback provider \"custom:\" is invalid"));
        assert!(fb_item.is_some());
        assert_eq!(fb_item.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn config_validation_warns_provider_resilience_degraded_with_single_available_provider() {
        let _guard = doctor_env_lock();

        // Isolate from host credentials: override HOME to an empty temp dir so
        // resolve_claude_code_context cannot find ~/.claude/.credentials.json,
        // and clear any ambient Anthropic env vars.
        let iso_home = std::env::temp_dir().join(format!("openprx-doctor-degrade-test-{}", std::process::id()));
        std::fs::create_dir_all(&iso_home).unwrap();
        let _home_guard = EnvGuard::set("HOME", Some(iso_home.to_str().unwrap()));
        let _anthropic_key_guard = EnvGuard::set("ANTHROPIC_API_KEY", None);
        let _anthropic_oauth_guard = EnvGuard::set("ANTHROPIC_OAUTH_TOKEN", None);

        let mut config = Config::default();
        config.default_provider = Some("openai".into());
        config.api_key = Some("sk-test".into());
        config.reliability.fallback_providers = vec!["anthropic".into()];

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        let _ = std::fs::remove_dir_all(&iso_home);
        let degraded = items.iter().find(|item| {
            item.message
                .contains("provider resilience degraded: only 1 configured+available")
        });
        assert!(degraded.is_some());
        assert_eq!(degraded.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn config_validation_acknowledged_single_provider_risk_is_quiet() {
        let _guard = doctor_env_lock();
        let iso_home = std::env::temp_dir().join(format!("openprx-doctor-ack-test-{}", std::process::id()));
        std::fs::create_dir_all(&iso_home).unwrap();
        let _home_guard = EnvGuard::set("HOME", Some(iso_home.to_str().unwrap()));
        let _anthropic_key_guard = EnvGuard::set("ANTHROPIC_API_KEY", None);
        let _anthropic_oauth_guard = EnvGuard::set("ANTHROPIC_OAUTH_TOKEN", None);

        let mut config = Config::default();
        config.default_provider = Some("openai".into());
        config.api_key = Some("sk-test".into());
        config.reliability.fallback_providers = vec!["anthropic".into()];
        config.reliability.acknowledge_single_provider_risk = true;

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        assert!(
            !items
                .iter()
                .any(|item| item.message.contains("provider resilience degraded"))
        );
        let _ = std::fs::remove_dir_all(&iso_home);
    }

    #[test]
    fn config_validation_warns_model_fallback_route_mismatch() {
        let mut config = Config::default();
        config.default_provider = Some("openai".into());
        config.reliability.model_fallbacks.insert(
            "openai/gpt-4o".into(),
            vec!["anthropic/claude-sonnet-4-20250514".into()],
        );

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        let mismatch = items
            .iter()
            .find(|item| item.message.contains("model_fallbacks route mismatch"));
        assert!(mismatch.is_some());
        assert_eq!(mismatch.unwrap().severity, Severity::Warn);
    }

    /// D-072 L-9 / D-075: doctor mirrors the failover chain. A llama.cpp
    /// fallback never answers another provider's route, so a fallback model
    /// only it could serve is a route mismatch; on its own route it serves
    /// any repository, so no warning there.
    #[test]
    fn config_validation_follows_llamacpp_route_identity() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let base = Config {
            config_path: temp.path().join("config.toml"),
            workspace_dir: temp.path().join("workspace"),
            ..Config::default()
        };
        let warning = |config: &Config, needle: &str| {
            let mut items = Vec::new();
            check_config_semantics(config, &mut items);
            items.into_iter().find(|item| item.message.contains(needle))
        };

        // D-076 N-1: a llama.cpp fallback with nothing it may serve is never used.
        let mut unused = base.clone();
        unused.default_provider = Some("anthropic".into());
        unused.reliability.fallback_providers = vec!["llamacpp".into()];
        assert!(
            warning(&unused, "\"llamacpp\" is never used").is_some_and(|item| item.severity == Severity::Warn),
            "a llama.cpp fallback without a repository entry is dead config"
        );
        unused
            .reliability
            .model_fallbacks
            .insert("anthropic/claude-sonnet-5".into(), vec!["claude-haiku-5".into()]);
        assert!(
            warning(&unused, "\"llamacpp\" is never used").is_some(),
            "a bare id is routed to another provider"
        );

        // Named in model_fallbacks: the fallback serves it; no warning.
        let mut named = base.clone();
        named.default_provider = Some("anthropic".into());
        named.reliability.fallback_providers = vec!["llamacpp".into()];
        named.reliability.model_fallbacks.insert(
            "anthropic/claude-sonnet-5".into(),
            vec!["ggml-org/gpt-oss-20b-GGUF".into()],
        );
        assert!(warning(&named, "is never used").is_none());
        assert!(warning(&named, "model_fallbacks route mismatch").is_none());

        // A `<provider>/…` fallback model only the llama.cpp fallback could
        // take is routed to another provider: a mismatch.
        let mut elsewhere = named.clone();
        elsewhere.reliability.model_fallbacks.insert(
            "anthropic/claude-sonnet-5".into(),
            vec!["openai/gpt-5".into(), "ggml-org/gpt-oss-20b-GGUF".into()],
        );
        assert!(
            warning(&elsewhere, "model_fallbacks route mismatch").is_some_and(|item| item.severity == Severity::Warn)
        );

        // On its own route llama.cpp serves any repository.
        let mut own_route = base;
        own_route.default_provider = Some("llamacpp".into());
        own_route.default_model = Some("ggml-org/gpt-oss-20b-GGUF".into());
        own_route.reliability.model_fallbacks.insert(
            "ggml-org/gpt-oss-20b-GGUF".into(),
            vec!["google/gemma-3-27b-it-GGUF".into()],
        );
        assert!(warning(&own_route, "model_fallbacks route mismatch").is_none());
    }

    /// PMM-C: doctor resolves the configured default: refused is an error,
    /// id-only is a warning (D-018 Q1: it keeps running, unverified),
    /// declared is fine.
    #[test]
    fn doctor_reports_how_the_default_model_resolves() {
        let temp = tempfile::tempdir().expect("test: temp dir");
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        config.config_path = temp.path().join("config.toml");
        config.default_provider = Some("openrouter".to_string());
        config.api_key = Some("PMMC-DOCTOR-ROOT".to_string());
        let models_item = |config: &Config| {
            diagnose(config)
                .items
                .into_iter()
                .find(|item| item.category == "models" && item.message.contains("default model"))
        };

        config.default_model = Some("jev-decision-test".to_string());
        config.providers.models.push(
            toml::from_str(
                "provider = \"openrouter\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n",
            )
            .expect("test: declaration"),
        );
        let item = models_item(&config).expect("a refused default is reported");
        assert_eq!(item.severity, Severity::Error, "{}", item.message);
        assert!(item.message.contains("cannot be used for chat"), "{}", item.message);
        assert!(!item.message.contains("PMMC-DOCTOR-ROOT"));

        config.default_model = Some("vendor/id-only".to_string());
        let item = models_item(&config).expect("an unverified default is reported");
        assert_eq!(item.severity, Severity::Warn, "{}", item.message);
        assert!(item.message.contains("unverified"), "{}", item.message);

        config.providers.models.push(
            toml::from_str("provider = \"openrouter\"\nmodel = \"vendor/id-only\"\noperations = [\"chat\"]\n")
                .expect("test: declaration"),
        );
        let item = models_item(&config).expect("a verified default is reported");
        assert_eq!(item.severity, Severity::Ok, "{}", item.message);
    }

    #[test]
    fn config_validation_warns_empty_model_route() {
        let mut config = Config::default();
        config.model_routes = vec![crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "fast".into(),
            provider: "groq".into(),
            model: String::new(),
            api_key: None,
            credential_ref: None,
        }];
        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let route_item = items.iter().find(|i| i.message.contains("empty model"));
        assert!(route_item.is_some());
        assert_eq!(route_item.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn config_validation_warns_empty_embedding_route_model() {
        let mut config = Config::default();
        config.embedding_routes = vec![crate::config::EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "openai".into(),
            model: String::new(),
            dimensions: Some(1536),
            api_key: None,
            credential_ref: None,
        }];

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let route_item = items
            .iter()
            .find(|item| item.message.contains("embedding route \"semantic\" has empty model"));
        assert!(route_item.is_some());
        assert_eq!(route_item.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn config_validation_warns_invalid_embedding_route_provider() {
        let mut config = Config::default();
        config.embedding_routes = vec![crate::config::EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "groq".into(),
            model: "text-embedding-3-small".into(),
            dimensions: None,
            api_key: None,
            credential_ref: None,
        }];

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let route_item = items
            .iter()
            .find(|item| item.message.contains("uses invalid provider \"groq\""));
        assert!(route_item.is_some());
        assert_eq!(route_item.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn config_validation_warns_missing_embedding_hint_target() {
        let mut config = Config::default();
        config.memory.embedding_model = "hint:semantic".into();

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);
        let route_item = items
            .iter()
            .find(|item| item.message.contains("no matching [[embedding_routes]] entry exists"));
        assert!(route_item.is_some());
        assert_eq!(route_item.unwrap().severity, Severity::Warn);
    }

    #[test]
    fn environment_check_finds_git() {
        let mut items = Vec::new();
        check_environment(&mut items);
        let git_item = items.iter().find(|i| i.message.starts_with("git:"));
        // git should be available in any CI/dev environment
        assert!(git_item.is_some());
        assert_eq!(git_item.unwrap().severity, Severity::Ok);
    }

    #[test]
    fn parse_df_available_mb_uses_last_data_line() {
        let stdout = "Filesystem 1M-blocks Used Available Use% Mounted on\n/dev/sda1 1000 500 500 50% /\n";
        assert_eq!(parse_df_available_mb(stdout), Some(500));
    }

    #[test]
    fn truncate_for_display_preserves_utf8_boundaries() {
        let preview = truncate_for_display("🙂example-alpha-build", 3);
        assert_eq!(preview, "🙂ex…");
    }

    #[test]
    fn workspace_check_does_not_create_probe_files() {
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("marker.txt");
        std::fs::write(&marker, "existing").unwrap();
        let before: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let mut config = Config::default();
        config.workspace_dir = tmp.path().to_path_buf();
        let mut items = Vec::new();

        check_workspace(&config, &mut items);

        let after: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(before, after);
        assert!(
            items
                .iter()
                .any(|item| item.message.contains("no write probe performed"))
        );
    }

    #[test]
    fn doctor_run_returns_error_when_report_contains_errors() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.config_path = temp.path().join("missing-config.toml");
        config.workspace_dir = temp.path().join("missing-workspace");

        assert!(
            run(&config).is_err(),
            "doctor must return nonzero semantics for ERROR findings"
        );
    }

    #[tokio::test]
    async fn runtime_memory_probe_does_not_create_missing_sqlite_database() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let db_path = config.workspace_dir.join("memory").join("brain.db");

        let mut items = Vec::new();
        check_runtime_memory_health(&config, &mut items);

        assert!(!db_path.exists(), "doctor memory probe must not create brain.db");
    }

    #[test]
    fn config_validation_reports_delegate_agents_in_sorted_order() {
        let mut config = Config::default();
        config.agents.insert(
            "zeta".into(),
            crate::config::DelegateAgentConfig {
                messaging: crate::config::schema::AgentMessaging::default(),
                approval_wait_secs: 600,
                limits: crate::config::AgentLimits::default(),
                provider: "totally-fake".into(),
                model: "model-z".into(),
                system_prompt: None,
                api_key: None,
                credential_ref: None,
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
                enabled: true,
                autonomy: None,
            },
        );
        config.agents.insert(
            "alpha".into(),
            crate::config::DelegateAgentConfig {
                messaging: crate::config::schema::AgentMessaging::default(),
                approval_wait_secs: 600,
                limits: crate::config::AgentLimits::default(),
                provider: "totally-fake".into(),
                model: "model-a".into(),
                system_prompt: None,
                api_key: None,
                credential_ref: None,
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
                enabled: true,
                autonomy: None,
            },
        );

        let mut items = Vec::new();
        check_config_semantics(&config, &mut items);

        let agent_messages: Vec<_> = items
            .iter()
            .filter(|item| item.message.starts_with("agent \""))
            .map(|item| item.message.as_str())
            .collect();

        assert_eq!(agent_messages.len(), 2);
        assert!(agent_messages[0].contains("agent \"alpha\""));
        assert!(agent_messages[1].contains("agent \"zeta\""));
    }

    // ---- FIX-P2-06: runtime readiness sub-checks ------------------------

    #[test]
    fn runtime_readiness_emits_six_subchecks() {
        let config = Config::default();
        let mut items = Vec::new();
        check_runtime_readiness(&config, &mut items);
        let readiness: Vec<&str> = items
            .iter()
            .filter(|i| i.category == "readiness")
            .map(|i| i.message.as_str())
            .collect();
        assert_eq!(readiness.len(), 6, "expected 6 readiness sub-checks");
        for prefix in ["owner:", "topic:", "task:", "document:", "vector:", "runtime-control:"] {
            assert!(
                readiness.iter().any(|m| m.starts_with(prefix)),
                "missing readiness sub-check with prefix {prefix}"
            );
        }
        assert!(items.iter().any(|item| {
            item.state == DiagnosticState::Ready && item.message.starts_with("topic: local similarity")
        }));
        assert!(
            items.iter().any(|item| {
                item.state == DiagnosticState::Ready && item.message.starts_with("vector: index ready")
            })
        );
    }

    #[test]
    fn runtime_readiness_flags_zero_dimension_with_embeddings() {
        let mut config = Config::default();
        config.memory.embedding_provider = "openai".into();
        config.memory.embedding_dimensions = 0;
        let mut items = Vec::new();
        check_runtime_readiness(&config, &mut items);
        assert!(
            items.iter().any(|i| i.category == "readiness"
                && i.severity == Severity::Error
                && i.message.starts_with("vector:")),
            "expected vector readiness error for zero dimension"
        );
    }

    // ---- FIX-P2-07: postgres health -------------------------------------

    #[tokio::test]
    async fn postgres_health_skips_when_not_selected() {
        // Default backend is sqlite -> no postgres items emitted at all.
        let config = Config::default();
        let mut items = Vec::new();
        check_postgres_health(&config, &mut items).await;
        assert!(items.iter().all(|i| i.category != "postgres"));
    }

    #[tokio::test]
    async fn postgres_health_errors_on_missing_db_url() {
        let mut config = Config::default();
        config.memory.backend = "postgres".into();
        let mut items = Vec::new();
        check_postgres_health(&config, &mut items).await;
        assert!(
            items
                .iter()
                .any(|i| i.category == "postgres" && i.severity == Severity::Error),
            "expected error when postgres selected without db_url"
        );
    }

    #[tokio::test]
    async fn postgres_health_errors_on_bad_scheme() {
        let mut config = Config::default();
        config.memory.backend = "postgres".into();
        config.storage.provider.config.db_url = Some("mysql://host/db".into());
        let mut items = Vec::new();
        check_postgres_health(&config, &mut items).await;
        assert!(
            items.iter().any(|i| i.category == "postgres"
                && i.severity == Severity::Error
                && i.message.contains("postgres://")),
            "expected scheme validation error"
        );
    }

    // ---- FIX-P2-08: embedding endpoint probe ----------------------------

    #[tokio::test]
    async fn embedding_probe_skips_without_custom_provider() {
        // Default provider is "none" -> no embedding probe items.
        let config = Config::default();
        let mut items = Vec::new();
        check_embedding_endpoint(&config, &mut items).await;
        assert!(items.iter().all(|i| i.category != "embedding"));
    }

    #[tokio::test]
    async fn embedding_probe_warns_on_unreachable_custom_endpoint() {
        let mut config = Config::default();
        // Port 1 on localhost is reserved and refuses connections quickly.
        config.memory.embedding_provider = "custom:http://127.0.0.1:1/embed".into();
        let mut items = Vec::new();
        check_embedding_endpoint(&config, &mut items).await;
        assert!(
            items
                .iter()
                .any(|i| i.category == "embedding" && i.severity == Severity::Warn),
            "expected warn for unreachable embedding endpoint"
        );
    }

    // ---- daemon process PATH (v0.8.139) ---------------------------------

    fn environ_blob(entries: &[&str]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for entry in entries {
            bytes.extend_from_slice(entry.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    fn path_value(dirs: &[&Path]) -> String {
        std::env::join_paths(dirs).unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn daemon_path_from_environ_picks_the_path_entry() {
        let blob = environ_blob(&["HOME=/home/ck", "PATH=/usr/bin:/bin", "LANG=C"]);
        assert_eq!(daemon_path_from_environ(&blob).as_deref(), Some("/usr/bin:/bin"));
    }

    #[test]
    fn daemon_path_from_environ_ignores_path_lookalike_keys() {
        let blob = environ_blob(&["LD_LIBRARY_PATH=/opt/lib", "MANPATH=/usr/share/man"]);
        assert!(daemon_path_from_environ(&blob).is_none());
    }

    #[test]
    fn daemon_path_ok_when_required_directory_is_present() {
        let temp = TempDir::new().unwrap();
        let bin = temp.path().join("cargo-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let other = temp.path().join("usr-bin");
        std::fs::create_dir_all(&other).unwrap();

        let path = path_value(&[other.as_path(), bin.as_path()]);
        let blob = environ_blob(&["HOME=/home/ck", &format!("PATH={path}")]);
        let item = evaluate_daemon_environ(&blob, std::slice::from_ref(&bin));

        assert_eq!(item.severity, Severity::Ok, "message: {}", item.message);
        assert_eq!(item.category, "daemon");
        assert!(
            item.message.contains(&path),
            "ok message must print the daemon PATH, got: {}",
            item.message
        );
    }

    #[test]
    fn daemon_path_warns_when_required_directory_is_missing() {
        let temp = TempDir::new().unwrap();
        let bin = temp.path().join("cargo-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let other = temp.path().join("usr-bin");
        std::fs::create_dir_all(&other).unwrap();

        let path = path_value(&[other.as_path()]);
        let blob = environ_blob(&[&format!("PATH={path}")]);
        let item = evaluate_daemon_environ(&blob, std::slice::from_ref(&bin));

        assert_eq!(item.severity, Severity::Warn, "message: {}", item.message);
        assert!(
            item.message.contains(&bin.display().to_string()),
            "warn message must name the missing directory, got: {}",
            item.message
        );
        assert!(
            item.message.contains("Environment=\"PATH="),
            "warn message must carry the systemd fix, got: {}",
            item.message
        );
        assert!(
            item.message.contains("prx service install"),
            "warn message must carry the re-install fix, got: {}",
            item.message
        );
    }

    #[test]
    fn daemon_path_warns_when_environ_has_no_path_key() {
        let blob = environ_blob(&["HOME=/home/ck", "LANG=C"]);
        let item = evaluate_daemon_environ(&blob, &[PathBuf::from("/home/ck/.cargo/bin")]);

        assert_eq!(item.severity, Severity::Warn, "message: {}", item.message);
        assert!(
            item.message.contains("no PATH entry"),
            "warn message must explain the missing PATH key, got: {}",
            item.message
        );
        assert!(
            item.message.contains("Environment="),
            "warn message must carry the systemd fix, got: {}",
            item.message
        );
    }

    /// `prx doctor` is very often run from a build tree (`cargo run -- doctor`,
    /// `./target/release/prx doctor`, a one-shot CI copy). Requiring the
    /// doctor's own directory there produced a warning about a healthy daemon
    /// *and* told the operator to put a build directory on the unit's PATH.
    ///
    /// MUTATION GUARD: drop the `is_cargo_build_dir` filter and this fails,
    /// because the test binary itself lives in `target/debug/deps`.
    #[test]
    fn a_doctor_running_from_a_build_directory_never_demands_it_on_the_daemon_path() {
        let required = required_daemon_path_dirs(None);
        assert!(
            required.iter().all(|dir| !is_cargo_build_dir(dir)),
            "a build output directory must never be proposed for a systemd unit: {required:?}"
        );
        let item = evaluate_daemon_path("/usr/local/bin:/usr/bin:/bin", &required);
        assert!(
            !item.message.contains("target/debug"),
            "the repair text must not name a build directory: {}",
            item.message
        );
    }

    /// The directory that actually matters is the daemon's, and the daemon
    /// records it. When it does, the doctor's own location is irrelevant.
    #[test]
    fn the_required_directory_comes_from_the_daemon_binary_the_state_file_records() {
        let required = required_daemon_path_dirs(Some(Path::new("/usr/local/bin/prx")));
        assert!(
            required.contains(&canonical_or_owned(Path::new("/usr/local/bin"))),
            "the daemon's own directory must be required: {required:?}"
        );
        assert!(
            required.iter().all(|dir| !is_cargo_build_dir(dir)),
            "a recorded daemon binary replaces the doctor's own directory: {required:?}"
        );

        let complete = required
            .iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(":");
        let item = evaluate_daemon_path(&complete, &required);
        assert_eq!(
            item.severity,
            Severity::Ok,
            "a PATH that holds every required directory is healthy: {}",
            item.message
        );

        let item = evaluate_daemon_path("/usr/bin:/bin", &required);
        assert_eq!(item.severity, Severity::Warn);
        assert!(
            item.message.contains("/usr/local/bin"),
            "a PATH without the daemon's own directory must name it: {}",
            item.message
        );
    }

    #[test]
    fn the_daemon_binary_is_read_back_from_the_state_file_when_it_is_recorded() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.config_path = temp.path().join("config.toml");
        assert_eq!(daemon_binary_from_state(&config), None, "no state file, no answer");

        std::fs::write(
            temp.path().join("daemon_state.json"),
            br#"{"pid":1,"binary_path":"/usr/local/bin/prx"}"#,
        )
        .unwrap();
        assert_eq!(
            daemon_binary_from_state(&config),
            Some(PathBuf::from("/usr/local/bin/prx"))
        );

        std::fs::write(
            temp.path().join("daemon_state.json"),
            br#"{"pid":1,"binary_path":"  "}"#,
        )
        .unwrap();
        assert_eq!(daemon_binary_from_state(&config), None, "a blank field is not a path");
    }
