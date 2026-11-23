#[cfg(test)]
mod tests {
    /// S7: the child snapshot fingerprint keeps its encoding:
    /// SHA-256 over the sorted JSON of `(workspace_dir, config_path, config)`.
    #[test]
    fn for_child_fingerprint_encoding_is_unchanged() {
        use sha2::Digest as _;
        // The vector input of `runtime::child_source::tests::golden_effective_config`
        // with the two paths set; its serialized object is the same literal.
        let mut config = crate::config::Config::default();
        config.decision_engine.backend =
            Some(crate::config::schema::decision_engine::effective_engine_backend(&config).clone());
        for model in ["zeta/model", "alpha/model"] {
            config.cost.prices.insert(
                model.to_owned(),
                crate::config::schema::ModelPricing {
                    input: 1.0,
                    output: 2.0,
                    cache_write: 0.0,
                    cache_read: 0.0,
                },
            );
        }
        config.workspace_dir = std::path::PathBuf::from("/prx/ws-1");
        config.config_path = std::path::PathBuf::from("/prx/config.toml");
        let generation = super::ConfigGeneration::for_child(&config).unwrap();
        // §2: SHA-256 of the sorted array `[workspace, config_path, config]`,
        // spelled out from literals, and the pinned digest of those bytes.
        let preimage = [
            br#"["/prx/ws-1","/prx/config.toml","#.as_slice(),
            include_bytes!("../../runtime/child_source/golden_effective_config.json").as_slice(),
            b"]",
        ]
        .concat();
        const FOR_CHILD_FINGERPRINT: &str = "82d04107528a017756b4311354691ae8cdae998870eb4902307c16e0b9e23df4";
        assert_eq!(format!("{:x}", sha2::Sha256::digest(&preimage)), FOR_CHILD_FINGERPRINT);
        assert_eq!(generation.source_revision.fingerprint_sha256, FOR_CHILD_FINGERPRINT);
        assert_eq!(generation.id.0, 0);
        assert!(generation.source_revision.disk_generation.is_none());
    }
    use super::*;
    use serial_test::serial;

    struct RecordingParticipant {
        name: &'static str,
        events: Arc<Mutex<Vec<String>>>,
        fail_prepare: bool,
        fail_commit: bool,
    }

    struct RecordingPrepared {
        name: &'static str,
        events: Arc<Mutex<Vec<String>>>,
        fail_commit: bool,
    }

    struct RecordingAuditSink {
        event_types: Arc<Mutex<Vec<String>>>,
    }

    impl ConfigGenerationAuditSink for RecordingAuditSink {
        fn record(&self, event: ConfigGenerationAuditEvent) {
            self.event_types.lock().push(event.event_type);
        }
    }

    impl ConfigGenerationParticipant for RecordingParticipant {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supports_rebuild_field(&self, _field: &str) -> bool {
            true
        }

        fn supports_controlled_restart_field(&self, _field: &str) -> bool {
            true
        }

        fn prepares_for_field(&self, _field: &str) -> bool {
            true
        }

        fn prepare(
            &self,
            _generation: Arc<ConfigGeneration>,
            _changed_fields: &[String],
        ) -> anyhow::Result<Box<dyn PreparedConfigGeneration>> {
            self.events.lock().push(format!("{}.prepare", self.name));
            if self.fail_prepare {
                anyhow::bail!("injected prepare failure");
            }
            Ok(Box::new(RecordingPrepared {
                name: self.name,
                events: Arc::clone(&self.events),
                fail_commit: self.fail_commit,
            }))
        }
    }

    impl PreparedConfigGeneration for RecordingPrepared {
        fn commit(&mut self) -> anyhow::Result<()> {
            self.events.lock().push(format!("{}.commit", self.name));
            if self.fail_commit {
                anyhow::bail!("injected commit failure");
            }
            Ok(())
        }

        fn rollback(&mut self) {
            self.events.lock().push(format!("{}.rollback", self.name));
        }

        fn finalize(&mut self) {
            self.events.lock().push(format!("{}.finalize", self.name));
        }
    }

    /// The proxy the publication tests publish: a real `all_proxy`, so the
    /// publication stays observable, but with loopback exempt, so a test
    /// running at the same time against its own `127.0.0.1` server is never
    /// sent to a proxy that is not there (R-INT2A F3). A services scope with a
    /// dummy key would be refused by validation, which only knows real keys.
    fn test_publication_proxy(url: &str) -> crate::config::ProxyConfig {
        crate::config::ProxyConfig {
            all_proxy: Some(url.to_string()),
            no_proxy: vec!["127.0.0.1".to_string(), "localhost".to_string(), "::1".to_string()],
            ..Config::default().proxy
        }
    }

    /// Restores the process-wide runtime proxy when dropped, so a failing
    /// assertion cannot leave a test proxy published.
    struct RuntimeProxyRestore(crate::config::ProxyConfig);

    impl RuntimeProxyRestore {
        fn capture() -> Self {
            Self(crate::config::runtime_proxy_config())
        }

        fn publish(proxy: crate::config::ProxyConfig) -> Self {
            let restore = Self::capture();
            crate::config::set_runtime_proxy_config(proxy);
            restore
        }
    }

    impl Drop for RuntimeProxyRestore {
        fn drop(&mut self) {
            crate::config::set_runtime_proxy_config(self.0.clone());
        }
    }

