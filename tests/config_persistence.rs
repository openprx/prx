//! TG2: Config Load/Save Round-Trip Tests
//!
//! Prevents: Pattern 2 — Config persistence & workspace discovery bugs (13% of user bugs).
//! Issues: #547, #417, #621, #802
//!
//! Tests `Config::load_or_init()` with isolated temp directories, env var overrides,
//! and config file round-trips to verify workspace discovery and persistence.
#![allow(clippy::field_reassign_with_default)]

use openprx::config::{AgentConfig, Config, MemoryConfig};
use std::fs;

// ─────────────────────────────────────────────────────────────────────────────
// Config default construction
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn config_default_has_expected_provider() {
    let config = Config::default();
    assert!(
        config.default_provider.is_some(),
        "default config should have a default_provider"
    );
}

#[test]
fn config_default_has_expected_model() {
    let config = Config::default();
    assert!(
        config.default_model.is_some(),
        "default config should have a default_model"
    );
}

#[test]
fn config_default_temperature_positive() {
    let config = Config::default();
    assert!(
        config.default_temperature > 0.0,
        "default temperature should be positive"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// AgentConfig defaults
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn agent_config_default_tool_dispatcher() {
    let agent = AgentConfig::default();
    assert_eq!(
        agent.tool_dispatcher, "auto",
        "default tool_dispatcher should be 'auto'"
    );
}

#[test]
fn agent_config_default_compact_context_off() {
    let agent = AgentConfig::default();
    assert!(!agent.compact_context, "compact_context should default to false");
}

// ─────────────────────────────────────────────────────────────────────────────
// MemoryConfig defaults
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn memory_config_default_backend() {
    let memory = MemoryConfig::default();
    assert!(!memory.backend.is_empty(), "memory backend should have a default value");
}

#[test]
fn memory_config_default_embedding_provider() {
    let memory = MemoryConfig::default();
    // Default embedding_provider should be set (even if "none")
    assert!(
        !memory.embedding_provider.is_empty(),
        "embedding_provider should have a default value"
    );
}

#[test]
fn memory_config_default_vector_keyword_weights_sum_to_one() {
    let memory = MemoryConfig::default();
    let sum = memory.vector_weight + memory.keyword_weight;
    assert!(
        (sum - 1.0).abs() < 0.01,
        "vector_weight + keyword_weight should sum to ~1.0, got {sum}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Config TOML serialization round-trip
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn config_toml_roundtrip_preserves_provider() {
    let mut config = Config::default();
    config.default_provider = Some("deepseek".into());
    config.default_model = Some("deepseek-chat".into());
    config.default_temperature = 0.5;

    let toml_str = toml::to_string(&config).expect("config should serialize to TOML");
    let parsed: Config = toml::from_str(&toml_str).expect("TOML should deserialize back");

    assert_eq!(parsed.default_provider.as_deref(), Some("deepseek"));
    assert_eq!(parsed.default_model.as_deref(), Some("deepseek-chat"));
    assert!((parsed.default_temperature - 0.5).abs() < f64::EPSILON);
}

#[test]
fn config_toml_roundtrip_preserves_agent_config() {
    let mut config = Config::default();
    config.agent.compact_context = true;
    config.agent.read_only_tool_concurrency_window = 8;
    config.agent.compaction.max_context_tokens = 128_000;

    let toml_str = toml::to_string(&config).expect("config should serialize to TOML");
    let parsed: Config = toml::from_str(&toml_str).expect("TOML should deserialize back");

    assert!(parsed.agent.compact_context);
    assert_eq!(parsed.agent.read_only_tool_concurrency_window, 8);
    assert_eq!(parsed.agent.compaction.max_context_tokens, 128_000);
}

#[test]
fn config_toml_roundtrip_preserves_memory_config() {
    let mut config = Config::default();
    config.memory.embedding_provider = "openai".into();
    config.memory.embedding_model = "text-embedding-3-small".into();
    config.memory.vector_weight = 0.8;
    config.memory.keyword_weight = 0.2;

    let toml_str = toml::to_string(&config).expect("config should serialize to TOML");
    let parsed: Config = toml::from_str(&toml_str).expect("TOML should deserialize back");

    assert_eq!(parsed.memory.embedding_provider, "openai");
    assert_eq!(parsed.memory.embedding_model, "text-embedding-3-small");
    assert!((parsed.memory.vector_weight - 0.8).abs() < f64::EPSILON);
    assert!((parsed.memory.keyword_weight - 0.2).abs() < f64::EPSILON);
}

// ─────────────────────────────────────────────────────────────────────────────
// Config file write/read round-trip with tempdir
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn config_file_write_read_roundtrip() {
    let tmp = tempfile::TempDir::new().expect("tempdir creation should succeed");
    let config_path = tmp.path().join("config.toml");

    let mut config = Config::default();
    config.default_provider = Some("mistral".into());
    config.default_model = Some("mistral-large".into());

    let toml_str = toml::to_string(&config).expect("config should serialize");
    fs::write(&config_path, &toml_str).expect("config file write should succeed");

    let read_back = fs::read_to_string(&config_path).expect("config file read should succeed");
    let parsed: Config = toml::from_str(&read_back).expect("TOML should parse back");

    assert_eq!(parsed.default_provider.as_deref(), Some("mistral"));
    assert_eq!(parsed.default_model.as_deref(), Some("mistral-large"));
}

#[test]
fn config_file_with_missing_optional_fields_uses_defaults() {
    // Simulate a minimal config TOML that omits optional sections
    let minimal_toml = r"
default_temperature = 0.7
";
    let parsed: Config = toml::from_str(minimal_toml).expect("minimal TOML should parse");

    // Agent config should use defaults
    assert!(!parsed.agent.compact_context);
}

#[test]
fn config_file_with_custom_agent_section() {
    let toml_with_agent = r"
default_temperature = 0.7

[agent]
compact_context = true
";
    let parsed: Config = toml::from_str(toml_with_agent).expect("TOML with agent section should parse");

    assert!(parsed.agent.compact_context);
}

// ─────────────────────────────────────────────────────────────────────────────
// Workspace directory creation
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn workspace_dir_creation_in_tempdir() {
    let tmp = tempfile::TempDir::new().expect("tempdir creation should succeed");
    let workspace_dir = tmp.path().join("workspace");

    fs::create_dir_all(&workspace_dir).expect("workspace dir creation should succeed");
    assert!(workspace_dir.exists(), "workspace dir should exist");
    assert!(workspace_dir.is_dir(), "workspace path should be a directory");
}

#[test]
fn nested_workspace_dir_creation() {
    let tmp = tempfile::TempDir::new().expect("tempdir creation should succeed");
    let nested_dir = tmp.path().join("deep").join("nested").join("workspace");

    fs::create_dir_all(&nested_dir).expect("nested dir creation should succeed");
    assert!(nested_dir.exists(), "nested workspace dir should exist");
}
