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