    fn participant(
        name: &'static str,
        events: Arc<Mutex<Vec<String>>>,
        fail_prepare: bool,
        fail_commit: bool,
    ) -> Arc<dyn ConfigGenerationParticipant> {
        Arc::new(RecordingParticipant {
            name,
            events,
            fail_prepare,
            fail_commit,
        })
    }

    #[test]
    #[serial]
    fn one_operation_keeps_pinned_generation_after_reload() {
        let manager = ConfigGenerationManager::new(Config::default());
        let pinned = manager.pin();
        let mut desired = (*pinned.effective).clone();
        desired.default_temperature = 0.31;

        let report = manager
            .apply_runtime_config(desired, ConfigReloadTrigger::Test)
            .expect("apply config");

        assert_eq!(pinned.id, ConfigGenerationId(0));
        assert_eq!(manager.pin().id, report.active_generation);
        assert_ne!(pinned.id, manager.pin().id);
        assert_ne!(
            pinned.effective.default_temperature,
            manager.pin().effective.default_temperature
        );
    }

    #[test]
    #[serial]
    fn restart_only_fields_update_desired_but_not_active() {
        let manager = ConfigGenerationManager::new(Config::default());
        let old_active = manager.load_full();
        let mut desired = (*old_active).clone();
        desired.runtime.kind = "docker".to_string();

        let report = manager
            .apply_runtime_config(desired.clone(), ConfigReloadTrigger::Test)
            .expect("apply config");

        assert_eq!(manager.load_full().runtime.kind, old_active.runtime.kind);
        assert_eq!(manager.desired().config.runtime.kind, desired.runtime.kind);
        assert_eq!(report.restart_required, vec!["runtime"]);
        assert_eq!(report.status(), "restart_required");
    }

    /// `[skills_lifecycle]` is read once at process start (the service, its
    /// store and the registered tools are built from it), so a reload that
    /// changes it reports `restart_required` and the effective configuration
    /// keeps the old value, while the desired one records the edit.
    #[test]
    #[serial]
    fn a_changed_skills_lifecycle_needs_a_restart_and_keeps_the_old_value() {
        let manager = ConfigGenerationManager::new(Config::default());
        let old_active = manager.load_full();
        assert!(!old_active.skills_lifecycle.enabled);
        let mut desired = (*old_active).clone();
        desired.skills_lifecycle.enabled = true;
        desired.skills_lifecycle.owner_scope = Some("owner:ws:cli:alice".to_string());
        desired.skills_lifecycle.mode = crate::config::skills_lifecycle::SkillsLifecycleModeConfig::Auto;

        let report = manager
            .apply_runtime_config(desired.clone(), ConfigReloadTrigger::Test)
            .expect("apply config");

        assert_eq!(report.restart_required, vec!["skills_lifecycle"]);
        assert_eq!(report.status(), "restart_required");
        assert_eq!(manager.load_full().skills_lifecycle, old_active.skills_lifecycle);
        assert!(!manager.load_full().skills_lifecycle.enabled);
        assert_eq!(manager.desired().config.skills_lifecycle, desired.skills_lifecycle);
        assert!(is_process_restart_only("skills_lifecycle"));
    }

