    #[test]
    fn cargo_build_directories_are_recognized_and_ordinary_bin_directories_are_not() {
        let temp = TempDir::new().unwrap();
        for layout in [
            "target/debug",
            "target/debug/deps",
            "target/release",
            "target/x86_64-unknown-linux-gnu/release",
            "prx-qa-output/debug",
            "prx-qa-output/debug/deps",
            "custom-output/aarch64-unknown-linux-gnu/release",
        ] {
            let build = temp.path().join(layout);
            std::fs::create_dir_all(build.join(".fingerprint")).unwrap();
            let build = build.to_str().unwrap();
            assert!(
                is_cargo_build_dir(Path::new(build)),
                "must be a build directory: {build}"
            );
        }
        for deployed in ["/usr/local/bin", "/home/ck/.cargo/bin", "/opt/prx/target", "/srv/debug"] {
            assert!(
                !is_cargo_build_dir(Path::new(deployed)),
                "must not be mistaken for a build directory: {deployed}"
            );
        }
    }

    /// A pid outlives the daemon that wrote it. Reading the environment of
    /// whatever process inherited the number judges the daemon PATH from a
    /// stranger's environment.
    #[test]
    fn only_a_process_that_is_prx_is_treated_as_the_daemon() {
        assert!(process_identity_looks_like_prx(
            Some(Path::new("/usr/local/bin/prx")),
            b""
        ));
        assert!(process_identity_looks_like_prx(
            Some(Path::new("/usr/local/bin/prx-node")),
            b""
        ));
        assert!(process_identity_looks_like_prx(
            None,
            b"/usr/local/bin/prx\x00daemon\x00"
        ));
        assert!(!process_identity_looks_like_prx(Some(Path::new("/usr/bin/sleep")), b""));
        assert!(!process_identity_looks_like_prx(None, b"/usr/bin/sleep\x00600\x00"));
        assert!(
            !process_identity_looks_like_prx(Some(Path::new("/opt/worker/code/prx/target/debug/other")), b""),
            "a checkout directory called prx is not a prx process"
        );
        assert!(!process_identity_looks_like_prx(None, b""), "no evidence is not a yes");
    }

    #[test]
    fn daemon_path_check_skips_when_state_has_no_pid() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.config_path = temp.path().join("config.toml");
        std::fs::write(
            temp.path().join("daemon_state.json"),
            br#"{"updated_at":"2026-09-17T00:00:00Z","components":{}}"#,
        )
        .unwrap();

        let mut items = Vec::new();
        check_daemon_process_path(&config, &mut items);

        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.severity, Severity::Ok, "skip must not be an error or warning");
        assert_eq!(item.state, DiagnosticState::Unknown);
        assert!(
            item.message.contains("no pid"),
            "skip message must explain why, got: {}",
            item.message
        );
    }

    #[test]
    fn daemon_path_check_skips_when_state_file_is_absent() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.config_path = temp.path().join("config.toml");

        let mut items = Vec::new();
        check_daemon_process_path(&config, &mut items);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, Severity::Ok);
        assert_eq!(items[0].state, DiagnosticState::Unknown);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn daemon_path_check_skips_for_a_pid_that_does_not_exist() {
        let temp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.config_path = temp.path().join("config.toml");
        // PID above the kernel maximum can never be live.
        std::fs::write(
            temp.path().join("daemon_state.json"),
            br#"{"pid":4294967000,"updated_at":"2026-09-17T00:00:00Z"}"#,
        )
        .unwrap();

        let mut items = Vec::new();
        check_daemon_process_path(&config, &mut items);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, Severity::Ok, "message: {}", items[0].message);
        assert!(
            items[0].message.contains("environ"),
            "skip message must name the unreadable source, got: {}",
            items[0].message
        );
    }

    /// USERINFO-2: `prx doctor` prints every configured provider label. Each
    /// may be a `custom:` URL carrying `user:password@`.
    #[test]
    fn doctor_names_every_provider_without_url_userinfo() {
        let tmp = tempfile::TempDir::new().unwrap();
        let label = |port: u16| format!("custom:http://u:SECRET-fixture-pw@127.0.0.1:{port}/v1");
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            default_provider: Some(label(1)),
            ..Config::default()
        };
        // An unparseable fallback and route make doctor print "invalid" lines too.
        config.reliability.fallback_providers =
            vec![label(2), "custom:http://u:SECRET-fixture-pw@127.0.0.1:99999/v1".into()];
        config.model_routes = vec![crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "fast".into(),
            provider: "custom:http://u:SECRET-fixture-pw@127.0.0.1:99998/v1".into(),
            model: "m".into(),
            api_key: None,
            credential_ref: None,
        }];
        let report = diagnose(&config);
        let rendered = report
            .items
            .iter()
            .map(|item| item.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("[redacted]@127.0.0.1"),
            "doctor no longer names the endpoints: {rendered}"
        );
        assert!(
            !rendered.contains("SECRET-fixture-pw"),
            "doctor printed URL userinfo: {rendered}"
        );
    }

    /// R-C2BUg G-1: doctor messages that start with a URL keep none of their
    /// credentials.
    #[test]
    fn doctor_messages_redact_every_url_in_url_first_text() {
        crate::providers::label_redaction_tests::assert_every_url_redacted("doctor", &|text| {
            DiagItem::warn("fixture", text).message
        });
    }

    /// D-063, RED before: the gateway probe resolved `(host, port)`, and a
    /// bracketed IPv6 `gateway.host = "[::1]"` (the spelling a URL uses) did
    /// not resolve, so doctor reported "cannot resolve gateway bind address".
    /// After: the brackets are removed before resolution.
    #[test]
    fn the_gateway_probe_resolves_a_bracketed_ipv6_host() {
        for host in ["[::1]", "::1", "127.0.0.1"] {
            let mut config = Config::default();
            config.gateway.host = host.to_string();
            config.gateway.port = 9;
            let mut items = Vec::new();
            check_gateway_runtime(&config, &mut items);
            let messages: Vec<&str> = items.iter().map(|item| item.message.as_str()).collect();
            assert!(
                !messages.iter().any(|message| message.contains("cannot resolve")),
                "{host}: {messages:?}"
            );
        }
    }

    /// RED before PMMDtail-j (D-084 E-3): `prx doctor`'s auth-profile check
    /// loaded through the migrating load, rewriting auth-profiles.json.
    #[test]
    fn doctor_never_rewrites_the_profiles_file() {
        let tmp = TempDir::new().expect("test: temp dir");
        let path = crate::auth::profiles::write_legacy_token_profile(tmp.path(), "moonshot", "sentinel-moonshot");
        let before = std::fs::read_to_string(&path).expect("test: read");
        let mut config = Config::default();
        config.config_path = tmp.path().join("config.toml");
        config.secrets.encrypt = true;

        let status = active_auth_profile_credential_status(&config, "moonshot");

        assert!(
            matches!(status, AuthProfileCredentialStatus::Present { .. }),
            "{status:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("test: read"),
            before,
            "the file was rewritten"
        );
    }
    /// Mutation: remove secret scanning in DiagItem::with_state.
    #[test]
    fn diagnostic_items_hide_nested_error_credentials() {
        let error = anyhow::anyhow!("token=opaque7credential").context("failed");
        let item = DiagItem::error("runtime", format!("{error:#}"));
        assert_eq!(item.message, "failed: token=[REDACTED]");
    }
