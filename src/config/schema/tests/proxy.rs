#![cfg(test)]

use super::*;
use tokio::test;

#[test]
async fn proxy_config_scope_services_requires_entries_when_enabled() {
    let proxy = ProxyConfig {
        http_proxy: Some("http://127.0.0.1:7890".into()),
        https_proxy: None,
        all_proxy: None,
        no_proxy: Vec::new(),
        scope: ProxyScope::Services,
        services: Vec::new(),
    };

    let error = proxy.validate().unwrap_err().to_string();
    assert!(error.contains("proxy.scope='services'"));
}

#[test]
#[serial_test::serial]
async fn runtime_proxy_client_cache_reuses_default_profile_key() {
    let _guard = PROXY_CACHE_TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let service_key = format!(
        "provider.cache_test.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos()
    );
    let cache_key = runtime_proxy_cache_key(&service_key, None, None);

    clear_runtime_proxy_client_cache();
    assert!(!runtime_proxy_cache_contains(&cache_key));

    build_runtime_proxy_client(&service_key).expect("test: proxy client build");
    assert!(runtime_proxy_cache_contains(&cache_key));

    build_runtime_proxy_client(&service_key).expect("test: proxy client build (cached)");
    assert!(runtime_proxy_cache_contains(&cache_key));
}

#[test]
#[serial_test::serial]
async fn set_runtime_proxy_config_clears_runtime_proxy_client_cache() {
    let _guard = PROXY_CACHE_TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let service_key = format!(
        "provider.cache_timeout_test.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos()
    );
    let cache_key = runtime_proxy_cache_key(&service_key, Some(30), Some(5));

    clear_runtime_proxy_client_cache();
    build_runtime_proxy_client_with_timeouts(&service_key, 30, 5).expect("test: proxy client with timeouts build");
    assert!(runtime_proxy_cache_contains(&cache_key));

    let generation = runtime_proxy_generation();
    set_runtime_proxy_config(ProxyConfig::default());
    assert!(!runtime_proxy_cache_contains(&cache_key));
    // Caches outside this module (the decision clients) key by it.
    assert!(
        runtime_proxy_generation() > generation,
        "a proxy change moves the generation"
    );
}

/// Old configs may still name the retired Matrix proxy selector. It must load,
/// the selector must be ignored (never routed), and a typo must still fail.
#[test]
async fn a_retired_matrix_proxy_selector_loads_ignored_and_a_typo_is_refused() {
    let _serial = crate::config::secret_paths::fixture::LOG_CALLSITE_LOCK.lock().await;
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let path = dir.path().join("config.toml");
    let old = "default_temperature = 0.7\n\n[proxy]\nhttp_proxy = \"http://127.0.0.1:7890\"\nscope = \"services\"\nservices = [\"channel.matrix\", \"channel.telegram\"]\n";
    std::fs::write(&path, old).expect("test: write config");

    let (loaded, logs) = capture_logs(|| Config::load_from_path(&path, dir.path().join("workspace")));
    let config = loaded.expect("test: an old channel.matrix proxy selector still loads");
    assert!(
        logs.contains("Matrix support was removed"),
        "the retired selector must be reported as removed: {logs}"
    );
    assert!(!config.proxy.should_apply_to_service("channel.matrix"));
    assert!(config.proxy.should_apply_to_service("channel.telegram"));

    std::fs::write(&path, old.replace("channel.telegram", "channle.telegram")).expect("test: write config");
    let error = Config::load_from_path(&path, dir.path().join("workspace"))
        .expect_err("a typo proxy selector must still be refused")
        .to_string();
    assert!(
        error.contains("channle.telegram"),
        "the misspelled selector must be named, got: {error}"
    );
}

fn proxy_with_scope(scope: ProxyScope, services: &[&str]) -> ProxyConfig {
    ProxyConfig {
        http_proxy: Some("http://127.0.0.1:7890".into()),
        https_proxy: None,
        all_proxy: None,
        no_proxy: Vec::new(),
        scope,
        services: services.iter().map(|service| (*service).to_string()).collect(),
    }
}

/// Review r1 finding 1: only `scope = "services"` with a matching selector is
/// an explicit selection; a general or environment proxy is not.
#[test]
async fn only_a_services_selector_explicitly_selects_a_service() {
    let general = proxy_with_scope(ProxyScope::Zeroclaw, &[]);
    assert!(
        general.should_apply_to_service("tool.http_request"),
        "a general proxy still applies to ordinary clients"
    );
    assert!(
        !general.explicitly_selects_service("tool.http_request"),
        "a general proxy is not an explicit selection"
    );
    assert!(
        !proxy_with_scope(ProxyScope::Environment, &["tool.http_request"])
            .explicitly_selects_service("tool.http_request"),
        "the environment scope selects nothing"
    );
    for selector in ["tool.http_request", "  Tool.HTTP_Request  ", "provider.openai, tool.http_request"] {
        assert!(
            proxy_with_scope(ProxyScope::Services, &[selector]).explicitly_selects_service("tool.http_request"),
            "services selector `{selector}` must select tool.http_request"
        );
    }
    for selector in ["tool.*", "TOOL.*"] {
        let wildcard = proxy_with_scope(ProxyScope::Services, &[selector]);
        assert!(
            wildcard.should_apply_to_service("tool.http_request"),
            "the wildcard `{selector}` still routes ordinary tool clients"
        );
        assert!(
            !wildcard.explicitly_selects_service("tool.http_request"),
            "the wildcard `{selector}` must not explicitly select tool.http_request"
        );
    }
    assert!(
        !proxy_with_scope(ProxyScope::Services, &["provider.openai"]).explicitly_selects_service("tool.http_request"),
        "a selector for another service must not select tool.http_request"
    );
}

/// Review r1 finding 2: a saved configuration with the environment scope
/// still loads and validates (startup must not fail); it carries the
/// migration notice, and no other scope does.
#[test]
async fn a_saved_environment_scope_loads_with_a_migration_notice() {
    let loaded: ProxyConfig = toml::from_str("scope = \"environment\"\nhttp_proxy = \"http://127.0.0.1:7890\"\n")
        .expect("test: an old environment-scope [proxy] table still parses");
    assert_eq!(
        loaded.scope,
        ProxyScope::Environment,
        "the saved scope is kept as written"
    );
    loaded
        .validate()
        .expect("test: the environment scope must not fail validation at load");
    assert!(
        !loaded.should_apply_to_service("tool.http_request"),
        "the inactive scope routes no traffic"
    );

    let notice = loaded
        .environment_scope_notice()
        .expect("test: the environment scope has a notice");
    assert_eq!(
        notice,
        ProxyConfig::ENVIRONMENT_SCOPE_INACTIVE,
        "the notice is the published migration text"
    );
    for needed in ["inactive", "zeroclaw", "services", "HTTP_PROXY"] {
        assert!(
            notice.contains(needed),
            "the migration notice must mention `{needed}`: {notice}"
        );
    }
    for scope in [ProxyScope::Zeroclaw, ProxyScope::Services] {
        assert!(
            proxy_with_scope(scope, &["tool.*"])
                .environment_scope_notice()
                .is_none(),
            "{scope:?} is active and has no notice"
        );
    }
}