    #[test]
    #[serial]
    fn reload_report_classifies_runtime_changes() {
        let manager = ConfigGenerationManager::new(Config::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let runtime = participant("runtime", events, false, false);
        manager.register_participant(&runtime);
        let mut desired = (*manager.load_full()).clone();
        desired.default_temperature = 0.22;
        desired.default_model = Some("example/model".to_string());
        desired.cron.max_run_history += 1;

        let report = manager
            .apply_runtime_config(desired, ConfigReloadTrigger::Test)
            .expect("apply config");

        assert_eq!(report.applied, vec!["default_temperature"]);
        assert_eq!(report.rebuilt, vec!["default_model"]);
        assert_eq!(report.restarted, vec!["cron"]);
        assert!(report.restart_required.is_empty(), "expected an empty value");
    }

    /// CONSOLE-OBS-3 (CONSOLE-VERIFY): once the gateway paired, every later
    /// save reported `restart_required: ["gateway"]`, even one that changed
    /// only `agent.compact_context`. `/pair` persists the new token hash and
    /// reloads; `gateway` is restart-only, so the active generation kept the
    /// old (empty) token list while disk held the new one, and every reload
    /// after it found `gateway` different again. The pairing guard already
    /// holds that token, so the pairing reload adopts the persisted list into
    /// the active generation; nothing else about `gateway` is adopted.
    #[test]
    #[serial]
    fn a_save_after_pairing_does_not_report_a_gateway_restart() {
        let manager = ConfigGenerationManager::new(Config::default());
        let mut paired = (*manager.load_full()).clone();
        paired.gateway.paired_tokens = vec![crate::security::pairing::hash_token("zc_paired_in_test")];

        let pairing = manager
            .apply_runtime_config(paired.clone(), ConfigReloadTrigger::PairingPersistence)
            .expect("test: pairing reload");
        assert!(
            pairing.restart_required.is_empty(),
            "the token pairing just issued is live already: {:?}",
            pairing.restart_required
        );
        assert_eq!(
            manager.load_full().gateway.paired_tokens,
            paired.gateway.paired_tokens,
            "the active generation carries the persisted token list"
        );
        assert_eq!(pairing.applied, vec!["gateway"], "the adoption is reported as applied");

        let mut save = paired;
        save.agent.compact_context = !save.agent.compact_context;
        let report = manager
            .apply_runtime_config(save, ConfigReloadTrigger::Api)
            .expect("test: low-risk save");
        assert!(
            report.restart_required.is_empty(),
            "a save of agent.compact_context must not report a restart: {:?}",
            report.restart_required
        );
        assert_eq!(report.applied, vec!["agent"]);
    }

    /// The pairing adoption is exactly the token list: another `gateway`
    /// change in the same reload stays deferred, and a token list changed by
    /// any other writer (a hand edit picked up by the watcher) still needs a
    /// restart, because the running pairing guard never loaded it.
    #[test]
    #[serial]
    fn only_pairing_adopts_only_the_token_list() {
        let manager = ConfigGenerationManager::new(Config::default());
        let original_port = manager.load_full().gateway.port;
        let mut desired = (*manager.load_full()).clone();
        desired.gateway.paired_tokens = vec![crate::security::pairing::hash_token("zc_first")];
        desired.gateway.port = original_port.wrapping_add(1);

        let report = manager
            .apply_runtime_config(desired.clone(), ConfigReloadTrigger::PairingPersistence)
            .expect("test: pairing reload with a port change");
        assert_eq!(report.restart_required, vec!["gateway"]);
        let active = manager.load_full();
        assert_eq!(
            active.gateway.port, original_port,
            "the port change waits for a restart"
        );
        assert_eq!(active.gateway.paired_tokens, desired.gateway.paired_tokens);

        let mut hand_edit = desired;
        hand_edit.gateway.port = original_port;
        hand_edit.gateway.paired_tokens.push("operator-added-token".to_string());
        let report = manager
            .apply_runtime_config(hand_edit, ConfigReloadTrigger::FileWatcher)
            .expect("test: watcher reload");
        assert_eq!(
            report.restart_required,
            vec!["gateway"],
            "a token the pairing guard never loaded is not live until restart"
        );
        assert_eq!(manager.load_full().gateway.paired_tokens.len(), 1);
    }

    #[test]
    fn auth_and_proxy_are_rebuild_and_swap_fields() {
        assert!(is_rebuild_and_swap("auth"));
        assert!(is_rebuild_and_swap("proxy"));
        assert!(!is_process_restart_only("auth"));
        assert!(!is_process_restart_only("proxy"));
    }

    /// Serve one HTTP response on a loopback port and return its URL.
    fn one_shot_loopback_server() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("test: bind");
        let addr = listener.local_addr().expect("test: addr");
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let mut buffer = [0_u8; 1024];
                let _ = socket.read(&mut buffer);
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
            }
        });
        format!("http://{addr}/")
    }

    /// R-INT2A F3: while a publication test's proxy is live, a concurrently
    /// running test's runtime-proxy-built client can still reach its own
    /// loopback server. Before, the published `all_proxy` applied to every
    /// service with no loopback exemption, so such a client was sent to the
    /// absent proxy and refused (this is how the A12 test hung).
    #[tokio::test]
    #[serial]
    async fn a_published_test_proxy_leaves_loopback_clients_alone() {
        let url = one_shot_loopback_server();
        let _restore = RuntimeProxyRestore::publish(test_publication_proxy("http://127.0.0.1:8080"));
        let client = crate::config::build_runtime_proxy_client("provider.compatible").expect("test: client");
        let response = tokio::time::timeout(std::time::Duration::from_secs(20), client.get(&url).send())
            .await
            .expect("test: bounded")
            .expect("test: the loopback request bypasses the test proxy");
        assert!(response.status().is_success());
    }

    #[test]
    #[serial]
    fn proxy_runtime_state_tracks_successful_generation_publication() {
        let _restore = RuntimeProxyRestore::capture();
        let initial = Config::default();
        let manager = ConfigGenerationManager::new(initial.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        let runtime = participant("runtime", events, false, false);
        manager.register_participant(&runtime);
        let mut desired = initial;
        desired.proxy = test_publication_proxy("http://127.0.0.1:8080");

        let report = manager
            .apply_runtime_config(desired.clone(), ConfigReloadTrigger::Test)
            .expect("proxy generation must publish");
        let runtime_proxy = crate::config::runtime_proxy_config();

        assert_eq!(report.rebuilt, vec!["proxy"]);
        assert!(runtime_proxy.has_any_proxy_url());
        assert_eq!(runtime_proxy.all_proxy, desired.proxy.all_proxy);
    }

    #[test]
    #[serial]
    fn proxy_runtime_state_rolls_back_when_participant_rejects_candidate() {
        let _restore = RuntimeProxyRestore::capture();
        let initial = Config::default();
        let manager = ConfigGenerationManager::new(initial.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        let rejecting = participant("rejecting", events, true, false);
        manager.register_participant(&rejecting);
        let mut desired = initial.clone();
        desired.proxy = test_publication_proxy("http://127.0.0.1:8081");

        manager
            .apply_runtime_config(desired, ConfigReloadTrigger::Test)
            .expect_err("rejected proxy candidate must roll back");
        let runtime_proxy = crate::config::runtime_proxy_config();

        assert_eq!(manager.active_generation_id(), ConfigGenerationId(0));
        assert_eq!(runtime_proxy.has_any_proxy_url(), initial.proxy.has_any_proxy_url());
        assert_eq!(runtime_proxy.all_proxy, initial.proxy.all_proxy);
    }

    #[test]
    #[serial]
    fn participant_prepare_failure_rolls_back_and_keeps_active_generation() {
        let manager = ConfigGenerationManager::new(Config::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let first = participant("first", Arc::clone(&events), false, false);
        let second = participant("second", Arc::clone(&events), true, false);
        manager.register_participant(&first);
        manager.register_participant(&second);
        let mut desired = (*manager.load_full()).clone();
        desired.default_temperature = 0.19;

        let error = manager
            .apply_runtime_config(desired.clone(), ConfigReloadTrigger::Test)
            .expect_err("second participant must reject candidate");

        assert!(error.to_string().contains("second"));
        assert_eq!(manager.active_generation_id(), ConfigGenerationId(0));
        assert_eq!(
            manager.desired().config.default_temperature,
            desired.default_temperature
        );
        assert_eq!(
            *events.lock(),
            vec!["first.prepare", "second.prepare", "first.rollback"]
        );
        assert!(manager.status().last_failure.is_some());
    }

    #[test]
    #[serial]
    fn participant_commit_failure_rolls_every_candidate_back_before_publish() {
        let manager = ConfigGenerationManager::new(Config::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let first = participant("first", Arc::clone(&events), false, false);
        let second = participant("second", Arc::clone(&events), false, true);
        manager.register_participant(&first);
        manager.register_participant(&second);
        let mut desired = (*manager.load_full()).clone();
        desired.default_temperature = 0.18;

        manager
            .apply_runtime_config(desired, ConfigReloadTrigger::Test)
            .expect_err("second participant commit must fail");

        assert_eq!(manager.active_generation_id(), ConfigGenerationId(0));
        assert_eq!(
            *events.lock(),
            vec![
                "first.prepare",
                "second.prepare",
                "first.commit",
                "second.commit",
                "second.rollback",
                "first.rollback",
            ]
        );
    }

    #[test]
    #[serial]
    fn successful_participants_finalize_only_after_publication() {
        let manager = ConfigGenerationManager::new(Config::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let participant = participant("runtime", Arc::clone(&events), false, false);
        manager.register_participant(&participant);
        let mut desired = (*manager.load_full()).clone();
        desired.default_temperature = 0.17;

        let report = manager
            .apply_runtime_config(desired, ConfigReloadTrigger::Test)
            .expect("candidate must publish");

        assert_eq!(manager.active_generation_id(), report.active_generation);
        assert_eq!(report.participant_acks, vec!["runtime"]);
        assert_eq!(
            *events.lock(),
            vec!["runtime.prepare", "runtime.commit", "runtime.finalize"]
        );
    }

    #[test]
    #[serial]
    fn concurrent_reload_attempts_are_serialized_into_distinct_generations() {
        let manager = Arc::new(ConfigGenerationManager::new(Config::default()));
        let mut handles = Vec::new();
        for temperature in [0.12, 0.13] {
            let manager = Arc::clone(&manager);
            handles.push(std::thread::spawn(move || {
                let mut desired = (*manager.load_full()).clone();
                desired.default_temperature = temperature;
                manager
                    .apply_runtime_config(desired, ConfigReloadTrigger::Test)
                    .expect("serialized reload")
                    .active_generation
            }));
        }
        let mut generations = handles
            .into_iter()
            .map(|handle| handle.join().expect("reload thread"))
            .collect::<Vec<_>>();
        generations.sort();

        assert_eq!(generations, vec![ConfigGenerationId(1), ConfigGenerationId(2)]);
        assert_eq!(manager.active_generation_id(), ConfigGenerationId(2));
    }

    #[test]
    #[serial]
    fn reload_and_supervisor_lifecycle_use_the_shared_audit_sink() {
        let manager = ConfigGenerationManager::new(Config::default());
        let participant_events = Arc::new(Mutex::new(Vec::new()));
        let runtime = participant("runtime", participant_events, false, false);
        manager.register_participant(&runtime);
        let audit_events = Arc::new(Mutex::new(Vec::new()));
        manager.register_audit_sink(Arc::new(RecordingAuditSink {
            event_types: Arc::clone(&audit_events),
        }));
        let mut desired = (*manager.load_full()).clone();
        desired.default_temperature = 0.11;
        desired.cron.max_run_history += 1;

        manager
            .apply_runtime_config(desired, ConfigReloadTrigger::Test)
            .expect("reload with controlled restart");

        assert_eq!(
            *audit_events.lock(),
            vec![
                "config.reload.prepared",
                "config.supervisor.restart_started",
                "config.supervisor.restart_completed",
                "config.reload.applied",
            ]
        );
    }

    /// PMMD-g (N-8): the revision `reload_from_disk` publishes with the
    /// desired configuration is the one of the files it loaded, not of a
    /// write that landed after the load.
    /// D-074 P1-1: the reload rebuilds the active generation through a JSON
    /// round trip (`preserve_deferred_fields`); configured values and the
    /// schema version survive the reload.
    #[test]
    fn a_reload_keeps_the_decision_engine_settings() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, CAUSAL_FIXTURE).expect("test: causal config");
        let initial = Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load");
        let manager = ConfigGenerationManager::new(initial);
        std::fs::write(
            &config_path,
            CAUSAL_FIXTURE.replace("default_temperature = 0.7", "default_temperature = 0.4"),
        )
        .expect("test: edited config");

        manager
            .reload_from_disk(ConfigReloadTrigger::Test)
            .expect("test: reload");
        assert!((manager.load_full().default_temperature - 0.4).abs() < f64::EPSILON);

        for config in [manager.load_full(), Arc::clone(&manager.desired().config)] {
            let engine = &config.decision_engine;
            assert_eq!(engine.latency_budget_ms, 3_000);
            assert!((engine.token_ratio_limit - 0.5).abs() < f32::EPSILON);
            assert_eq!(engine.max_candidates, 5);
        }
    }

    /// D-079 L-4: a hand edit that turns remote decisions on is found, so
    /// the reload can warn; a local edit is not.
    #[test]
    fn a_hand_edited_remote_enablement_is_reported() {
        use crate::config::schema::decision_engine::BackendSelection;
        let old = Config::default();
        let mut new = old.clone();
        new.decision_engine.backend.get_or_insert_with(Default::default).kind =
            BackendSelection::Fixed(crate::config::schema::decision_engine::BackendKind::CompatibleHttp);
        new.decision_engine
            .backend
            .get_or_insert_with(Default::default)
            .endpoint = "https://decisions.example.com/v1".to_string();
        new.decision_engine
            .backend
            .get_or_insert_with(Default::default)
            .remote_enabled = true;
        assert_eq!(
            hand_edited_remote_decisions(&old, &new),
            vec!["decision_engine.backend.remote_enabled".to_string()]
        );
        let mut local = old.clone();
        local.decision_engine.enabled = true;
        assert!(
            hand_edited_remote_decisions(&old, &local).is_empty(),
            "expected an empty value"
        );
    }

    /// D-082 N-4: the hand-edit warning is logged only when the reload is
    /// applied; a refused reload logs nothing of it.
    /// D-059: the configured-model allowlist follows the applied
    /// generation: a model named only by the new configuration becomes a
    /// label after the reload, and one no longer named becomes `unlisted`.
    /// The manager refreshes its own allowlist here, so no other test can
    /// race it.
    #[test]
    fn the_model_allowlist_follows_the_applied_generation() {
        use crate::observability::labels::{ModelAllowlist, UNLISTED_MODEL};
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "default_temperature = 0.7\n\n[decision_engine.backend]\nmodel = \"vendor/before-reload\"\n",
        )
        .expect("test: config");
        let initial = Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load");
        let allowlist = Arc::new(ModelAllowlist::default());
        let manager = ConfigGenerationManager::with_model_allowlist(initial, Arc::clone(&allowlist));
        // A participant that rebuilds, so every edit is applied rather than
        // deferred to a restart.
        let events = Arc::new(Mutex::new(Vec::new()));
        let rebuilding = participant("decision", events, false, false);
        manager.register_participant(&rebuilding);
        assert_eq!(allowlist.label("vendor/before-reload"), "vendor/before-reload");
        assert_eq!(allowlist.label("vendor/after-reload"), UNLISTED_MODEL);
        std::fs::write(
            &config_path,
            "default_temperature = 0.7\n\n[decision_engine.backend]\nmodel = \"vendor/after-reload\"\n",
        )
        .expect("test: edit");
        manager
            .reload_from_disk(ConfigReloadTrigger::FileWatcher)
            .expect("test: reload");
        assert_eq!(allowlist.label("vendor/after-reload"), "vendor/after-reload");
        assert_eq!(allowlist.label("vendor/before-reload"), UNLISTED_MODEL);
    }

    #[test]
    #[serial]
    fn a_hand_edited_remote_enablement_is_logged_only_when_applied() {
        #[derive(Clone, Default)]
        struct Sink(Arc<parking_lot::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        const REMOTE: &str = "default_temperature = 0.4\n\n[decision_engine.backend]\n\
                              kind = \"compatible_http\"\nendpoint = \"https://decisions.example.com/v1\"\n\
                              model = \"decider-1\"\nremote_enabled = true\n\n[[providers.models]]\n\
                              provider = \"custom:https://decisions.example.com/v1\"\nmodel = \"decider-1\"\n\
                              operations = [\"decision_endpoint\"]\n";
        let run = |fail_prepare: bool| {
            let dir = tempfile::TempDir::new().expect("test: temp dir");
            let config_path = dir.path().join("config.toml");
            std::fs::write(&config_path, "default_temperature = 0.7\n").expect("test: config");
            let initial = Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load");
            let manager = ConfigGenerationManager::new(initial);
            let events = Arc::new(Mutex::new(Vec::new()));
            let refusing = participant("refusing", Arc::clone(&events), fail_prepare, false);
            manager.register_participant(&refusing);
            std::fs::write(&config_path, REMOTE).expect("test: edit");
            let sink = Sink::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(sink.clone())
                .with_ansi(false)
                .finish();
            let result = tracing::subscriber::with_default(subscriber, || {
                // Another test may have cached this callsite as disabled.
                tracing::callsite::rebuild_interest_cache();
                let result = manager.reload_from_disk(ConfigReloadTrigger::FileWatcher);
                // A control line: the capture worked for this run, so a
                // missing warning is a real absence (D-086 C-3).
                tracing::warn!("capture control line");
                result
            });
            let logs = String::from_utf8_lossy(&sink.0.lock()).to_string();
            (result.is_ok(), logs)
        };
        let (applied, logs) = run(false);
        assert!(applied);
        assert!(
            logs.contains("sends decision state off this machine")
                && logs.contains("decision_engine.backend.remote_enabled"),
            "{logs}"
        );
        assert!(
            !logs.contains("decisions.example.com"),
            "the warning names the path, never the endpoint: {logs}"
        );
        let (applied, logs) = run(true);
        assert!(!applied, "the participant refused the reload");
        assert!(logs.contains("capture control line"), "the capture must work: {logs}");
        assert!(!logs.contains("sends decision state off this machine"), "{logs}");
    }

    /// CDP-04 (V6.07 hot-reload half): a hand edit that points the decision
    /// backend at a remote host without `remote_enabled` does not activate;
    /// the running generation is kept.
    #[test]
    fn a_remote_decision_endpoint_without_the_remote_switch_keeps_the_generation() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "default_temperature = 0.7\n").expect("test: config");
        let initial = Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load");
        let manager = ConfigGenerationManager::new(initial);
        let active = manager.active_generation_id();
        std::fs::write(
            &config_path,
            "default_temperature = 0.4\n\n[decision_engine.backend]\nkind = \"compatible_http\"\n\
             endpoint = \"https://decisions.example.com/v1\"\n",
        )
        .expect("test: edit");
        let error = manager
            .reload_from_disk(ConfigReloadTrigger::Test)
            .expect_err("test: refused");
        let message = format!("{error:#}");
        assert!(message.contains("remote_enabled"), "{message}");
        assert!(!message.contains("decisions.example.com"), "{message}");
        assert_eq!(manager.active_generation_id(), active);
        assert!((manager.load_full().default_temperature - 0.7).abs() < f64::EPSILON);
        assert!(manager.load_full().decision_engine.backend.is_none());
    }

    /// CDP-12 (R-CXR P0-B t2): a hand edit to a decision backend the strict
    /// selection refuses (here a model nobody declared for
    /// `decision_endpoint`) leaves the running generation active.
    #[test]
    fn an_invalid_causal_backend_leaves_the_prior_generation_active() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "default_temperature = 0.7\n").expect("test: config");
        let initial = Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load");
        let manager = ConfigGenerationManager::new(initial);
        let events = Arc::new(Mutex::new(Vec::new()));
        let rebuilding = participant("decision", events, false, false);
        manager.register_participant(&rebuilding);
        let active = manager.active_generation_id();
        std::fs::write(
            &config_path,
            "default_temperature = 0.4\n\n[decision_engine.backend]\nkind = \"compatible_http\"\n\
             endpoint = \"http://127.0.0.1:8900/v1\"\nmodel = \"undeclared-model\"\n",
        )
        .expect("test: edit");
        let error = manager
            .reload_from_disk(ConfigReloadTrigger::Test)
            .expect_err("test: refused");
        assert!(format!("{error:#}").contains("decision_engine.backend"), "{error:#}");
        assert_eq!(manager.active_generation_id(), active);
        assert!((manager.load_full().default_temperature - 0.7).abs() < f64::EPSILON);

        // Declared for decision_endpoint, the same backend activates.
        std::fs::write(
            &config_path,
            "default_temperature = 0.4\n\n[decision_engine.backend]\nkind = \"compatible_http\"\n\
             endpoint = \"http://127.0.0.1:8900/v1\"\nmodel = \"undeclared-model\"\n\n\
             [[providers.models]]\nprovider = \"custom:http://127.0.0.1:8900/v1\"\nmodel = \"undeclared-model\"\n\
             operations = [\"decision_endpoint\"]\n",
        )
        .expect("test: edit");
        manager
            .reload_from_disk(ConfigReloadTrigger::Test)
            .expect("test: applied");
        assert_ne!(manager.active_generation_id(), active);
    }

    /// D-094 C-1: a daemon started with a decision section the check
    /// refuses starts, and a later reload that does not touch
    /// `[decision_engine]` is applied; one that does is still refused.
    #[test]
    fn an_invalid_decision_section_never_blocks_startup_or_unrelated_reloads() {
        const INVALID: &str = "\n[decision_engine.backend]\nkind = \"compatible_http\"\n\
                               endpoint = \"http://127.0.0.1:8900/v1\"\nmodel = \"undeclared-model\"\n";
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, format!("default_temperature = 0.7\n{INVALID}")).expect("test: config");
        let initial = Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load");
        let manager = ConfigGenerationManager::new(initial);
        let active = manager.active_generation_id();
        // A declaration for another provider: the check runs, finds the
        // section already refused, and does not hold the change back.
        const UNRELATED: &str = "\n[[providers.models]]\nprovider = \"openrouter\"\nmodel = \"vendor/chat-model\"\n\
                                 operations = [\"chat\"]\n";
        std::fs::write(&config_path, format!("default_temperature = 0.4\n{INVALID}{UNRELATED}")).expect("test: edit");
        manager
            .reload_from_disk(ConfigReloadTrigger::Test)
            .expect("test: an unrelated change is applied");
        assert_ne!(manager.active_generation_id(), active);
        assert!((manager.load_full().default_temperature - 0.4).abs() < f64::EPSILON);
        let active = manager.active_generation_id();
        std::fs::write(
            &config_path,
            format!(
                "default_temperature = 0.4\n{}{UNRELATED}",
                INVALID.replace("undeclared-model", "other-model")
            ),
        )
        .expect("test: edit");
        let error = manager
            .reload_from_disk(ConfigReloadTrigger::Test)
            .expect_err("test: a change to the section is refused");
        assert!(format!("{error:#}").contains("decision_engine.backend"), "{error:#}");
        assert_eq!(manager.active_generation_id(), active);
    }

    const CAUSAL_FIXTURE: &str = r#"default_temperature = 0.7

