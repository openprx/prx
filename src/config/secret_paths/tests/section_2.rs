    /// A redacted label posted or written back is a mask: it must be found by
    /// the scan that refuses masks, never stored as a provider.
    #[test]
    fn a_redacted_endpoint_label_counts_as_a_mask() {
        let tree: toml::Value = toml::from_str(
            "default_provider = \"custom:http://[redacted]@127.0.0.1:1/v1\"\n\
             [reliability]\nfallback_providers = [\"openrouter\", \"custom:http://[redacted]@h.invalid/v1\"]\n",
        )
        .expect("test: masked tree parses");
        let masked: BTreeSet<String> = scan(&tree).masked.into_iter().collect();
        assert_eq!(
            masked,
            BTreeSet::from([
                "default_provider".to_string(),
                "reliability.fallback_providers[1]".to_string()
            ])
        );
        // Userinfo is a secret value for scrubbing messages, and a file that
        // holds one is served masked.
        let raw: toml::Value = toml::from_str(USERINFO_TOML).expect("test: userinfo fixture parses");
        let found = scan(&raw);
        assert!(found.has_secret_values());
        assert!(
            !found
                .scrub("rejected custom:http://alice:userinfo-sentinel-default@127.0.0.1:1/v1")
                .contains("userinfo-sentinel")
        );
    }

    /// R-C2BU F-3: the memory embedding provider also takes a `custom:` URL.
    #[test]
    fn config_show_redacts_url_userinfo_in_the_memory_embedding_provider() {
        let mut tree: toml::Value = toml::from_str(
            "[memory]\nembedding_provider = \"custom:https://u:userinfo-sentinel-embed@embed.invalid/v1\"\n",
        )
        .expect("test: fixture parses");
        let found = scan(&tree);
        assert!(
            found.has_secret_values(),
            "the userinfo must be a secret value for scrubbing"
        );
        redact(&mut tree);
        let rendered = tree.to_string();
        assert!(!rendered.contains("userinfo-sentinel"), "{rendered}");
        assert!(
            rendered.contains("custom:https://[redacted]@embed.invalid/v1"),
            "{rendered}"
        );
    }

    /// D-027: two credential-bearing `[cost].prices` keys that redact to the
    /// same spelling are a configuration error. The error names the keys in
    /// their redacted form only.
    #[test]
    fn colliding_credentialed_price_keys_fail_validation_without_printing_credentials() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "default_temperature = 0.7\n\n[cost.prices.\"custom:http://alice:pwA-d027@h.invalid/v1/m\"]\ninput = 1.0\noutput = 1.0\n\n\
             [cost.prices.\"custom:http://bob:pwB-d027@h.invalid/v1/m\"]\ninput = 9.0\noutput = 9.0\n",
        )
        .expect("test: write config");
        let error = crate::config::Config::load_from_path(&path, dir.path().join("workspace"))
            .expect_err("test: colliding price keys must not load");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("custom:http://[redacted]@h.invalid/v1/m"),
            "{rendered}"
        );
        assert!(rendered.contains("cost.prices"), "{rendered}");
        assert!(
            !rendered.contains("pwA-d027") && !rendered.contains("pwB-d027"),
            "{rendered}"
        );
        assert!(!rendered.contains("alice") && !rendered.contains("bob"), "{rendered}");

        // Distinct endpoints, or one credentialed key, still load.
        std::fs::write(
            &path,
            "default_temperature = 0.7\n\n[cost.prices.\"custom:http://alice:pwA-d027@h.invalid/v1/m\"]\ninput = 1.0\noutput = 1.0\n\n\
             [cost.prices.\"custom:http://bob:pwB-d027@other.invalid/v1/m\"]\ninput = 9.0\noutput = 9.0\n",
        )
        .expect("test: write config");
        crate::config::Config::load_from_path(&path, dir.path().join("workspace"))
            .expect("test: distinct endpoints load");
    }

    /// The `{:?}` of every table-valued section of `config`, by its key.
    fn section_debug(config: &crate::config::Config) -> Vec<(&'static str, String)> {
        macro_rules! sections {
            ($($field:ident),* $(,)?) => {
                vec![$((stringify!($field), format!("{:?}", config.$field))),*]
            };
        }
        sections!(
            providers,
            model_settings,
            observability,
            autonomy,
            runtime,
            reliability,
            scheduler,
            agent,
            sessions_spawn,
            chat,
            self_system,
            skills,
            skill_rag,
            plugins,
            skills_lifecycle,
            model_routes,
            embedding_routes,
            query_classification,
            task_routing,
            router,
            smart_group,
            heartbeat,
            xin,
            cron,
            channels_config,
            memory,
            identity_bindings,
            user_policies,
            storage,
            tunnel,
            gateway,
            webhook,
            composio,
            mcp,
            mcp_server,
            a2a,
            auth,
            secrets,
            browser,
            http_request,
            multimodal,
            web_search,
            proxy,
            identity,
            cost,
            nodes,
            agents,
            media,
            decision_engine,
            security,
            compliance,
            tool_tiering,
        )
    }

    /// Provider labels with URL userinfo at the label paths the endpoint
    /// fixture leaves out.
    const LABEL_USERINFO_TOML: &str = r#"
