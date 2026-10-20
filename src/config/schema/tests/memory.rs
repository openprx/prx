#![cfg(test)]

use super::*;
use tokio::test;

#[test]
async fn config_rejects_hybrid_process_memory_without_merge_consumer() {
    let mut config = Config::default();
    config.sessions_spawn.process_memory_strategy = "hybrid".to_string();

    let error = config.validate().unwrap_err().to_string();
    assert_eq!(error, HYBRID_PROCESS_MEMORY_UNAVAILABLE);

    config.sessions_spawn.process_memory_strategy = "shared_fabric".to_string();
    config.validate().unwrap();
    config.sessions_spawn.process_memory_strategy = "isolated_private".to_string();
    config.validate().unwrap();
}

/// D-104 C-1 (ruling): two embedding routes sharing a hint still load
/// (an upgrade never stops PRX over them); they are named, with the fix.
#[test]
async fn duplicate_embedding_route_hints_load_and_are_reported() {
    let route = |model: &str| EmbeddingRouteConfig {
        hint: "semantic".into(),
        provider: "openai".into(),
        model: model.into(),
        dimensions: None,
        api_key: None,
        credential_ref: None,
    };
    let mut config = Config::default();
    config.embedding_routes = vec![route("text-embedding-3-small"), route("text-embedding-3-large")];
    config.validate().expect("an existing duplicate still loads");
    let notes = duplicate_embedding_route_hints(&config);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes.first().is_some_and(|note| {
            note.contains("embedding_routes[1] and embedding_routes[0] both use hint 'semantic'")
                && note.contains("Give each route its own hint")
        }),
        "{notes:?}"
    );
}

/// A `memory_scope` typo used to survive config load and only fail at the
/// first spawn of that agent, deep inside `sessions_spawn`.
///
/// MUTATION GUARD: delete the `parse_memory_scope` call in `validate` and
/// `config.validate()` accepts "isolate" here.
#[test]
async fn config_rejects_an_unknown_agent_memory_scope() {
    let mut config = Config::default();
    let mut agent = agentic_delegate_config(vec!["*".into()]);
    agent.memory_scope = Some("isolate".into());
    config.agents.insert("worker".into(), agent);

    let error = format!("{:#}", config.validate().unwrap_err());
    assert!(error.contains("agents.worker.memory_scope"), "{error}");
    assert!(error.contains("isolate"), "{error}");

    // Both accepted spellings, and the unset default, still load.
    for scope in [None, Some("shared".to_string()), Some(" Isolated ".to_string())] {
        let mut agent = agentic_delegate_config(vec!["*".into()]);
        agent.memory_scope = scope.clone();
        config.agents.insert("worker".into(), agent);
        config
            .validate()
            .unwrap_or_else(|error| panic!("{scope:?} must load: {error:#}"));
    }
}

#[test]
async fn memory_config_default_hygiene_settings() {
    let m = MemoryConfig::default();
    assert_eq!(m.backend, "sqlite");
    assert!(m.auto_save);
    assert!(m.events.record_user_messages);
    assert!(m.events.record_assistant_messages);
    assert!(!m.events.record_tool_events);
    assert!(m.semantic.auto_promote_user_messages);
    assert!(!m.semantic.auto_promote_assistant_messages);
    assert_eq!(m.semantic.min_chars, 30);
    assert!(m.should_auto_promote_user_message("this is a long enough user preference that should be stored"));
    assert!(!m.acl_enabled);
    assert_eq!(m.archive_after_days, 7);
    assert_eq!(m.purge_after_days, 30);
    assert_eq!(m.conversation_retention_days, 3);
    assert_eq!(m.daily_retention_days, 7);
    assert!(m.sqlite_open_timeout_secs.is_none());
}

#[tokio::test]
async fn memory_events_and_semantic_config_parse_independently() {
    let parsed: MemoryConfig = toml::from_str(
        r#"
backend = "sqlite"
auto_save = true

[events]
record_user_messages = true
record_assistant_messages = false
record_tool_events = true

[semantic]
auto_promote_user_messages = false
auto_promote_assistant_messages = true
min_chars = 12
"#,
    )
    .unwrap();

    assert!(parsed.should_record_user_message_event());
    assert!(!parsed.should_record_assistant_message_event());
    assert!(parsed.events.record_tool_events);
    assert!(
        !parsed.should_auto_promote_user_message("this user message is long enough but semantic promotion is disabled")
    );
    assert!(parsed.should_auto_promote_assistant_message("assistant text long enough for semantic promotion"));
}

#[test]
async fn storage_provider_config_defaults() {
    let storage = StorageConfig::default();
    assert!(storage.provider.config.provider.is_empty(), "expected an empty value");
    assert!(storage.provider.config.db_url.is_none());
    assert_eq!(storage.provider.config.schema, "public");
    assert_eq!(storage.provider.config.table, "memories");
    assert!(storage.provider.config.connect_timeout_secs.is_none());
    assert_eq!(storage.provider.config.pool_max_size, 32);
}

