    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn one_scrubber_masks_by_value_and_by_shape_with_or_without_a_scan() {
        let message = "bad key sk-proj-abcdefghijklmnop12345 near sentinel-route";
        let tree: toml::Value = toml::from_str(
            "[[model_routes]]\nhint = \"fast\"\nprovider = \"p\"\nmodel = \"m\"\napi_key = \"sentinel-route\"\n",
        )
        .expect("test: parse");
        let found = scan(&tree);
        let with_scan = scrub_message(Some(&found), message);
        assert!(!with_scan.contains("sk-proj-abcdefghijklmnop12345"), "{with_scan}");
        assert!(!with_scan.contains("sentinel-route"), "{with_scan}");
        let without_scan = scrub_message(None, message);
        assert!(
            !without_scan.contains("sk-proj-abcdefghijklmnop12345"),
            "{without_scan}"
        );
        assert_eq!(found.scrub(message), with_scan);
    }

    fn fixture_tree() -> toml::Value {
        toml::from_str(fixture::TOML).expect("test: fixture parses")
    }

    fn sentinel_paths() -> BTreeSet<Vec<PathElem>> {
        fixture::string_leaves(&fixture_tree(), &|value| value.contains(fixture::SENTINEL_PREFIX))
    }

    /// Resolve `$ref` and `anyOf`/`oneOf`/`allOf` alternatives of a schema node.
    fn alternatives<'a>(root: &'a serde_json::Value, node: &'a serde_json::Value) -> Vec<&'a serde_json::Value> {
        if let Some(reference) = node.get("$ref").and_then(serde_json::Value::as_str) {
            let name = reference.rsplit('/').next().unwrap_or_default();
            return root
                .get("$defs")
                .and_then(|defs| defs.get(name))
                .map(|target| alternatives(root, target))
                .unwrap_or_default();
        }
        let mut found = vec![node];
        for combinator in ["anyOf", "oneOf", "allOf"] {
            if let Some(options) = node.get(combinator).and_then(serde_json::Value::as_array) {
                for option in options {
                    found.extend(alternatives(root, option));
                }
            }
        }
        found
    }

    fn step<'a>(
        root: &'a serde_json::Value,
        node: &'a serde_json::Value,
        segment: Segment,
    ) -> Option<&'a serde_json::Value> {
        alternatives(root, node)
            .into_iter()
            .find_map(|candidate| match segment {
                Segment::Key(key) => candidate.get("properties").and_then(|properties| properties.get(key)),
                Segment::AnyKey => candidate
                    .get("additionalProperties")
                    .filter(|schema| schema.is_object()),
                Segment::AnyIndex => candidate.get("items"),
            })
    }

    fn admits_string(root: &serde_json::Value, node: &serde_json::Value) -> bool {
        alternatives(root, node)
            .into_iter()
            .any(|candidate| match candidate.get("type") {
                Some(serde_json::Value::String(kind)) => kind == "string",
                Some(serde_json::Value::Array(kinds)) => kinds.iter().any(|kind| kind == "string"),
                _ => false,
            })
    }

    /// Every registered pattern names a real string slot of `Config`. A typo
    /// in the registry would otherwise silently protect nothing.
    #[test]
    fn every_registered_path_names_a_string_slot_of_the_config_schema() {
        let root = serde_json::to_value(schemars::schema_for!(crate::config::Config)).expect("test: schema serializes");
        // The JSON schema lists canonical names only; aliases have their
        // own guard below.
        for secret in SECRET_PATHS.iter().filter(|secret| secret.alias_of.is_none()) {
            let leaf = secret
                .segments
                .iter()
                .try_fold(&root, |node, segment| step(&root, node, *segment));
            let leaf = leaf.unwrap_or_else(|| panic!("registered path {} is not in the Config schema", secret.pattern));
            assert!(
                admits_string(&root, leaf),
                "registered path {} does not hold a string",
                secret.pattern
            );
        }
    }

    /// The fixture really covers the registry: each registered pattern has at
    /// least one sentinel instance, and every sentinel sits at a secret path.
    #[test]
    fn fixture_holds_a_sentinel_at_every_registered_path() {
        let sentinels = sentinel_paths();
        // One fixture cannot hold a field and its alias at once.
        for secret in SECRET_PATHS.iter().filter(|secret| secret.alias_of.is_none()) {
            assert!(
                sentinels.iter().any(|path| pattern_matches(secret.segments, path)),
                "the fixture has no sentinel at {}",
                secret.pattern
            );
        }
        for path in &sentinels {
            assert!(
                registered(path).is_some(),
                "sentinel at {} is not registered",
                display_path(path)
            );
        }
    }

    /// Guard: the registry's encrypted set is exactly what `Config::save`
    /// encrypts, through the real save path on a temp config dir.
    #[tokio::test]
    async fn registry_encrypted_set_is_exactly_what_save_encrypts() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config = fixture::config_in(dir.path());
        config.save().await.expect("test: save fixture");

        let stored: toml::Value =
            toml::from_str(&std::fs::read_to_string(&config.config_path).expect("test: read stored config"))
                .expect("test: stored config parses");
        let store = crate::security::SecretStore::new(dir.path(), true);

        let encrypted_on_disk = fixture::string_leaves(&stored, &crate::security::SecretStore::is_encrypted);
        let expected_encrypted: BTreeSet<Vec<PathElem>> = sentinel_paths()
            .into_iter()
            .filter(|path| registered(path).is_some_and(|secret| secret.handling == SecretHandling::EncryptedAtRest))
            .collect();
        assert_eq!(
            encrypted_on_disk
                .iter()
                .map(|path| display_path(path))
                .collect::<Vec<_>>(),
            expected_encrypted
                .iter()
                .map(|path| display_path(path))
                .collect::<Vec<_>>(),
            "save must encrypt exactly the registered encrypted-at-rest paths"
        );

        let fixture = fixture_tree();
        for path in &expected_encrypted {
            let stored_value = lookup(&stored, path).expect("test: encrypted path stored");
            let plaintext = lookup(&fixture, path).expect("test: fixture value");
            assert_eq!(
                store.decrypt(stored_value).expect("test: decrypt stored value"),
                plaintext,
                "{} must decrypt to what was saved",
                display_path(path)
            );
        }
    }

    fn lookup<'a>(root: &'a toml::Value, path: &[PathElem]) -> Option<&'a str> {
        path.iter()
            .try_fold(root, |node, elem| match elem {
                PathElem::Key(key) => node.get(key.as_str()),
                PathElem::Index(index) => node.get(*index),
            })
            .and_then(toml::Value::as_str)
    }

    /// `prx config show` renders the stored form with every registered path
    /// masked and no sentinel left.
    #[test]
    fn config_show_rendering_masks_every_registered_path() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config = fixture::config_in(dir.path());
        let rendered = redacted_stored_toml(&config).expect("test: render");
        let text = toml::to_string_pretty(&rendered).expect("test: serialize rendering");
        assert!(
            !text.contains(fixture::SENTINEL_PREFIX),
            "config show leaked a sentinel:\n{text}"
        );

        let masked = fixture::masked_paths(&rendered);
        for path in sentinel_paths() {
            assert!(
                masked.contains(&display_path(&path)),
                "{} is not masked by config show",
                display_path(&path)
            );
        }
    }

    // ── SECRETS-OTHER: every registered secret is encrypted at rest ──────

    /// RED before SECRETS-OTHER: with `secrets.encrypt = true`, channel and
    /// bot tokens, paired tokens, MCP environment values, webhook, tunnel,
    /// daemon and node credentials were stored as plaintext. After: every
    /// sentinel of the fixture is `enc2:` on disk, and the load restores it.
    #[tokio::test]
    async fn save_encrypts_every_registered_secret_and_load_restores_it() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config = fixture::config_in(dir.path());
        config.save().await.expect("test: save fixture");
        let (raw, stored) = stored_tree(&config);
        assert!(
            !raw.contains(fixture::SENTINEL_PREFIX),
            "a secret is stored as plaintext:\n{raw}"
        );
        for path in sentinel_paths() {
            let value = lookup(&stored, &path).unwrap_or_else(|| panic!("{} is not stored", display_path(&path)));
            assert!(
                crate::security::SecretStore::is_secure_encrypted(value),
                "{} is not enc2: on disk",
                display_path(&path)
            );
        }

        let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir.clone())
            .expect("test: load saved fixture");
        let loaded = toml::Value::try_from(&loaded).expect("test: render loaded");
        let fixture = fixture_tree();
        for path in sentinel_paths() {
            assert_eq!(
                lookup(&loaded, &path),
                lookup(&fixture, &path),
                "{} must load back as written",
                display_path(&path)
            );
        }
    }

    /// Migration (D-018 Q4, no forced migration): a file an older release
    /// wrote holds channel tokens, paired tokens and MCP environment values
    /// as plaintext. They load as they are and are encrypted on the next
    /// save; the values survive every step.
    #[tokio::test]
    async fn plaintext_channel_and_gateway_secrets_from_an_older_release_are_encrypted_on_next_save() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "default_temperature = 0.7\n\n[gateway]\npaired_tokens = [\"sentinel-old-paired\"]\n\n\
             [channels_config]\ncli = true\n\n[channels_config.telegram]\nbot_token = \"sentinel-old-telegram\"\nallowed_users = []\n\n\
             [mcp.servers.local]\ncommand = \"mcp-local\"\nenv = { GITHUB_PAT = \"sentinel-old-mcp\" }\n",
        )
        .expect("test: write legacy config");

        let check = |config: &crate::config::Config| {
            assert_eq!(config.gateway.paired_tokens, vec!["sentinel-old-paired".to_string()]);
            assert_eq!(
                config
                    .channels_config
                    .telegram
                    .as_ref()
                    .map(|telegram| telegram.bot_token.as_str()),
                Some("sentinel-old-telegram")
            );
            assert_eq!(
                config
                    .mcp
                    .servers
                    .get("local")
                    .and_then(|server| server.env.get("GITHUB_PAT"))
                    .map(String::as_str),
                Some("sentinel-old-mcp")
            );
        };
        let loaded = crate::config::Config::load_from_path(&config_path, dir.path().join("workspace"))
            .unwrap_or_else(|error| panic!("test: a plaintext legacy file must still load: {error:#}"));
        check(&loaded);
        loaded.save().await.expect("test: next save");
        let (raw, _) = stored_tree(&loaded);
        assert!(
            !raw.contains(fixture::SENTINEL_PREFIX),
            "a secret survived the next save as plaintext:\n{raw}"
        );
        let reloaded = crate::config::Config::load_from_path(&config_path, dir.path().join("workspace"))
            .expect("test: reload re-encrypted config");
        check(&reloaded);
    }

    /// Guard (downgrade path, D-018 Q4): `secrets.encrypt = false` stores
    /// every registered secret as plaintext, so an older release can read
    /// the file again.
    #[tokio::test]
    async fn disabled_encryption_keeps_every_registered_secret_plaintext() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let mut config = fixture::config_in(dir.path());
        config.secrets.encrypt = false;
        config.save().await.expect("test: save fixture");
        let (_, stored) = stored_tree(&config);
        assert!(fixture::string_leaves(&stored, &crate::security::SecretStore::is_encrypted).is_empty());
        let fixture = fixture_tree();
        for path in sentinel_paths() {
            assert_eq!(
                lookup(&stored, &path),
                lookup(&fixture, &path),
                "{}",
                display_path(&path)
            );
        }
    }

    // ── A-2: every provider credential slot is encrypted at rest ──────────

    const ROUTE_SECRET: &str = "sentinel-route-secret";
    const EMBEDDING_SECRET: &str = "sentinel-embedding-secret";
    const ROTATION_SECRETS: [&str; 2] = ["sentinel-rotation-a", "sentinel-rotation-b"];

    fn provider_slots_config(dir: &std::path::Path, route_provider: &str) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.config_path = dir.join("config.toml");
        config.workspace_dir = dir.join("workspace");
        config.model_routes = vec![crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "fast".into(),
            provider: route_provider.into(),
            model: "route-model".into(),
            api_key: Some(ROUTE_SECRET.into()),
            credential_ref: None,
        }];
        config.embedding_routes = vec![crate::config::EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "openai".into(),
            model: "text-embedding-3-small".into(),
            dimensions: None,
            api_key: Some(EMBEDDING_SECRET.into()),
            credential_ref: None,
        }];
        config.reliability.api_keys = ROTATION_SECRETS.iter().map(|key| (*key).to_string()).collect();
        config
    }

    fn stored_tree(config: &crate::config::Config) -> (String, toml::Value) {
        let raw = std::fs::read_to_string(&config.config_path).expect("test: read stored config");
        let tree = toml::from_str(&raw).expect("test: stored config parses");
        (raw, tree)
    }

    fn stored_provider_slots(tree: &toml::Value) -> Vec<String> {
        let mut slots = Vec::new();
        for (section, key) in [("model_routes", "api_key"), ("embedding_routes", "api_key")] {
            if let Some(routes) = tree.get(section).and_then(toml::Value::as_array) {
                slots.extend(
                    routes
                        .iter()
                        .filter_map(|route| route.get(key).and_then(toml::Value::as_str).map(str::to_string)),
                );
            }
        }
        if let Some(keys) = tree
            .get("reliability")
            .and_then(|reliability| reliability.get("api_keys"))
            .and_then(toml::Value::as_array)
        {
            slots.extend(keys.iter().filter_map(toml::Value::as_str).map(str::to_string));
        }
        slots
    }

    /// RED before A-2: `Config::save` wrote route, embedding-route and
    /// rotation keys to disk as plaintext on every write path.
    #[tokio::test]
    async fn save_encrypts_route_embedding_and_rotation_keys() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config = provider_slots_config(dir.path(), "openrouter");
        config.save().await.expect("test: save");

        let (raw, tree) = stored_tree(&config);
        for secret in [ROUTE_SECRET, EMBEDDING_SECRET, ROTATION_SECRETS[0], ROTATION_SECRETS[1]] {
            assert!(!raw.contains(secret), "{secret} is stored as plaintext:\n{raw}");
        }
        let stored = stored_provider_slots(&tree);
        assert_eq!(stored.len(), 4, "every slot must still be stored: {stored:?}");
        assert!(
            stored
                .iter()
                .all(|value| crate::security::SecretStore::is_secure_encrypted(value)),
            "every provider slot must be enc2: {stored:?}"
        );

        let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir.clone())
            .expect("test: load saved config");
        assert_eq!(loaded.model_routes[0].api_key.as_deref(), Some(ROUTE_SECRET));
        assert_eq!(loaded.embedding_routes[0].api_key.as_deref(), Some(EMBEDDING_SECRET));
        assert_eq!(loaded.reliability.api_keys, ROTATION_SECRETS.to_vec());
    }

    /// Migration (D-018 Q4): a file an older release wrote holds these keys
    /// as plaintext. The new binary reads them as they are and encrypts them
    /// on the next save; the values survive every step.
    #[tokio::test]
    async fn plaintext_provider_slots_from_an_older_release_load_and_are_encrypted_on_next_save() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config_path = dir.path().join("config.toml");
        let legacy = format!(
            "default_temperature = 0.7\n\n[[model_routes]]\nhint = \"fast\"\nprovider = \"openrouter\"\nmodel = \"route-model\"\napi_key = \"{ROUTE_SECRET}\"\n\n[[embedding_routes]]\nhint = \"semantic\"\nprovider = \"openai\"\nmodel = \"text-embedding-3-small\"\napi_key = \"{EMBEDDING_SECRET}\"\n\n[reliability]\napi_keys = [\"{}\", \"{}\"]\n",
            ROTATION_SECRETS[0], ROTATION_SECRETS[1]
        );
        std::fs::write(&config_path, legacy).expect("test: write legacy config");

        let loaded = crate::config::Config::load_from_path(&config_path, dir.path().join("workspace"))
            .expect("test: a plaintext legacy file must still load");
        assert_eq!(loaded.model_routes[0].api_key.as_deref(), Some(ROUTE_SECRET));
        assert_eq!(loaded.embedding_routes[0].api_key.as_deref(), Some(EMBEDDING_SECRET));
        assert_eq!(loaded.reliability.api_keys, ROTATION_SECRETS.to_vec());

        loaded.save().await.expect("test: next save");
        let (raw, tree) = stored_tree(&loaded);
        for secret in [ROUTE_SECRET, EMBEDDING_SECRET, ROTATION_SECRETS[0], ROTATION_SECRETS[1]] {
            assert!(!raw.contains(secret), "{secret} survived the next save as plaintext");
        }
        assert!(
            stored_provider_slots(&tree)
                .iter()
                .all(|value| crate::security::SecretStore::is_secure_encrypted(value))
        );

        let reloaded = crate::config::Config::load_from_path(&config_path, dir.path().join("workspace"))
            .expect("test: reload re-encrypted config");
        assert_eq!(reloaded.model_routes[0].api_key.as_deref(), Some(ROUTE_SECRET));
        assert_eq!(reloaded.embedding_routes[0].api_key.as_deref(), Some(EMBEDDING_SECRET));
        assert_eq!(reloaded.reliability.api_keys, ROTATION_SECRETS.to_vec());
    }

    /// `secrets.encrypt = false` keeps every slot as plaintext, as before.
    #[tokio::test]
    async fn disabled_encryption_keeps_provider_slots_plaintext() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let mut config = provider_slots_config(dir.path(), "openrouter");
        config.secrets.encrypt = false;
        config.save().await.expect("test: save");
        let (_, tree) = stored_tree(&config);
        assert_eq!(
            stored_provider_slots(&tree),
            vec![
                ROUTE_SECRET.to_string(),
                EMBEDDING_SECRET.to_string(),
                ROTATION_SECRETS[0].to_string(),
                ROTATION_SECRETS[1].to_string()
            ]
        );
    }

    /// A legacy XOR `enc:` route key is decrypted on load and upgraded to
    /// `enc2:` by the startup migration, like the other encrypted slots.
    /// RED before A-2: the route key was neither decrypted nor migrated, so
    /// the literal `enc:` string was the route's bearer credential.
    #[tokio::test]
    async fn legacy_xor_route_key_is_decrypted_and_migrated() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let store = crate::security::SecretStore::new(dir.path(), true);
        let legacy_value = legacy_xor(dir.path(), ROUTE_SECRET);
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "default_temperature = 0.7\n\n[[model_routes]]\nhint = \"fast\"\nprovider = \"openrouter\"\nmodel = \"route-model\"\napi_key = \"{legacy_value}\"\n"
            ),
        )
        .expect("test: write legacy config");

        let loaded = crate::config::Config::load_from_path(&config_path, dir.path().join("workspace"))
            .expect("test: load legacy route key");
        assert_eq!(loaded.model_routes[0].api_key.as_deref(), Some(ROUTE_SECRET));

        crate::config::schema::migrate_config_legacy_secrets(&loaded)
            .await
            .expect("test: legacy migration");
        let (_, tree) = stored_tree(&loaded);
        let stored = stored_provider_slots(&tree);
        assert_eq!(stored.len(), 1);
        assert!(
            crate::security::SecretStore::is_secure_encrypted(&stored[0]),
            "{stored:?}"
        );
        assert_eq!(store.decrypt(&stored[0]).expect("test: decrypt migrated"), ROUTE_SECRET);
    }

    /// RED before PMMH-f (D-101 F-5): with `secrets.encrypt = false` the
    /// startup upgrade rewrote a legacy `enc:` value as plaintext. It is
    /// left as it is now (it keeps loading).
    #[tokio::test]
    async fn a_legacy_value_is_never_rewritten_as_plaintext() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let legacy_value = legacy_xor(dir.path(), ROUTE_SECRET);
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "default_temperature = 0.7\n\n[secrets]\nencrypt = false\n\n[[model_routes]]\nhint = \"fast\"\nprovider = \"openrouter\"\nmodel = \"route-model\"\napi_key = \"{legacy_value}\"\n"
            ),
        )
        .expect("test: write legacy config");
        let before = std::fs::read(&config_path).expect("test: read");

        let loaded = crate::config::Config::load_from_path(&config_path, dir.path().join("workspace"))
            .expect("test: load legacy route key");
        crate::config::schema::migrate_config_legacy_secrets(&loaded)
            .await
            .expect("test: legacy migration");

        let after = std::fs::read(&config_path).expect("test: read");
        assert!(
            !String::from_utf8_lossy(&after).contains(ROUTE_SECRET),
            "the legacy value was rewritten as plaintext"
        );
        assert_eq!(after, before);
        assert_eq!(
            loaded.model_routes.first().and_then(|route| route.api_key.as_deref()),
            Some(ROUTE_SECRET)
        );
    }

    /// A loopback OpenAI-compatible endpoint that answers every chat request
    /// with "ok" and reports each request's `Authorization` header.
    async fn spawn_authorization_recorder() -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<Option<String>>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind loopback");
        let addr = listener.local_addr().expect("test: loopback addr");
        let (seen_tx, seen_rx) = tokio::sync::mpsc::unbounded_channel::<Option<String>>();
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 4096];
                let head_end = loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break None,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                    }
                    if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                        break Some(position + 4);
                    }
                };
                let Some(head_end) = head_end else { continue };
                let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                let authorization = head.split("\r\n").find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.trim().eq_ignore_ascii_case("authorization"))
                        .map(|(_, value)| value.trim().to_string())
                });
                let content_length = head
                    .split("\r\n")
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let mut body_read = buffer.len() - head_end;
                while body_read < content_length {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => body_read += read,
                    }
                }
                let _ = seen_tx.send(authorization);
                let body = r#"{"id":"c","object":"chat.completion","created":1,"model":"route-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        (addr, seen_rx, server)
    }

    fn isolated_provider_options(dir: &std::path::Path) -> crate::providers::ProviderRuntimeOptions {
        crate::providers::ProviderRuntimeOptions {
            openprx_dir: Some(dir.to_path_buf()),
            codex_auth_json_path: Some(dir.join("absent-codex-auth.json")),
            codex_auth_json_auto_import: false,
            ..crate::providers::ProviderRuntimeOptions::default()
        }
    }

    /// The legacy XOR `enc:` form of `plaintext` under the key in `dir`.
    fn legacy_xor(dir: &std::path::Path, plaintext: &str) -> String {
        let store = crate::security::SecretStore::new(dir, true);
        store.encrypt("prime-the-key").expect("test: create key file");
        let key_hex = std::fs::read_to_string(dir.join(".secret_key")).expect("test: key file");
        let key_hex = key_hex.trim();
        let key = (0..key_hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&key_hex[index..index + 2], 16).expect("test: key hex"))
            .collect::<Vec<u8>>();
        format!(
            "enc:{}",
            plaintext
                .bytes()
                .enumerate()
                .map(|(index, byte)| format!("{:02x}", byte ^ key[index % key.len()]))
                .collect::<String>()
        )
    }

    /// F10: on the default split layout the route keys live in
    /// `config.d/routing.toml`. A legacy `enc:` route key there is decrypted
    /// on load (the merged tree is decrypted, fragments included), the routed
    /// provider receives the real key, and the next structured save rewrites
    /// the fragment as `enc2:`. The startup migration still rewrites
    /// `config.toml` only.
    #[tokio::test]
    async fn legacy_route_key_in_a_fragment_reaches_the_provider_and_is_upgraded_on_next_save() {
        let (addr, mut seen_rx, server) = spawn_authorization_recorder().await;
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let route_provider = format!("custom:http://{addr}/v1");
        let mut config = provider_slots_config(dir.path(), &route_provider);
        config.reliability.api_keys = Vec::new();
        config.embedding_routes = Vec::new();
        crate::config::files::write_split_config(&config, false)
            .await
            .expect("test: write split layout");
        let fragment = crate::config::files::config_dir_path(&config.config_path).join("routing.toml");
        let stored = std::fs::read_to_string(&fragment).expect("test: routing fragment");
        assert!(
            !std::fs::read_to_string(&config.config_path)
                .expect("test: main")
                .contains("model_routes")
        );

        // Replace the encrypted route key with its legacy XOR form.
        let mut tree: toml::Value = toml::from_str(&stored).expect("test: fragment parses");
        let slot = tree
            .get_mut("model_routes")
            .and_then(|routes| routes.get_mut(0))
            .and_then(|route| route.get_mut("api_key"))
            .expect("test: route key in the fragment");
        *slot = toml::Value::String(legacy_xor(dir.path(), ROUTE_SECRET));
        std::fs::write(&fragment, toml::to_string_pretty(&tree).expect("test: render")).expect("test: write fragment");

        let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir.clone())
            .expect("test: load split layout");
        assert_eq!(
            loaded.model_routes.first().and_then(|r| r.api_key.as_deref()),
            Some(ROUTE_SECRET)
        );

        let mut runtime_config = loaded.clone();
        runtime_config.default_provider = Some(route_provider.clone());
        runtime_config.default_model = Some("route-model".to_string());
        runtime_config.reliability.provider_retries = 0;
        runtime_config.reliability.fallback_providers = Vec::new();
        let provider =
            crate::providers::create_provider_from_test_config(&runtime_config, &isolated_provider_options(dir.path()))
                .expect("test: routed provider");
        let reply = provider.chat_with_system(None, "ping", "hint:fast", 0.0).await;
        assert_eq!(reply.as_deref().ok(), Some("ok"), "unexpected reply: {reply:?}");
        assert_eq!(
            seen_rx.recv().await.flatten().as_deref(),
            Some(format!("Bearer {ROUTE_SECRET}").as_str())
        );

        loaded.save().await.expect("test: next save");
        let upgraded = std::fs::read_to_string(&fragment).expect("test: fragment after save");
        let upgraded: toml::Value = toml::from_str(&upgraded).expect("test: parses");
        let key = upgraded
            .get("model_routes")
            .and_then(|routes| routes.get(0))
            .and_then(|route| route.get("api_key"))
            .and_then(toml::Value::as_str)
            .expect("test: route key after save");
        assert!(
            crate::security::SecretStore::is_secure_encrypted(key),
            "the next save must upgrade to enc2:"
        );
        server.abort();
    }

    /// RED before PMM-H (PMMA F-10): the startup migration upgraded legacy
    /// `enc:` values in `config.toml` only; one in a `config.d` fragment
    /// stayed XOR-encrypted on disk until a later save. It is now upgraded
    /// at startup, and every other file is left byte for byte.
    #[tokio::test]
    async fn legacy_route_key_in_a_fragment_is_upgraded_at_startup() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let mut config = provider_slots_config(dir.path(), "openrouter");
        config.reliability.api_keys = Vec::new();
        config.embedding_routes = Vec::new();
        crate::config::files::write_split_config(&config, false)
            .await
            .expect("test: write split layout");
        let fragment = crate::config::files::config_dir_path(&config.config_path).join("routing.toml");
        let stored = std::fs::read_to_string(&fragment).expect("test: routing fragment");
        let mut tree: toml::Value = toml::from_str(&stored).expect("test: fragment parses");
        let slot = tree
            .get_mut("model_routes")
            .and_then(|routes| routes.get_mut(0))
            .and_then(|route| route.get_mut("api_key"))
            .expect("test: route key in the fragment");
        *slot = toml::Value::String(legacy_xor(dir.path(), ROUTE_SECRET));
        std::fs::write(&fragment, toml::to_string_pretty(&tree).expect("test: render")).expect("test: write fragment");
        let main_before = std::fs::read(&config.config_path).expect("test: main");

        let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir.clone())
            .expect("test: load split layout");
        crate::config::schema::migrate_config_legacy_secrets(&loaded)
            .await
            .expect("test: startup migration");

        let upgraded: toml::Value =
            toml::from_str(&std::fs::read_to_string(&fragment).expect("test: fragment")).expect("test: parses");
        let key = upgraded
            .get("model_routes")
            .and_then(|routes| routes.get(0))
            .and_then(|route| route.get("api_key"))
            .and_then(toml::Value::as_str)
            .expect("test: route key");
        assert!(
            crate::security::SecretStore::is_secure_encrypted(key),
            "the fragment still holds a legacy value: {key}"
        );
        let store = crate::security::SecretStore::new(dir.path(), true);
        assert_eq!(store.decrypt(key).expect("test: decrypt"), ROUTE_SECRET);
        assert_eq!(std::fs::read(&config.config_path).expect("test: main"), main_before);
        // Nothing left to migrate: a second run writes nothing.
        let fragment_after = std::fs::read(&fragment).expect("test: fragment");
        crate::config::schema::migrate_config_legacy_secrets(&loaded)
            .await
            .expect("test: second run");
        assert_eq!(std::fs::read(&fragment).expect("test: fragment"), fragment_after);
    }

    /// PMM-H: a route that names a credential reference instead of an
    /// inline key sends the referenced key to its own provider, and a
    /// reference issued for another provider is never sent.
    #[tokio::test]
    async fn a_route_credential_ref_reaches_its_owner_provider_only() {
        let (addr, mut seen_rx, server) = spawn_authorization_recorder().await;
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let route_provider = format!("custom:http://{addr}/v1");
        let mut config = provider_slots_config(dir.path(), &route_provider);
        config.reliability.api_keys = Vec::new();
        config.embedding_routes = Vec::new();
        let references =
            crate::config::manage::credentials::CredentialReferences::for_config(&config).expect("test: references");
        references
            .set(&route_provider, "route", ROUTE_SECRET)
            .expect("test: reference for the route's provider");
        references
            .set("anthropic", "other", "sentinel-other-provider-secret")
            .expect("test: reference for another provider");
        config.model_routes = vec![
            crate::config::ModelRouteConfig {
                settings: Default::default(),
                hint: "fast".into(),
                provider: route_provider.clone(),
                model: "route-model".into(),
                api_key: None,
                credential_ref: Some("cred-route".into()),
            },
            crate::config::ModelRouteConfig {
                settings: Default::default(),
                hint: "wrong".into(),
                provider: route_provider.clone(),
                model: "route-model".into(),
                api_key: None,
                credential_ref: Some("cred-other".into()),
            },
        ];
        config.save().await.expect("test: save");
        let stored = std::fs::read_to_string(&config.config_path).expect("test: stored");
        assert!(stored.contains("credential_ref = \"cred-route\""), "{stored}");
        assert!(!stored.contains(ROUTE_SECRET), "{stored}");

        let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir.clone())
            .expect("test: load");
        let routes = loaded.model_routes_with_references();
        assert_eq!(routes[0].api_key.as_deref(), Some(ROUTE_SECRET));
        assert_eq!(routes[1].api_key, None, "another provider's reference was released");

        let mut runtime_config = loaded.clone();
        runtime_config.default_provider = Some(route_provider.clone());
        runtime_config.default_model = Some("route-model".to_string());
        runtime_config.model_routes = routes.to_vec();
        runtime_config.reliability.provider_retries = 0;
        runtime_config.reliability.fallback_providers = Vec::new();
        let provider =
            crate::providers::create_provider_from_test_config(&runtime_config, &isolated_provider_options(dir.path()))
                .expect("test: routed provider");
        let reply = provider.chat_with_system(None, "ping", "hint:fast", 0.0).await;
        assert_eq!(reply.as_deref().ok(), Some("ok"), "unexpected reply: {reply:?}");
        assert_eq!(
            seen_rx.recv().await.flatten().as_deref(),
            Some(format!("Bearer {ROUTE_SECRET}").as_str())
        );
        server.abort();
    }

    /// The route credential reaches its provider as plaintext after the
    /// encrypted round trip: loopback `Authorization` equals the sentinel.
    #[tokio::test]
    async fn routed_provider_built_from_the_loaded_config_presents_the_plaintext_route_key() {
        let (addr, mut seen_rx, server) = spawn_authorization_recorder().await;

        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let route_provider = format!("custom:http://{addr}/v1");
        let mut config = provider_slots_config(dir.path(), &route_provider);
        config.reliability.api_keys = Vec::new();
        config.save().await.expect("test: save");
        let (raw, _) = stored_tree(&config);
        assert!(!raw.contains(ROUTE_SECRET), "the route key must be stored encrypted");

        let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir.clone())
            .expect("test: load");
        let mut runtime_config = loaded.clone();
        runtime_config.default_provider = Some(route_provider.clone());
        runtime_config.default_model = Some("route-model".to_string());
        runtime_config.reliability.provider_retries = 0;
        runtime_config.reliability.fallback_providers = Vec::new();
        let options = isolated_provider_options(dir.path());
        let provider = crate::providers::create_provider_from_test_config(&runtime_config, &options)
            .expect("test: routed provider");
        let reply = provider.chat_with_system(None, "ping", "hint:fast", 0.0).await;
        assert_eq!(reply.as_deref().ok(), Some("ok"), "unexpected reply: {reply:?}");
        assert_eq!(
            seen_rx.recv().await.flatten().as_deref(),
            Some(format!("Bearer {ROUTE_SECRET}").as_str())
        );
        server.abort();
    }

    /// The scan names plaintext at every registered encrypted slot, the
    /// elements of `reliability.api_keys` and `gateway.paired_tokens`
    /// included, and never flags ciphertext.
    #[test]
    fn scan_finds_plaintext_at_encrypted_slots_and_masks_anywhere() {
        let tree: toml::Value = toml::from_str(
            r#"
api_key = "enc2:00"
[reliability]
api_keys = ["enc2:00", "sentinel-rotation"]
[[model_routes]]
hint = "fast"
provider = "p"
model = "m"
api_key = "sentinel-route"
[gateway]
paired_tokens = ["sentinel-paired"]
[channels_config.telegram]
bot_token = "***"
"#,
        )
        .expect("test: parse");
        let mut found = scan(&tree);
        found.plaintext_at_rest.sort();
        assert_eq!(
            found.plaintext_at_rest,
            vec![
                "gateway.paired_tokens[0]".to_string(),
                "model_routes[0].api_key".to_string(),
                "reliability.api_keys[1]".to_string()
            ]
        );
        assert_eq!(found.masked, vec!["channels_config.telegram.bot_token".to_string()]);
        assert!(found.has_secret_values());
        assert_eq!(found.scrub("x sentinel-route y sentinel-paired"), "x *** y ***");
    }

    /// F1b, RED before PMMA-f: decryption ran on the stored tree before
    /// serde mapped the alias, and the alias was not registered, so an
    /// encrypted `db_url` written under an alias reached the storage backend
    /// as the literal ciphertext.
    #[test]
    fn encrypted_db_url_under_every_alias_decrypts_on_load() {
        for alias in ["db_url", "dbURL", "database_url", "databaseUrl"] {
            let dir = tempfile::TempDir::new().expect("test: temp config dir");
            let store = crate::security::SecretStore::new(dir.path(), true);
            let secret = "postgres://u:sentinel-db-alias@h/db";
            let encrypted = store.encrypt(secret).expect("test: encrypt");
            let config_path = dir.path().join("config.toml");
            std::fs::write(
                &config_path,
                format!("default_temperature = 0.7\n\n[storage.provider.config]\n{alias} = \"{encrypted}\"\n"),
            )
            .expect("test: write config");
            let loaded =
                crate::config::Config::load_from_path(&config_path, dir.path().join("workspace")).expect("test: load");
            assert_eq!(
                loaded.storage.provider.config.db_url.as_deref(),
                Some(secret),
                "{alias}"
            );
        }
    }

    /// D-049 A: every inbound-trust setting names a real slot of the
    /// `Config` schema (of any type), is documented with where it is
    /// enforced, and is listed once. A typo would silently classify nothing.
    #[test]
    fn every_access_control_path_names_a_slot_of_the_config_schema() {
        let root = serde_json::to_value(schemars::schema_for!(crate::config::Config)).expect("test: schema serializes");
        let mut seen = BTreeSet::new();
        for entry in ACCESS_CONTROL_PATHS {
            assert!(seen.insert(entry.pattern), "{} is listed twice", entry.pattern);
            assert!(
                !entry.enforced_at.is_empty(),
                "{} names no enforcement site",
                entry.pattern
            );
            assert!(
                entry
                    .segments
                    .iter()
                    .try_fold(&root, |node, segment| step(&root, node, *segment))
                    .is_some(),
                "access-control path {} is not in the Config schema",
                entry.pattern
            );
            // None of them is a secret: they are public trust material or
            // switches, so they are not encrypted, only classified.
            assert!(
                !SECRET_PATHS.iter().any(|secret| secret.segments == entry.segments),
                "{} is registered as a secret too",
                entry.pattern
            );
        }
        for listed in [
            "gateway.require_pairing",
            "gateway.trust_forwarded_headers",
            "mcp_server.require_auth",
            "mcp_server.jwt_public_key_pem",
            "mcp_server.jwt_jwks",
            "mcp_server.jwt_jwks_uri",
            "mcp_server.jwt_issuer",
            "mcp_server.jwt_audience",
            "mcp_server.jwt_algorithms",
            "a2a.allowed_peer_issuers",
            "a2a.trusted_trust_domains",
            "a2a.spiffe_trust_bundle_pem",
            "a2a.spiffe_strict_validation",
        ] {
            assert!(seen.contains(listed), "D-049 names {listed}");
        }
        // Paths under a setting count as the setting.
        fn path(elems: &[&str]) -> Vec<PathElem> {
            elems.iter().map(|key| PathElem::Key((*key).to_string())).collect()
        }
        let mut element = path(&["channels_config", "telegram", "allowed_users"]);
        element.push(PathElem::Index(3));
        assert!(is_access_control_path(&element));
        assert!(is_access_control_path(&path(&["gateway", "require_pairing"])));
        assert!(!is_access_control_path(&path(&["gateway", "port"])));
        assert!(!is_access_control_path(&path(&[
            "channels_config",
            "telegram",
            "bot_token"
        ])));
    }

    /// Every registered secret whose value admits a client (D-048), with what
    /// it admits. Reclassifying a path, or registering a new admission
    /// secret, has to change this table: the guard below compares it with
    /// the registry both ways.
    const ADMISSION_SECRETS: &[(&str, &str)] = &[
        ("gateway.paired_tokens[]", "Gateway API clients (bearer tokens)"),
        ("webhook.token", "senders of the standalone webhook"),
        ("webhook.signing_secret", "signed requests to the standalone webhook"),
        (
            "channels_config.webhook.secret",
            "signed requests to the webhook channel",
        ),
        (
            "channels_config.whatsapp.verify_token",
            "the WhatsApp Cloud API webhook handshake",
        ),
        (
            "channels_config.whatsapp.app_secret",
            "signed WhatsApp Cloud API webhook calls",
        ),
        ("channels_config.lark.verification_token", "Lark/Feishu event callbacks"),
        (
            "channels_config.lark.encrypt_key",
            "encrypted Lark/Feishu event callbacks",
        ),
        ("channels_config.linq.signing_secret", "signed Linq webhook calls"),
        (
            "channels_config.nextcloud_talk.webhook_secret",
            "signed Nextcloud Talk bot calls",
        ),
        ("channels_config.wacli.webhook_secret", "signed wacli webhook calls"),
        ("nodes.server.bearer_token", "`prx-node` RPC clients"),
        ("nodes.server.hmac_secret", "signed `prx-node` RPC requests"),
    ];

    /// D-048: the registry marks exactly the admission secrets as access
    /// grants; every other canonical path is an outbound credential or the
    /// MCP launch environment. An alias carries its canonical path's kind.
    #[test]
    fn exactly_the_admission_secrets_are_access_grants() {
        let expected: BTreeSet<&str> = ADMISSION_SECRETS.iter().map(|(pattern, _)| *pattern).collect();
        let registered: BTreeSet<&str> = SECRET_PATHS
            .iter()
            .filter(|secret| secret.alias_of.is_none() && secret.used_as == SecretUse::AccessGrant)
            .map(|secret| secret.pattern)
            .collect();
        assert_eq!(registered, expected);
        let environment: Vec<&str> = SECRET_PATHS
            .iter()
            .filter(|secret| secret.used_as == SecretUse::ProcessEnvironment)
            .map(|secret| secret.pattern)
            .collect();
        assert_eq!(environment, vec!["mcp.servers.<name>.env.<var>"]);
        // Outbound credentials PRX presents to others stay credentials.
        for outbound in [
            "channels_config.telegram.bot_token",
            "channels_config.whatsapp.access_token",
            "channels_config.linq.api_token",
            "nodes.nodes[].bearer_token",
            "nodes.nodes[].hmac_secret",
            "chat.daemon.token",
        ] {
            assert!(
                SECRET_PATHS
                    .iter()
                    .any(|secret| secret.pattern == outbound && secret.used_as == SecretUse::Credential),
                "{outbound} must stay an outbound credential"
            );
        }
    }

    /// Every registered alias is a real serde alias of its canonical path, so
    /// registering it protects a spelling the loader actually accepts. The
    /// fixture's sentinel at the canonical path is moved to the alias key and
    /// must land back in the canonical field.
    #[test]
    fn every_registered_alias_deserializes_into_its_canonical_field() {
        let sentinels = sentinel_paths();
        for secret in SECRET_PATHS.iter().filter(|secret| secret.alias_of.is_some()) {
            let Some(canonical) = secret.alias_of else { continue };
            let canonical_path = SECRET_PATHS
                .iter()
                .find(|other| {
                    other.pattern == canonical
                        && other.alias_of.is_none()
                        && other.handling == secret.handling
                        && other.used_as == secret.used_as
                })
                .unwrap_or_else(|| panic!("{} names an unregistered canonical path {canonical}", secret.pattern));
            let Some(Segment::Key(alias_key)) = secret.segments.last() else {
                panic!("alias {} must end in a key", secret.pattern);
            };
            let concrete = sentinels
                .iter()
                .find(|path| pattern_matches(canonical_path.segments, path))
                .unwrap_or_else(|| panic!("the fixture has no sentinel at {canonical}"));
            let Some((PathElem::Key(canonical_key), parent)) = concrete.split_last() else {
                panic!("{canonical} must end in a key");
            };
            // The default tree holds the canonical key too (a default value),
            // so the rename happens on the merged tree the loader would see.
            let mut tree = toml::Value::try_from(crate::config::Config::default()).expect("test: default tree");
            fixture::overlay(&mut tree, fixture_tree());
            let table = parent
                .iter()
                .try_fold(&mut tree, |node, elem| match elem {
                    PathElem::Key(key) => node.get_mut(key.as_str()),
                    PathElem::Index(index) => node.get_mut(*index),
                })
                .and_then(toml::Value::as_table_mut)
                .expect("test: parent table of the sentinel");
            let value = table.remove(canonical_key).expect("test: sentinel value");
            let expected = value.clone();
            table.insert((*alias_key).to_string(), value);

            let config: crate::config::Config = tree
                .try_into()
                .unwrap_or_else(|error| panic!("{} is not accepted by the loader: {error}", secret.pattern));
            let rendered = toml::Value::try_from(&config).expect("test: render config");
            assert_eq!(
                lookup(&rendered, concrete),
                expected.as_str(),
                "{} does not land in {canonical}",
                secret.pattern
            );
        }
    }

    /// The source files of the structs the `Config` schema is made of. A serde
    /// alias of a credential field in any of them must be registered.
    const SCHEMA_SOURCES: &[(&str, &str)] = &[
        ("config/schema.rs", include_str!("../../schema.rs")),
        ("config/schema/agent.rs", include_str!("../../schema/agent.rs")),
        ("config/schema/atomic.rs", include_str!("../../schema/atomic.rs")),
        ("config/schema/autonomy.rs", include_str!("../../schema/autonomy.rs")),
        ("config/schema/channels.rs", include_str!("../../schema/channels.rs")),
        ("config/schema/chat.rs", include_str!("../../schema/chat.rs")),
        ("config/schema/codex_effort.rs", include_str!("../../schema/codex_effort.rs")),
        ("config/schema/cost.rs", include_str!("../../schema/cost.rs")),
        ("config/schema/credentials.rs", include_str!("../../schema/credentials.rs")),
        (
            "config/schema/decision_engine.rs",
            include_str!("../../schema/decision_engine.rs"),
        ),
        (
            "config/schema/decision_engine/backend.rs",
            include_str!("../../schema/decision_engine/backend.rs"),
        ),
        (
            "config/schema/decision_engine/backend_kind.rs",
            include_str!("../../schema/decision_engine/backend_kind.rs"),
        ),
        ("config/schema/gateway.rs", include_str!("../../schema/gateway.rs")),
        ("config/schema/integrations.rs", include_str!("../../schema/integrations.rs")),
        ("config/schema/legacy.rs", include_str!("../../schema/legacy.rs")),
        ("config/schema/lifecycle.rs", include_str!("../../schema/lifecycle.rs")),
        ("config/schema/media.rs", include_str!("../../schema/media.rs")),
        ("config/schema/memory.rs", include_str!("../../schema/memory.rs")),
        ("config/schema/messaging.rs", include_str!("../../schema/messaging.rs")),
        ("config/schema/nodes.rs", include_str!("../../schema/nodes.rs")),
        (
            "config/schema/observability.rs",
            include_str!("../../schema/observability.rs"),
        ),
        ("config/schema/paths.rs", include_str!("../../schema/paths.rs")),
        ("config/schema/providers.rs", include_str!("../../schema/providers.rs")),
        ("config/schema/proxy.rs", include_str!("../../schema/proxy.rs")),
        (
            "config/schema/ramflux_channel.rs",
            include_str!("../../schema/ramflux_channel.rs"),
        ),
        ("config/schema/routing.rs", include_str!("../../schema/routing.rs")),
        ("config/schema/runtime.rs", include_str!("../../schema/runtime.rs")),
        ("config/schema/secrets.rs", include_str!("../../schema/secrets.rs")),
        ("config/schema/security.rs", include_str!("../../schema/security.rs")),
        ("config/schema/tools.rs", include_str!("../../schema/tools.rs")),
        ("config/schema/tunnel.rs", include_str!("../../schema/tunnel.rs")),
        (
            "channels/email_channel.rs",
            include_str!("../../../channels/email_channel.rs"),
        ),
    ];
    /// Every `(alias, field)` pair of a `#[serde(alias = "...")]` attribute in
    /// the schema sources, with the field it decorates.
    fn serde_aliases() -> Vec<(String, String, &'static str)> {
        let mut found = Vec::new();
        for &(file, source) in SCHEMA_SOURCES {
            let lines: Vec<&str> = source.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                if !trimmed.starts_with("#[serde(") || !trimmed.contains("alias = \"") {
                    continue;
                }
                let Some(field) = lines.iter().skip(index + 1).take(6).find_map(|next| {
                    next.trim_start()
                        .strip_prefix("pub ")
                        .and_then(|rest| rest.split_once(':'))
                        .map(|(name, _)| name.trim().to_string())
                }) else {
                    continue;
                };
                for piece in trimmed.split("alias = \"").skip(1) {
                    if let Some((alias, _)) = piece.split_once('"') {
                        found.push((alias.to_string(), field.clone(), file));
                    }
                }
            }
        }
        found
    }

    /// a39 (R-PMMAf R1): a serde alias of a secret field is a second spelling
    /// the loader accepts. Unless it is registered with `alias_of`, a value
    /// written under it is not encrypted, not decrypted when encrypted, and
    /// not refused as plaintext. RED before SECRETS-OTHER: the node
    /// `bearer_token` aliases `token` were not registered.
    #[test]
    fn every_serde_alias_of_a_secret_field_is_registered() {
        assert!(
            SCHEMA_SOURCES
                .iter()
                .any(|(path, source)| path.starts_with("config/schema/") && !source.is_empty()),
            "the schema scan must include nonempty split sources"
        );
        let split_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config/schema");
        for entry in std::fs::read_dir(split_dir).expect("read schema split sources") {
            let path = entry.expect("schema source entry").path();
            if path.extension().is_some_and(|ext| ext == "rs")
                && path
                    .file_name()
                    .is_none_or(|name| name != "decision_engine_config_tests.rs")
            {
                let relative = format!("config/schema/{}", path.file_name().unwrap().to_string_lossy());
                assert!(
                    SCHEMA_SOURCES.iter().any(|(name, _)| *name == relative),
                    "schema guard misses {relative}"
                );
            }
        }
        let aliases = serde_aliases();
        assert!(
            aliases
                .iter()
                .any(|(alias, field, _)| alias == "dbURL" && field == "db_url"),
            "the alias scan must see the db_url aliases: {aliases:?}"
        );
        let canonical_names: BTreeSet<&str> = SECRET_PATHS
            .iter()
            .filter(|secret| secret.alias_of.is_none())
            .filter_map(|secret| match secret.segments.last() {
                Some(Segment::Key(key)) => Some(*key),
                _ => None,
            })
            .collect();
        let mut missing = Vec::new();
        for (alias, field, file) in &aliases {
            if !(is_secret_key_name(field) || canonical_names.contains(field.as_str())) {
                continue;
            }
            let occurrences = aliases
                .iter()
                .filter(|(other_alias, other_field, _)| other_alias == alias && other_field == field)
                .count();
            let registered = SECRET_PATHS
                .iter()
                .filter(|secret| {
                    matches!(secret.segments.last(), Some(Segment::Key(key)) if key == alias)
                        && secret
                            .alias_of
                            .is_some_and(|canonical| canonical.rsplit('.').next() == Some(field.as_str()))
                })
                .count();
            if registered < occurrences {
                missing.push(format!("{file}: alias `{alias}` of `{field}`"));
            }
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "serde aliases of secret fields without an alias_of row: {missing:#?}"
        );
    }

    /// One step of a schema slot, owned (the schema's keys are not static).
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    enum Step {
        Key(String),
        AnyKey,
        AnyIndex,
    }

    fn display_steps(steps: &[Step]) -> String {
        let mut rendered = String::new();
        for step in steps {
            match step {
                Step::Key(key) => {
                    if !rendered.is_empty() {
                        rendered.push('.');
                    }
                    rendered.push_str(key);
                }
                Step::AnyKey => rendered.push_str(".<*>"),
                Step::AnyIndex => rendered.push_str("[]"),
            }
        }
        rendered
    }

    fn steps_match(segments: &[Segment], steps: &[Step]) -> bool {
        segments.len() == steps.len()
            && segments.iter().zip(steps).all(|(segment, step)| match (segment, step) {
                (Segment::Key(expected), Step::Key(actual)) => expected == actual,
                (Segment::AnyKey, Step::AnyKey) | (Segment::AnyIndex, Step::AnyIndex) => true,
                _ => false,
            })
    }

    /// Every slot of the `Config` schema that admits a string, as a pattern.
    fn schema_string_slots() -> BTreeSet<Vec<Step>> {
        fn walk(
            root: &serde_json::Value,
            node: &serde_json::Value,
            path: &mut Vec<Step>,
            out: &mut BTreeSet<Vec<Step>>,
        ) {
            // Recursive schema types cannot nest this deep in a real config.
            if path.len() > 16 {
                return;
            }
            if !path.is_empty() && admits_string(root, node) {
                out.insert(path.clone());
            }
            for candidate in alternatives(root, node) {
                if let Some(properties) = candidate.get("properties").and_then(serde_json::Value::as_object) {
                    for (key, child) in properties {
                        path.push(Step::Key(key.clone()));
                        walk(root, child, path, out);
                        path.pop();
                    }
                }
                if let Some(child) = candidate
                    .get("additionalProperties")
                    .filter(|schema| schema.is_object())
                {
                    path.push(Step::AnyKey);
                    walk(root, child, path, out);
                    path.pop();
                }
                if let Some(child) = candidate.get("items").filter(|schema| schema.is_object()) {
                    path.push(Step::AnyIndex);
                    walk(root, child, path, out);
                    path.pop();
                }
            }
        }
        let root = serde_json::to_value(schemars::schema_for!(crate::config::Config)).expect("test: schema serializes");
        let mut out = BTreeSet::new();
        walk(&root, &root, &mut Vec::new(), &mut out);
        out
    }

    /// The field name of a slot: its last key, looking through list indices
    /// (`reliability.api_keys[]` is named `api_keys`). A map value has no
    /// field name of its own.
    fn slot_name(steps: &[Step]) -> Option<&str> {
        steps
            .iter()
            .rev()
            .find(|step| !matches!(step, Step::AnyIndex))
            .and_then(|step| match step {
                Step::Key(key) => Some(key.as_str()),
                Step::AnyKey | Step::AnyIndex => None,
            })
    }

    /// Schema slots whose name looks like a credential but that hold none.
    /// Each carries the reason; the guard below fails when one disappears
    /// from the schema, so the list cannot go stale.
    const NOT_SECRET: &[(&str, &str)] = &[(
        "nodes.server.tls_key",
        "a filesystem path to the PEM file (like nodes.server.tls_cert), not the key material",
    )];

    /// Registry completeness (R-PMMA F5, D-018 Q6): every string slot of the
    /// `Config` schema whose name looks like a credential is registered or
    /// explicitly exempt. A credential field added later fails here until it
    /// is registered, so it cannot be stored in plaintext unnoticed. RED
    /// before SECRETS-OTHER: about 30 channel, node, tunnel and webhook
    /// credentials were masked by the name rule only.
    #[test]
    fn every_secret_named_string_slot_of_the_schema_is_registered_or_exempt() {
        let slots = schema_string_slots();
        assert!(
            slots
                .iter()
                .any(|slot| display_steps(slot) == "channels_config.telegram.bot_token"),
            "the schema walk must reach the channel sections"
        );
        let unregistered: Vec<String> = slots
            .iter()
            .filter(|slot| slot_name(slot).is_some_and(is_secret_key_name))
            .filter(|slot| {
                !SECRET_PATHS
                    .iter()
                    .any(|secret| secret.alias_of.is_none() && steps_match(secret.segments, slot))
            })
            .map(|slot| display_steps(slot))
            .filter(|pattern| !NOT_SECRET.iter().any(|(exempt, _)| exempt == pattern))
            .collect();
        assert!(
            unregistered.is_empty(),
            "credential-named config fields missing from SECRET_PATHS (register them, or add them to NOT_SECRET \
             with a reason): {unregistered:#?}"
        );
        for (exempt, reason) in NOT_SECRET {
            assert!(!reason.is_empty(), "expected a non-empty value");
            assert!(
                slots.iter().any(|slot| display_steps(slot) == *exempt),
                "NOT_SECRET entry {exempt} is not a string slot of the schema any more"
            );
        }
    }

    /// Whether a field name says it holds a URL (or a proxy URL).
    fn url_named(name: &str) -> bool {
        name == "homeserver"
            || name
                .split('_')
                .any(|token| matches!(token, "url" | "uri" | "endpoint" | "proxy"))
    }

    /// URL-named slots that are not endpoint URLs.
    const NOT_URL: &[(&str, &str)] = &[
        (
            "proxy.no_proxy[]",
            "host and domain names that bypass the proxy, never a URL with userinfo",
        ),
        (
            "tunnel.custom.url_pattern",
            "a pattern matched against the tunnel command's output, not a URL PRX connects to",
        ),
    ];

    /// Every URL-named string slot of the schema either is a registered
    /// secret (a connection string such as `storage.provider.config.db_url`)
    /// or has its userinfo redacted as a `URL_USERINFO_PATHS` entry. RED
    /// before SECRETS-OTHER: proxy, MCP, node, OTEL, daemon and channel
    /// endpoint URLs were shown with their `user:password@` in clear.
    #[test]
    fn every_url_named_string_slot_of_the_schema_redacts_its_userinfo() {
        let slots = schema_string_slots();
        let uncovered: Vec<String> = slots
            .iter()
            .filter(|slot| slot_name(slot).is_some_and(url_named))
            .filter(|slot| {
                !SECRET_PATHS
                    .iter()
                    .any(|secret| secret.alias_of.is_none() && steps_match(secret.segments, slot))
                    && !ENDPOINT_PATHS
                        .iter()
                        .any(|entry| entry.userinfo && steps_match(entry.segments, slot))
            })
            .map(|slot| display_steps(slot))
            .filter(|pattern| !NOT_URL.iter().any(|(exempt, _)| exempt == pattern))
            .collect();
        assert!(
            uncovered.is_empty(),
            "URL-named config fields missing from ENDPOINT_PATHS (with userinfo): {uncovered:#?}"
        );
        for (exempt, _) in NOT_URL {
            assert!(
                slots.iter().any(|slot| display_steps(slot) == *exempt),
                "NOT_URL entry {exempt} is not a string slot of the schema any more"
            );
        }
        for entry in ENDPOINT_PATHS {
            assert!(
                slots.iter().any(|slot| steps_match(entry.segments, slot)),
                "ENDPOINT_PATHS entry {} is not a string slot of the schema",
                entry.pattern
            );
        }
    }

    /// R-SECRETS F3, RED before SECRETS-f: userinfo in `a2a.card_jwks_uri`
    /// loaded and was published in the public Agent Card. After: the load
    /// refuses it, naming the field and never the password.
    #[test]
    fn a_published_jwks_uri_with_userinfo_is_refused() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "default_temperature = 0.7\n\n[a2a]\ncard_jwks_uri = \"https://u:userinfo-sentinel-jwks@h.invalid/jwks.json\"\n",
        )
        .expect("test: write config");
        let error = crate::config::Config::load_from_path(&path, dir.path().join("workspace"))
            .expect_err("test: userinfo in a published URI must not load");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("a2a.card_jwks_uri"), "{rendered}");
        assert!(!rendered.contains("userinfo-sentinel"), "{rendered}");

        std::fs::write(
            &path,
            "default_temperature = 0.7\n\n[a2a]\ncard_jwks_uri = \"https://h.invalid/jwks.json\"\n",
        )
        .expect("test: write config");
        crate::config::Config::load_from_path(&path, dir.path().join("workspace")).expect("test: plain URI loads");
    }

    /// `prx config show` and the Gateway redaction keep every endpoint URL
    /// readable and replace only its userinfo.
    #[test]
    fn config_show_redacts_url_userinfo_in_every_endpoint_url() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config = fixture::config_from_overlay(dir.path(), fixture::ENDPOINT_USERINFO_TOML);
        let rendered = redacted_stored_toml(&config)
            .expect("test: config show rendering")
            .to_string();
        assert!(
            !rendered.contains("userinfo-sentinel"),
            "config show leaked URL userinfo: {rendered}"
        );
        for endpoint in [
            "http://[redacted]@127.0.0.1:11",
            "socks5://[redacted]@127.0.0.1:13",
            "https://[redacted]@127.0.0.1:14/mcp",
            "http://[redacted]@127.0.0.1:16",
            "http://[redacted]@127.0.0.1:19/health",
            "ws://[redacted]@127.0.0.1:26",
            // The decision backend endpoint (`[causal_tree.backend]`, :27, was
            // deleted with its table in P2-R24).
            "https://[redacted]@127.0.0.1:28/v1",
        ] {
            assert!(rendered.contains(endpoint), "{endpoint} is missing from: {rendered}");
        }

        let mut json = serde_json::to_value(&config).expect("test: json");
        redact(&mut json);
        let json = json.to_string();
        assert!(!json.contains("userinfo-sentinel"), "GET /api/config leaked: {json}");

        let raw: toml::Value = toml::from_str(fixture::ENDPOINT_USERINFO_TOML).expect("test: parses");
        let found = scan(&raw);
        assert!(found.has_secret_values(), "the userinfo must be scrubbed from messages");
        assert!(
            !found
                .scrub(fixture::ENDPOINT_USERINFO_TOML)
                .contains("userinfo-sentinel")
        );
    }

    /// CDP-04 (V6.09 endpoint half): the decision endpoint written by hand
    /// with user information never shows it on a read surface: `prx config
    /// show`, `GET /api/config`, a scrubbed message or the section's `Debug`.
    #[test]
    fn the_decision_endpoint_is_redacted_on_every_read_surface() {
        const SENTINEL: &str = "userinfo-sentinel-cdp04";
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let overlay = format!("[decision_engine.backend]\nendpoint = \"https://decider:{SENTINEL}@127.0.0.1:27/v1\"\n");
        let config = fixture::config_from_overlay(dir.path(), &overlay);
        let rendered = redacted_stored_toml(&config)
            .expect("test: config show rendering")
            .to_string();
        assert!(!rendered.contains(SENTINEL), "config show leaked: {rendered}");
        assert!(rendered.contains("https://[redacted]@127.0.0.1:27/v1"), "{rendered}");
        let mut json = serde_json::to_value(&config).expect("test: json");
        redact(&mut json);
        assert!(!json.to_string().contains(SENTINEL), "GET /api/config leaked: {json}");
        let found = scan(&toml::from_str::<toml::Value>(&overlay).expect("test: overlay"));
        assert!(!found.scrub(&overlay).contains(SENTINEL));
        let debug = format!("{:?}", config.decision_engine);
        assert!(!debug.contains(SENTINEL), "{debug}");
        assert!(is_credential_recipient_endpoint(&[
            PathElem::Key("decision_engine".to_string()),
            PathElem::Key("backend".to_string()),
            PathElem::Key("endpoint".to_string()),
        ]));
    }

    /// F2, RED before PMMA-f: a credential stored as the literal mask (left
    /// by the pre-A-4 round trip, possibly encrypted) loaded silently and was
    /// sent to the provider as the key. After: the load warns, naming the
    /// path and never a value.
    #[test]
    fn load_warns_about_a_stored_mask_by_path_only() {
        #[derive(Clone, Default)]
        struct Sink(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
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

        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let store = crate::security::SecretStore::new(dir.path(), true);
        let encrypted_mask = store.encrypt(REDACTION_MASK).expect("test: encrypt mask");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "default_temperature = 0.7\n\n[[model_routes]]\nhint = \"fast\"\nprovider = \"openrouter\"\nmodel = \"m\"\napi_key = \"{encrypted_mask}\"\n\n[gateway]\npaired_tokens = [\"***\", \"sentinel-paired-ok\"]\n"
            ),
        )
        .expect("test: write config");

        let _serial = fixture::LOG_CALLSITE_LOCK.blocking_lock();
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        let loaded = tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            crate::config::Config::load_from_path(&config_path, dir.path().join("workspace"))
        })
        .expect("test: a stored mask must not stop the load");
        assert_eq!(
            loaded.model_routes.first().and_then(|r| r.api_key.as_deref()),
            Some(REDACTION_MASK)
        );

        let logs = String::from_utf8_lossy(&sink.0.lock()).to_string();
        assert!(
            logs.contains("model_routes[0].api_key"),
            "the stored mask must be reported: {logs}"
        );
        assert!(
            logs.contains("gateway.paired_tokens[0]"),
            "the stored mask must be reported: {logs}"
        );
        assert!(
            !logs.contains("sentinel-paired-ok"),
            "the warning must not carry a value: {logs}"
        );
        assert!(!logs.contains("enc2:"), "the warning must not carry a value: {logs}");
    }

    /// F7, RED before PMMA-f: `Config`'s `Debug` redacted only the root key;
    /// route, agent, rotation, tool, storage and channel secrets printed in
    /// clear. After: every registry path is masked and ordinary values stay.
    #[test]
    fn config_debug_masks_every_registered_path() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let config = fixture::config_in(dir.path());
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains(fixture::SENTINEL_PREFIX),
            "Debug leaked a secret: {rendered}"
        );
        assert!(
            rendered.contains("model-test"),
            "Debug must keep ordinary values: {rendered}"
        );
        assert!(
            rendered.contains("config_path"),
            "Debug must keep the path fields: {rendered}"
        );
    }

    #[test]
    fn name_rule_masks_an_unregistered_credential_and_keeps_ordinary_values() {
        let mut value = serde_json::json!({
            "future_section": { "client_secret": "sentinel-future", "enabled": true, "count": 3 },
            "default_model": "kimi-k2",
            "nested": { "credential": { "username": "user", "password": "pass" } },
        });
        redact(&mut value);
        assert_eq!(
            value.pointer("/future_section/client_secret"),
            Some(&serde_json::json!(REDACTION_MASK))
        );
        assert_eq!(value.pointer("/future_section/enabled"), Some(&serde_json::json!(true)));
        assert_eq!(value.pointer("/default_model"), Some(&serde_json::json!("kimi-k2")));
        assert_eq!(
            value.pointer("/nested/credential/username"),
            Some(&serde_json::json!(REDACTION_MASK))
        );
        assert_eq!(
            value.pointer("/nested/credential/password"),
            Some(&serde_json::json!(REDACTION_MASK))
        );
    }

    /// Every configuration path that names a provider or an endpoint by URL.
    /// Each value carries a distinct fixture password, so one search finds a
    /// leak from any of them.
    const USERINFO_TOML: &str = r#"
