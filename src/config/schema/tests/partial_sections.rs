#![cfg(test)]

//! Partial sections (PRX-CONFIG-PARTIAL-01, observation O6).
//!
//! Every section reachable from [`Config`] may be written partially: a key the
//! operator leaves out takes the same value the section's `impl Default`
//! produces. These tests pin that agreement, because a serde field default and
//! the hand-written `Default` are two spellings of one decision and drift
//! silently otherwise.

use super::*;

/// Whether a `[section]` that names no key deserializes to the section's
/// `Default`.
///
/// `toml::from_str::<T>("")` reads an empty table, which is exactly what
/// `[section]` with no keys below it is. The two values are compared through
/// their serialized form because these sections do not derive `PartialEq`,
/// and adding a derive only for a test would widen the public API.
fn assert_empty_section_matches_default<T>(name: &str)
where
    T: serde::de::DeserializeOwned + Default + serde::Serialize,
{
    let parsed: T = toml::from_str("").unwrap_or_else(|error| panic!("{name}: an empty section must parse: {error}"));
    let parsed = toml::Value::try_from(&parsed).unwrap_or_else(|error| panic!("{name}: serializes: {error}"));
    let defaults = toml::Value::try_from(T::default()).unwrap_or_else(|error| panic!("{name}: serializes: {error}"));
    assert_eq!(
        parsed, defaults,
        "{name}: a section that names no key must equal its Default"
    );
}

/// Item 1: every section this card touched agrees with its `Default`.
#[test]
fn every_touched_section_defaults_to_its_default_impl() {
    assert_empty_section_matches_default::<AutonomyConfig>("autonomy");
    assert_empty_section_matches_default::<ScopeConfig>("autonomy.scopes");
    assert_empty_section_matches_default::<MemoryConfig>("memory");
    assert_empty_section_matches_default::<MemoryEventsConfig>("memory.events");
    assert_empty_section_matches_default::<MemorySemanticConfig>("memory.semantic");
    assert_empty_section_matches_default::<ObservabilityConfig>("observability");
    assert_empty_section_matches_default::<TunnelConfig>("tunnel");
    assert_empty_section_matches_default::<HeartbeatConfig>("heartbeat");
}

/// Item 1, the security half: a partial `[autonomy]` fails closed.
///
/// The three keys whose absence could widen the posture keep the safe value
/// rather than an empty one, so an operator who writes only `level` never
/// silently loses the workspace restriction or the path denylist.
#[test]
fn a_partial_autonomy_section_fails_closed() {
    let parsed: AutonomyConfig = toml::from_str("level = \"full\"").expect("test: partial autonomy parses");

    assert!(parsed.workspace_only, "workspace_only must stay true when omitted");
    assert_eq!(
        parsed.forbidden_paths,
        AutonomyConfig::default().forbidden_paths,
        "forbidden_paths must keep the built-in list when omitted"
    );
    assert!(
        !parsed.acknowledge_unrestricted_profile,
        "acknowledge_unrestricted_profile must stay false when omitted"
    );
    assert_eq!(parsed.max_cost_per_day_cents, 500);
    assert_eq!(parsed.level, AutonomyLevel::Full);
}

/// Item 2: `[memory]` with only `backend` loads, and the omitted keys take
/// `MemoryConfig::default()`.
#[test]
fn a_memory_section_with_only_a_backend_loads() {
    let parsed: MemoryConfig = toml::from_str("backend = \"sqlite\"").expect("test: partial memory parses");

    assert_eq!(parsed.backend, "sqlite");
    assert!(parsed.auto_save, "auto_save must default to the Default value");
    let defaults = MemoryConfig::default();
    assert_eq!(parsed.backend, defaults.backend);
    assert_eq!(parsed.auto_save, defaults.auto_save);
    assert_eq!(parsed.embedding_provider, defaults.embedding_provider);
    assert_eq!(parsed.archive_after_days, defaults.archive_after_days);
}

/// Item 2: the presence bit the comment in `memory.rs` describes is filled in
/// by the loader from the merged tree, not by serde, so a partial `[memory]`
/// section must still leave it `false` after deserialization.
#[test]
fn a_partial_memory_section_leaves_the_presence_bit_to_the_loader() {
    let parsed: MemoryConfig = toml::from_str("backend = \"sqlite\"").expect("test: partial memory parses");
    assert!(
        !parsed.embedding_provider_explicit,
        "serde must not claim the operator wrote embedding_provider"
    );
    assert_eq!(
        parsed.embedding_provider,
        MemoryConfig::default().embedding_provider,
        "an omitted embedding_provider takes the Default value"
    );
}

/// Item 1: the top-level scalars are part of the same contract, so a
/// `config.toml` that names nothing at all still parses.
#[test]
fn an_empty_config_file_parses() {
    let parsed: Config = toml::from_str("").expect("test: an empty config parses");
    let defaults = Config::default();

    assert_eq!(parsed.default_temperature, defaults.default_temperature);
    assert_eq!(parsed.default_provider, defaults.default_provider);
    assert_eq!(parsed.default_model, defaults.default_model);
    assert_eq!(parsed.api_key, defaults.api_key);
    assert_eq!(parsed.api_url, defaults.api_url);
}

/// The full `Config` from a file that writes only the sections from O6: the
/// three reported failures in one document.
#[test]
fn the_reported_partial_sections_load_together() {
    let text = r#"
[autonomy]
level = "full"

[memory]
backend = "sqlite"
"#;
    let parsed: Config = toml::from_str(text).expect("test: the reported partial sections load");

    assert_eq!(parsed.autonomy.level, AutonomyLevel::Full);
    assert!(parsed.autonomy.workspace_only);
    assert_eq!(parsed.memory.backend, "sqlite");
    assert!(parsed.memory.auto_save);
}