[[router.models]]
provider = "custom:http://router:sentinel-label-router@127.0.0.1:31/v1"
model_id = "m"

[[providers.models]]
provider = "custom:http://models:sentinel-label-models@127.0.0.1:32/v1"
model = "m"

[[providers.tool_dialects]]
provider = "custom:http://dialect:sentinel-label-dialect@127.0.0.1:33/v1"
tool_schema = "raw"

[memory]
embedding_provider = "custom:http://embed:sentinel-label-memory@127.0.0.1:34/v1"

[smart_group]
classifier_provider = "custom:http://group:sentinel-label-group@127.0.0.1:35/v1"

[[model_routes]]
hint = "fast"
provider = "custom:http://route:sentinel-label-route@127.0.0.1:36/v1"
model = "m"

[reliability]
fallback_providers = ["custom:http://fallback:sentinel-label-fallback@127.0.0.1:37/v1"]

[cost.prices."custom:http://price:sentinel-label-price@127.0.0.1:38/v1/m"]
input = 1.0
output = 1.0
"#;

    /// The display path of every table in `tree` that has a key carrying a
    /// fixture sentinel (the value leaves alone miss a secret in a key).
    fn sentinel_key_owners(tree: &toml::Value) -> BTreeSet<String> {
        fn walk(node: &toml::Value, path: &mut Vec<PathElem>, out: &mut BTreeSet<String>) {
            match node {
                toml::Value::Table(table) => {
                    if table.keys().any(|key| key.contains(fixture::SENTINEL_PREFIX)) {
                        out.insert(display_path(path));
                    }
                    for (key, child) in table {
                        path.push(PathElem::Key(key.clone()));
                        walk(child, path, out);
                        path.pop();
                    }
                }
                toml::Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        path.push(PathElem::Index(index));
                        walk(child, path, out);
                        path.pop();
                    }
                }
                _ => {}
            }
        }
        let mut out = BTreeSet::new();
        walk(tree, &mut Vec::new(), &mut out);
        out
    }

    /// `{:?}` of the sub-struct that owns the value at `owner` (the display
    /// path of a sentinel's parent), or `None` when no probe is written for it.
    fn owner_debug(config: &crate::config::Config, owner: &str) -> Option<String> {
        fn d(value: &dyn std::fmt::Debug) -> String {
            format!("{value:?}")
        }
        let c = config;
        let channels = &c.channels_config;
        Some(match owner {
            "" => d(c),
            "agents.worker" => d(c.agents.get("worker")?),
            "model_routes[0]" => d(c.model_routes.first()?),
            "embedding_routes[0]" => d(c.embedding_routes.first()?),
            "reliability" | "reliability.api_keys" | "reliability.fallback_providers" => d(&c.reliability),
            "composio" => d(&c.composio),
            "browser.computer_use" => d(&c.browser.computer_use),
            "web_search" => d(&c.web_search),
            "storage.provider.config" => d(&c.storage.provider.config),
            "gateway" | "gateway.paired_tokens" => d(&c.gateway),
            "mcp.servers.local" | "mcp.servers.local.env" => d(c.mcp.servers.get("local")?),
            "mcp.servers.remote" => d(c.mcp.servers.get("remote")?),
            "channels_config.telegram" => d(channels.telegram.as_ref()?),
            "channels_config.discord" => d(channels.discord.as_ref()?),
            "channels_config.slack" => d(channels.slack.as_ref()?),
            "channels_config.mattermost" => d(channels.mattermost.as_ref()?),
            "channels_config.dingtalk" => d(channels.dingtalk.as_ref()?),
            "channels_config.email" => d(channels.email.as_ref()?),
            "channels_config.irc" => d(channels.irc.as_ref()?),
            "channels_config.lark" => d(channels.lark.as_ref()?),
            "channels_config.linq" => d(channels.linq.as_ref()?),
            "channels_config.nextcloud_talk" => d(channels.nextcloud_talk.as_ref()?),
            "channels_config.qq" => d(channels.qq.as_ref()?),
            "channels_config.wacli" => d(channels.wacli.as_ref()?),
            "channels_config.webhook" => d(channels.webhook.as_ref()?),
            "channels_config.whatsapp" => d(channels.whatsapp.as_ref()?),
            "channels_config.signal" => d(channels.signal.as_ref()?),
            "webhook" => d(&c.webhook),
            "tunnel.cloudflare" => d(c.tunnel.cloudflare.as_ref()?),
            "tunnel.ngrok" => d(c.tunnel.ngrok.as_ref()?),
            "tunnel.custom" => d(c.tunnel.custom.as_ref()?),
            "chat.daemon" => d(&c.chat.daemon),
            "decision_engine.backend" => d(c.decision_engine.backend.as_ref()?),
            "nodes.nodes[0]" => d(c.nodes.nodes.first()?),
            "nodes.server" => d(&c.nodes.server),
            "proxy" => d(&c.proxy),
            "observability" => d(&c.observability),
            "media" => d(&c.media),
            "mcp_server" => d(&c.mcp_server),
            "a2a" => d(&c.a2a),
            "router.models[0]" => d(c.router.models.first()?),
            "providers.models[0]" => d(c.providers.models.first()?),
            "providers.tool_dialects[0]" => d(c.providers.tool_dialects.first()?),
            "memory" => d(&c.memory),
            "smart_group" => d(&c.smart_group),
            "cost.prices" => d(&c.cost),
            _ => return None,
        })
    }

    /// INT-2B a40 (R-PMMAf sub-struct Debug), RED before: the configuration
    /// sub-structs derived `Debug`, so `{:?}` of a model route, a channel
    /// config or a delegate agent printed its key, and of an endpoint its URL
    /// password. After: for every sentinel of the registry fixture, the
    /// endpoint fixture and a provider-label fixture, `{:?}` of the sub-struct
    /// that owns it prints no sentinel (a sentinel whose owner has no probe
    /// fails the test), and neither does `{:?}` of any table-valued section.
    #[test]
    fn no_configuration_struct_debug_prints_a_secret() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let mut unprobed = BTreeSet::new();
        let mut leaks = BTreeSet::new();
        for overlay in [fixture::TOML, fixture::ENDPOINT_USERINFO_TOML, LABEL_USERINFO_TOML] {
            let config = fixture::config_from_overlay(dir.path(), overlay);
            let tree: toml::Value = toml::from_str(overlay).expect("test: overlay parses");
            let sentinels = fixture::string_leaves(&tree, &|value| value.contains(fixture::SENTINEL_PREFIX));
            assert!(!sentinels.is_empty());
            // A secret in a map key (a credentialed `cost.prices` key) is
            // owned by the struct that holds the map.
            let key_owners = sentinel_key_owners(&tree);
            let owners = sentinels
                .into_iter()
                .map(|path| {
                    let mut owner = path;
                    if matches!(owner.pop(), Some(PathElem::Index(_))) {
                        owner.pop();
                    }
                    display_path(&owner)
                })
                .chain(key_owners);
            for owner in owners {
                match owner_debug(&config, &owner) {
                    Some(rendered) if rendered.contains(fixture::SENTINEL_PREFIX) => {
                        leaks.insert(owner);
                    }
                    Some(_) => {}
                    None => {
                        unprobed.insert(owner);
                    }
                }
            }

            let sections = section_debug(&config);
            let serialized = serde_json::to_value(&config).expect("test: serialize");
            let probed: BTreeSet<&str> = sections.iter().map(|(name, _)| *name).collect();
            for (key, value) in serialized.as_object().expect("test: config is an object") {
                if value.is_object() || value.is_array() {
                    assert!(probed.contains(key.as_str()), "section `{key}` has no Debug probe");
                }
            }
            for (name, rendered) in &sections {
                if rendered.contains(fixture::SENTINEL_PREFIX) {
                    leaks.insert((*name).to_string());
                }
            }
        }
        assert!(unprobed.is_empty(), "add a Debug probe for: {unprobed:?}");
        assert!(leaks.is_empty(), "these print a secret with {{:?}}: {leaks:?}");
    }

    /// INT-2B a41 (R-PMMAf R5, D-056 DC-4), RED before: a commented-out
    /// credential was found only as `name = value` with the exact credential
    /// name, so `# api-key = …`, `# apikey = …` and `# token: …` were served
    /// verbatim. After: `-` reads as `_`, `apikey` counts, and `:` separates
    /// like `=`. Ordinary comments still do not count.
    #[test]
    fn a_commented_credential_is_found_in_its_common_spellings() {
        for text in [
            "# api-key = \"sk-old\"",
            "# apikey = \"sk-old\"",
            "# OpenAI-ApiKey = \"sk-old\"",
            "# api_key: sk-old",
            "#token: abc",
            "model = \"m\" # bot-token: abc",
            "# Password = hunter2",
            "# api_key = \"sk-old\"",
            "# note: api_key = \"sk-old\"",
        ] {
            assert!(has_commented_secret(text), "{text:?} must count as a commented secret");
        }
        for text in [
            "# see the API documentation",
            "# key points: none",
            "# timeout = 5",
            "# model: gpt-4o",
            "# http://example.invalid/a=b",
            "default_model = \"m\"",
        ] {
            assert!(!has_commented_secret(text), "{text:?} is an ordinary comment");
        }
    }