[decision_engine]
latency_budget_ms = 3000
token_ratio_limit = 0.5
max_candidates = 5
"#;

    #[test]
    fn a_reload_publishes_the_revision_of_the_files_it_loaded() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "default_temperature = 0.7\n").expect("test: config");
        let mut initial = Config::default();
        initial.config_path = config_path.clone();
        initial.workspace_dir = dir.path().join("workspace");
        let manager = ConfigGenerationManager::new(initial);
        let loaded_revision = crate::config::files::current_config_revision(&config_path).expect("test: revision");
        let writer_path = config_path.clone();
        *AFTER_SNAPSHOT_HOOK.lock() = Some((
            config_path,
            Box::new(move || {
                std::fs::write(&writer_path, "default_temperature = 0.4\n").expect("test: late write");
            }),
        ));

        manager
            .reload_from_disk(ConfigReloadTrigger::Test)
            .expect("test: reload");

        let desired = manager.desired();
        assert!((desired.config.default_temperature - 0.7).abs() < f64::EPSILON);
        assert_eq!(
            desired.source_revision.fingerprint_sha256, loaded_revision,
            "the desired configuration must carry the revision it was loaded from"
        );
    }
}

#[cfg(test)]
mod pinned_parent_process_binding_tests {
    use super::*;

    fn fixture() -> anyhow::Result<(
        tempfile::TempDir,
        ConfigGeneration,
        Arc<crate::security::runtime_key::DecisionKey>,
    )> {
        let home = tempfile::tempdir()?;
        let config = Config {
            config_path: home.path().join("config.toml"),
            workspace_dir: home.path().join("workspace"),
            ..Config::default()
        };
        std::fs::write(&config.config_path, toml::to_string(&config)?)?;
        let generation = ConfigGeneration {
            id: ConfigGenerationId(7),
            source_revision: ConfigSourceRevision::from_config(&config)?,
            effective: Arc::new(config),
            applied_at: Utc::now(),
            trigger: ConfigReloadTrigger::Test,
            deferred_changes: Arc::from([]),
        };
        let transport = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"prx-parent-binding-fixture");
        let key = crate::security::runtime_key::DecisionKey::from_worker_channel(
            &transport,
            "prx-workspace",
            "prx-run",
            "prx-config",
        )?;
        Ok((home, generation, key))
    }

    #[cfg(unix)]
    #[test]
    fn canonical_config_working_root_survives_signed_worker_restore() -> anyhow::Result<()> {
        let (home, mut generation, key) = fixture()?;
        let root = home.path().join("root");
        std::fs::create_dir(&root)?;
        let alias = home.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias)?;
        let mut stored = generation.effective.as_ref().clone();
        stored.runtime.workdir = Some(alias);
        std::fs::write(&stored.config_path, toml::to_string(&stored)?)?;
        let parent = Config::load_from_path(&stored.config_path, stored.workspace_dir.clone())?;
        generation.source_revision = ConfigSourceRevision::from_config(&parent)?;
        generation.effective = Arc::new(parent);
        let binding = generation.bind_process(&key)?;
        let canonical = root.canonicalize()?;
        assert_eq!(
            binding.working_root.as_ref(),
            Some(&canonical),
            "test: signed carrier uses the canonical parent value"
        );
        generation.verify_pinned_process_binding(&binding, &key)?;
        let mut worker = Config::load_from_path(&stored.config_path, stored.workspace_dir)?;
        worker.runtime.workdir = binding.working_root.clone();
        let restored = ConfigGeneration::restore_process(worker, &binding, &key)?;
        assert_eq!(
            restored.effective.runtime.workdir.as_ref(),
            Some(&canonical),
            "test: worker and parent authenticate the same canonical root"
        );
        Ok(())
    }

    #[test]
    fn working_root_binding_rejects_worker_widening_and_tampering() -> anyhow::Result<()> {
        let (home, mut generation, key) = fixture()?;
        let root = home.path().join("root");
        std::fs::create_dir(&root)?;
        Arc::make_mut(&mut generation.effective).runtime.workdir = Some(root.clone());
        let binding = generation.bind_process(&key)?;
        assert_eq!(
            binding.working_root.as_ref(),
            Some(&root),
            "test: parent root is carried"
        );
        ConfigGeneration::restore_process(generation.effective.as_ref().clone(), &binding, &key)?;
        for candidate in [None, Some(home.path().to_path_buf())] {
            let mut child = generation.effective.as_ref().clone();
            child.runtime.workdir = candidate;
            let error = ConfigGeneration::restore_process(child, &binding, &key)
                .expect_err("test: worker cannot widen or discard signed parent root");
            assert!(
                error.downcast_ref::<ProcessWorkingRootMismatch>().is_some(),
                "test: root mismatch is a typed refusal"
            );
        }
        let mut tampered = binding;
        tampered.working_root = Some(home.path().to_path_buf());
        assert!(
            generation.verify_pinned_process_binding(&tampered, &key).is_err(),
            "test: carrier cannot differ from pinned parent root"
        );
        Ok(())
    }

    #[test]
    fn pinned_parent_binding_survives_removed_source_but_child_restore_rejects() -> anyhow::Result<()> {
        let (_home, generation, key) = fixture()?;
        let binding = generation.bind_process(&key)?;
        generation.verify_pinned_process_binding(&binding, &key)?;
        ConfigGeneration::restore_process(generation.effective.as_ref().clone(), &binding, &key)?;
        std::fs::remove_file(&generation.effective.config_path)?;
        generation.verify_pinned_process_binding(&binding, &key)?;
        assert!(ConfigGeneration::restore_process(generation.effective.as_ref().clone(), &binding, &key).is_err());
        Ok(())
    }

    #[test]
    fn pinned_parent_binding_survives_changed_source_but_child_restore_rejects() -> anyhow::Result<()> {
        let (_home, generation, key) = fixture()?;
        let binding = generation.bind_process(&key)?;
        std::fs::write(&generation.effective.config_path, "default_temperature = 0.25\n")?;
        generation.verify_pinned_process_binding(&binding, &key)?;
        assert!(ConfigGeneration::restore_process(generation.effective.as_ref().clone(), &binding, &key).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn process_binding_accepts_a_filesystem_alias_for_the_same_source() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;

        let (home, generation, key) = fixture()?;
        std::fs::create_dir_all(&generation.effective.workspace_dir)?;
        let binding = generation.bind_process(&key)?;
        let alias = home.path().join("source-alias");
        symlink(home.path(), &alias)?;
        let mut child = generation.effective.as_ref().clone();
        child.config_path = alias.join("config.toml");
        child.workspace_dir = alias.join("workspace");

        let restored = ConfigGeneration::restore_process(child, &binding, &key)?;
        assert_eq!(restored.id, generation.id);
        assert_eq!(restored.source_revision, generation.source_revision);
        Ok(())
    }

    #[test]
    fn pinned_parent_binding_rejects_metadata_snapshot_paths_and_key_mutation() -> anyhow::Result<()> {
        let (_home, generation, key) = fixture()?;
        let binding = generation.bind_process(&key)?;
        let mut changed = generation.clone();
        changed.id = ConfigGenerationId(8);
        assert!(changed.verify_pinned_process_binding(&binding, &key).is_err());
        let mut changed = generation.clone();
        changed.source_revision.disk_generation = None;
        assert!(changed.verify_pinned_process_binding(&binding, &key).is_err());
        let mut changed = generation.clone();
        changed.source_revision.fingerprint_sha256 = "0".repeat(64);
        assert!(changed.verify_pinned_process_binding(&binding, &key).is_err());
        let mut changed = generation.clone();
        changed.applied_at += chrono::Duration::seconds(1);
        assert!(changed.verify_pinned_process_binding(&binding, &key).is_err());
        let mut changed = generation.clone();
        changed.trigger = ConfigReloadTrigger::Startup;
        assert!(changed.verify_pinned_process_binding(&binding, &key).is_err());
        let mut changed = generation.clone();
        changed.deferred_changes = vec![DeferredConfigChange {
            field: "prx-field".to_owned(),
            reason: "prx-reason".to_owned(),
        }]
        .into();
        assert!(changed.verify_pinned_process_binding(&binding, &key).is_err());
        for field in ["workspace", "config", "effective"] {
            let mut changed = generation.clone();
            let config = Arc::make_mut(&mut changed.effective);
            match field {
                "workspace" => config.workspace_dir = std::path::PathBuf::from("/prx-other-workspace"),
                "config" => config.config_path = std::path::PathBuf::from("/prx-other-config"),
                _ => config.default_temperature += 0.1,
            }
            assert!(changed.verify_pinned_process_binding(&binding, &key).is_err());
        }
        let transport = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"prx-wrong-binding-key");
        let wrong = crate::security::runtime_key::DecisionKey::from_worker_channel(
            &transport,
            "prx-workspace",
            "prx-run",
            "prx-config",
        )?;
        assert!(generation.verify_pinned_process_binding(&binding, &wrong).is_err());
        let transport = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"prx-parent-binding-fixture");
        for (workspace, run, config) in [
            ("prx-other-workspace", "prx-run", "prx-config"),
            ("prx-workspace", "prx-other-run", "prx-config"),
            ("prx-workspace", "prx-run", "prx-other-config"),
        ] {
            let different_scope =
                crate::security::runtime_key::DecisionKey::from_worker_channel(&transport, workspace, run, config)?;
            assert!(
                generation
                    .verify_pinned_process_binding(&binding, &different_scope)
                    .is_err()
            );
        }
        let mut forged = binding;
        forged.seal = [0; 32];
        assert!(generation.verify_pinned_process_binding(&forged, &key).is_err());
        Ok(())
    }
}