default_provider = "custom:http://alice:userinfo-sentinel-default@127.0.0.1:1/v1"
api_url = "http://alice:userinfo-sentinel-api-url@127.0.0.1:2/v1"

[reliability]
fallback_providers = ["custom:https://bob:userinfo-sentinel-fallback@127.0.0.1:3/v1", "openrouter"]

[[model_routes]]
hint = "fast"
provider = "custom:http://carol:userinfo-sentinel-route@127.0.0.1:4/v1"
model = "route-model"

[[embedding_routes]]
hint = "semantic"
provider = "custom:http://dave:userinfo-sentinel-embedding@127.0.0.1:5/v1"
model = "embed-model"

[agents.worker]
provider = "custom:http://erin:userinfo-sentinel-agent@127.0.0.1:6/v1"
model = "agent-model"

[[providers.tool_dialects]]
provider = "custom:http://frank:userinfo-sentinel-dialect@127.0.0.1:7/v1"
tool_schema = "no_root_combinators"

[smart_group]
classifier_provider = "custom:http://grace:userinfo-sentinel-classifier@127.0.0.1:8/v1"
"#;

    fn userinfo_config(dir: &std::path::Path) -> crate::config::Config {
        let mut tree = toml::Value::try_from(crate::config::Config::default()).expect("test: default config tree");
        fixture::overlay(
            &mut tree,
            toml::from_str(USERINFO_TOML).expect("test: userinfo fixture parses"),
        );
        let mut config: crate::config::Config = tree.try_into().expect("test: the userinfo fixture must deserialize");
        config.config_path = dir.join("config.toml");
        config.workspace_dir = dir.join("workspace");
        config
    }

    /// R-C2Kf S-2: `prx config show` printed every provider label verbatim,
    /// so a `custom:` URL's `user:password@` reached the terminal. The
    /// endpoints stay readable; only their userinfo is replaced.
    #[test]
    fn config_show_redacts_url_userinfo_in_every_provider_and_endpoint_field() {
        let dir = tempfile::TempDir::new().expect("test: temp config dir");
        let rendered = redacted_stored_toml(&userinfo_config(dir.path()))
            .expect("test: config show rendering")
            .to_string();
        assert!(
            !rendered.contains("userinfo-sentinel"),
            "config show leaked URL userinfo: {rendered}"
        );
        for endpoint in [
            "custom:http://[redacted]@127.0.0.1:1/v1",
            "http://[redacted]@127.0.0.1:2/v1",
            "custom:https://[redacted]@127.0.0.1:3/v1",
            "custom:http://[redacted]@127.0.0.1:4/v1",
            "custom:http://[redacted]@127.0.0.1:5/v1",
            "custom:http://[redacted]@127.0.0.1:6/v1",
            "custom:http://[redacted]@127.0.0.1:7/v1",
            "custom:http://[redacted]@127.0.0.1:8/v1",
        ] {
            assert!(rendered.contains(endpoint), "{endpoint} is missing from: {rendered}");
        }
        assert!(
            rendered.contains("\"openrouter\""),
            "an ordinary label must stay: {rendered}"
        );
    }