#[test]
async fn storage_provider_dburl_alias_deserializes() {
    let raw = r#"
default_temperature = 0.7

[storage.provider.config]
provider = "postgres"
dbURL = "postgres://postgres:postgres@localhost:5432/openprx"
schema = "public"
table = "memories"
connect_timeout_secs = 12
pool_max_size = 64
"#;

    let parsed: Config = toml::from_str(raw).unwrap();
    assert_eq!(parsed.storage.provider.config.provider, "postgres");
    assert_eq!(
        parsed.storage.provider.config.db_url.as_deref(),
        Some("postgres://postgres:postgres@localhost:5432/openprx")
    );
    assert_eq!(parsed.storage.provider.config.schema, "public");
    assert_eq!(parsed.storage.provider.config.table, "memories");
    assert_eq!(parsed.storage.provider.config.connect_timeout_secs, Some(12));
    assert_eq!(parsed.storage.provider.config.pool_max_size, 64);
}

#[test]
async fn explicit_embedding_provider_silences_the_upgrade_notice() {
    let dir = tempfile::TempDir::new().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
default_temperature = 0.7

[memory]
backend = "sqlite"
auto_save = true
embedding_provider = "local"
"#,
    )
    .unwrap();

    let loaded = Config::load_from_path(&config_path, dir.path().join("workspace")).unwrap();
    assert_eq!(loaded.memory.embedding_provider, "local");
    assert!(loaded.memory.embedding_provider_explicit);
    assert!(loaded.embedding_default_upgrade_notice().is_none());
}

#[test]
async fn legacy_embedding_table_counts_as_an_explicit_provider() {
    let dir = tempfile::TempDir::new().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
default_temperature = 0.7

[memory]
backend = "sqlite"
auto_save = true

[memory.embedding]
provider = "none"
model = "legacy"
dimension = 512
"#,
    )
    .unwrap();

    let loaded = Config::load_from_path(&config_path, dir.path().join("workspace")).unwrap();
    assert_eq!(loaded.memory.embedding_provider, "none");
    assert!(loaded.memory.embedding_provider_explicit);
    assert!(loaded.embedding_default_upgrade_notice().is_none());
}

// ---- FIX-P2-09: MemoryConfig::validate -------------------------------

#[test]
async fn memory_default_validates() {
    assert!(MemoryConfig::default().validate().is_ok());
}

#[test]
async fn memory_unknown_backend_rejected() {
    let mut mem = MemoryConfig::default();
    mem.backend = "cassandra".into();
    assert!(mem.validate().is_err());
}

#[test]
async fn memory_empty_embedding_provider_rejected() {
    let mut mem = MemoryConfig::default();
    mem.embedding_provider = "  ".into();
    assert!(mem.validate().is_err());
}

#[test]
async fn memory_zero_dimension_with_embeddings_rejected() {
    let mut mem = MemoryConfig::default();
    mem.embedding_provider = "openai".into();
    mem.embedding_dimensions = 0;
    assert!(mem.validate().is_err());
}

#[test]
async fn memory_zero_dimension_with_none_provider_allowed() {
    // When embeddings are disabled, a zero dimension is acceptable.
    let mut mem = MemoryConfig::default();
    mem.embedding_provider = "none".into();
    mem.embedding_dimensions = 0;
    assert!(mem.validate().is_ok());
}

#[test]
async fn memory_custom_provider_requires_valid_url() {
    let mut mem = MemoryConfig::default();
    mem.embedding_provider = "custom:not a url".into();
    assert!(mem.validate().is_err());

    mem.embedding_provider = "custom:ftp://host/embed".into();
    assert!(mem.validate().is_err());

    mem.embedding_provider = "custom:https://host/embed".into();
    assert!(mem.validate().is_ok());
}

#[test]
async fn memory_both_weights_zero_rejected() {
    let mut mem = MemoryConfig::default();
    mem.vector_weight = 0.0;
    mem.keyword_weight = 0.0;
    assert!(mem.validate().is_err());
}

#[test]
async fn memory_out_of_range_relevance_rejected() {
    let mut mem = MemoryConfig::default();
    mem.min_relevance_score = 1.5;
    assert!(mem.validate().is_err());
    mem.min_relevance_score = -0.1;
    assert!(mem.validate().is_err());
}

#[test]
async fn config_validate_rejects_bad_memory_subtree() {
    // FIX-P2-09: memory validation is now reachable from Config::validate.
    let mut config = Config::default();
    config.memory.min_relevance_score = 9.0;
    assert!(config.validate().is_err());
}
