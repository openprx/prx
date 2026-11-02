#![cfg(test)]

//! Partial sections (PRX-CONFIG-PARTIAL-01, observation O6).
//!
//! Sections with defaults accept partial settings; identities, credentials,
//! endpoints and required allowlists remain explicit. An omitted defaulted
//! field takes the same value the section's `impl Default` produces. These tests pin that agreement, because a serde field default and
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

/// Load `text` as a `config.toml` through the real loader, the way a start-up
/// reads it (merge, legacy migration, unknown-path check, validation).
fn load(text: &str) -> (tempfile::TempDir, Result<Config>) {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).expect("test: write config");
    let loaded = Config::load_from_path(&path, dir.path().join("workspace"));
    (dir, loaded)
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
    assert_empty_section_matches_default::<ChannelsConfig>("channels_config");
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

/// Item 2, through the real loader: the `embedding_provider` presence bit the
/// comment in `memory.rs` describes is filled in from the merged tree, so a
/// `[memory]` section that writes only `backend` must leave it `false`, and a
/// section that writes the key must set it.
///
/// The bit is `#[serde(default, skip_serializing)]`, so a serde-level test
/// cannot fail; only the loader path proves the logic.
#[test]
fn a_partial_memory_section_leaves_the_presence_bit_false_through_the_loader() {
    let (_dir, loaded) = load("default_temperature = 0.7\n\n[memory]\nbackend = \"sqlite\"\n");
    let loaded = loaded.expect("test: a backend-only [memory] loads");
    assert_eq!(loaded.memory.backend, "sqlite");
    assert_eq!(loaded.memory.auto_save, MemoryConfig::default().auto_save);
    assert!(
        !loaded.memory.embedding_provider_explicit,
        "the operator never wrote embedding_provider, so the loader must not claim it was written"
    );
    assert_eq!(
        loaded.memory.embedding_provider,
        MemoryConfig::default().embedding_provider,
        "an omitted embedding_provider takes the Default value"
    );

    let explicit = format!(
        "[memory]\nbackend = \"sqlite\"\nembedding_provider = {:?}\n",
        MemoryConfig::default().embedding_provider
    );
    let (_dir, loaded) = load(&explicit);
    let loaded = loaded.expect("test: a [memory] naming embedding_provider loads");
    assert!(
        loaded.memory.embedding_provider_explicit,
        "the operator wrote the key, so the loader must record it"
    );
}

/// M2: `[channels_config]` may be written partially. The realistic shape is a
/// single channel sub-section with no `cli` line, which used to fail the load
/// with `missing field cli`.
#[test]
fn a_channels_config_with_only_a_telegram_section_loads() {
    let text = "default_temperature = 0.7\n\n\
                [channels_config.telegram]\nbot_token = \"000000000:test-token\"\nallowed_users = [\"prx_user\"]\n";
    let (_dir, loaded) = load(text);
    let loaded = loaded.expect("test: a [channels_config.telegram]-only config loads");
    assert!(
        loaded.channels_config.cli,
        "an omitted cli keeps the Default value, true"
    );
    assert_eq!(
        loaded
            .channels_config
            .telegram
            .as_ref()
            .map(|telegram| telegram.allowed_users.len()),
        Some(1),
        "the channel sub-section is read"
    );
}

/// Item 1: the top-level scalars are part of the same contract, so a
/// `config.toml` that names nothing at all still parses.
///
/// `default_provider` and `default_model` stay `None`: the release catalog
/// owns the release default, so an omitted key is not pinned into the file.
#[test]
fn an_empty_config_file_parses() {
    let parsed: Config = toml::from_str("").expect("test: an empty config parses");

    assert_eq!(parsed.default_temperature, 0.7);
    assert!(parsed.default_provider.is_none());
    assert!(parsed.default_model.is_none());
    assert!(parsed.api_key.is_none());
    assert!(parsed.api_url.is_none());
}

#[tokio::test]
async fn omitted_provider_and_model_stay_unset_after_a_structured_save() {
    for text in ["", "default_provider = \"ollama\"\n"] {
        let (dir, loaded) = load(text);
        let loaded = loaded.expect("test: optional defaults load");
        let provider = loaded.default_provider.clone();
        assert!(loaded.default_model.is_none());
        loaded.save().await.expect("test: structured save");
        let reloaded = Config::load_from_path(&dir.path().join("config.toml"), dir.path().join("workspace"))
            .expect("test: reload saved config");
        assert_eq!(reloaded.default_provider, provider);
        assert!(reloaded.default_model.is_none(), "saving must not pin a catalog model");
        if text.is_empty() {
            assert!(
                reloaded.default_provider.is_none(),
                "saving must not pin a catalog provider"
            );
        }
    }
}

#[test]
fn partial_channels_still_require_credentials_and_allowlists() {
    for (text, field) in [
        (
            "[channels_config.telegram]\nbot_token = \"test-token\"\n",
            "allowed_users",
        ),
        ("[channels_config.telegram]\nallowed_users = []\n", "bot_token"),
        ("[channels_config.imessage]\nmention_only = true\n", "allowed_contacts"),
    ] {
        let (_dir, loaded) = load(text);
        let error = format!("{:#}", loaded.expect_err("test: required boundary field"));
        assert!(error.contains(&format!("missing field `{field}`")), "{error}");
    }
}

/// The three O6 cases in one document, read through the real loader: a
/// partial `[autonomy]`, a partial `[memory]`, and a bare `[causal_tree]
/// enabled = false`, which D-074 reads as unset and the load reports.
#[test]
fn the_three_reported_partial_sections_load_together() {
    let text = "default_temperature = 0.7\n\n\
                [autonomy]\nlevel = \"full\"\n\n\
                [memory]\nbackend = \"sqlite\"\n\n\
                [causal_tree]\nenabled = false\n";
    let (_dir, loaded) = load(text);
    let loaded = loaded.expect("test: the three reported partial sections load");

    assert_eq!(loaded.autonomy.level, AutonomyLevel::Full);
    assert!(loaded.autonomy.workspace_only, "autonomy stays closed");
    assert_eq!(
        loaded.autonomy.forbidden_paths,
        AutonomyConfig::default().forbidden_paths
    );
    assert_eq!(loaded.memory.backend, "sqlite");
    assert!(loaded.memory.auto_save);
    assert!(
        loaded.causal_tree.enabled,
        "D-074: a bare unversioned disable is read as unset, so the plane keeps its default"
    );
}
