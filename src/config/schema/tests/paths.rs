#![cfg(test)]

use super::*;
use tokio::test;

#[test]
async fn config_dir_creation_error_mentions_openrc_and_path() {
    let msg = config_dir_creation_error(Path::new("/etc/prx"));
    assert!(msg.contains("/etc/prx"));
    assert!(msg.contains("OpenRC"));
    assert!(msg.contains("prx"));
}

#[test]
async fn load_path_marks_compaction_max_context_presence_only_when_explicit() {
    let dir = tempfile::TempDir::new().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
default_temperature = 0.7

[agent.compaction]
mode = "safeguard"

[router]

[[router.models]]
model_id = "wide"
provider = "openrouter"
max_context = 1000000
"#,
    )
    .unwrap();

    let loaded = Config::load_from_path(&config_path, dir.path().join("workspace")).unwrap();
    assert!(!loaded.agent.compaction.max_context_tokens_explicit);

    std::fs::write(
        &config_path,
        r#"
default_temperature = 0.7

[agent.compaction]
mode = "safeguard"
max_context_tokens = 128000

[router]

[[router.models]]
model_id = "wide"
provider = "openrouter"
max_context = 1000000
"#,
    )
    .unwrap();

    let loaded = Config::load_from_path(&config_path, dir.path().join("workspace")).unwrap();
    assert!(loaded.agent.compaction.max_context_tokens_explicit);
    assert_eq!(loaded.agent.compaction.max_context_tokens, 128_000);
}

#[test]
async fn load_path_treats_the_pre_catalog_serialized_latency_default_as_unset() {
    let dir = tempfile::TempDir::new().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
default_temperature = 0.7

[causal_tree]
schema_version = 1
enabled = true
"#,
    )
    .unwrap();

    let loaded = Config::load_from_path(&config_path, dir.path().join("workspace")).unwrap();
    assert!(!loaded.causal_tree.policy.extra_latency_budget_ms_explicit);

    std::fs::write(
        &config_path,
        r#"
default_temperature = 0.7

[causal_tree]
schema_version = 1
enabled = true

[causal_tree.policy]
extra_latency_budget_ms = 500
"#,
    )
    .unwrap();

    let loaded = Config::load_from_path(&config_path, dir.path().join("workspace")).unwrap();
    assert!(!loaded.causal_tree.policy.extra_latency_budget_ms_explicit);
    assert_eq!(loaded.causal_tree.policy.extra_latency_budget_ms, 500);

    let upgraded = crate::config::manage::mutation::causal::CausalConfigService::mutate(
        &loaded,
        &[crate::config::manage::mutation::causal::CausalMutation::SetBackend {
            kind: crate::decision::BackendKind::OpenRouter,
            endpoint: None,
            model: Some("typesafe/jev-1.13".to_owned()),
            pinned_snapshot: None,
            timeout_ms: None,
        }],
    )
    .expect("a stored old default adopts the hosted catalog budget");
    assert_eq!(upgraded.causal_tree.policy.extra_latency_budget_ms, 3_000);
    assert!(upgraded.causal_tree.policy.extra_latency_budget_ms_explicit);
}

#[tokio::test]
async fn sync_directory_handles_existing_directory() {
    let dir = std::env::temp_dir().join(format!("openprx_test_sync_directory_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).await.unwrap();

    sync_directory(&dir).await.unwrap();

    let _ = fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn config_load_from_path_supports_single_file() {
    let dir = std::env::temp_dir().join(format!("openprx_test_single_file_load_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).await.unwrap();

    let config_path = dir.join("config.toml");
    fs::write(
        &config_path,
        r#"
default_temperature = 0.7
default_model = "single-file"
"#,
    )
    .await
    .unwrap();

    let loaded = Config::load_from_path(&config_path, dir.join("workspace")).unwrap();
    assert_eq!(loaded.default_model.as_deref(), Some("single-file"));
    assert_eq!(loaded.memory.backend, "sqlite");

    let _ = fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn config_load_from_path_merges_config_dir_and_replaces_arrays() {
    let dir = std::env::temp_dir().join(format!("openprx_test_config_merge_{}", uuid::Uuid::new_v4()));
    let config_dir = dir.join("config.d");
    fs::create_dir_all(&config_dir).await.unwrap();

    let config_path = dir.join("config.toml");
    fs::write(
        &config_path,
        r#"
default_temperature = 0.7
default_model = "base-model"

[[model_routes]]
hint = "alpha"
provider = "openrouter"
model = "base-alpha"

[memory]
backend = "sqlite"
auto_save = true
acl_enabled = false
archive_after_days = 7
purge_after_days = 30
conversation_retention_days = 3
daily_retention_days = 7
embedding_provider = "base"
embedding_model = "text-embedding-3-small"
embedding_dimensions = 1536
vector_weight = 0.7
keyword_weight = 0.3
min_relevance_score = 0.4
embedding_cache_size = 1000
auto_hydrate = true
"#,
    )
    .await
    .unwrap();
    fs::write(
        config_dir.join("memory.toml"),
        r#"
[memory]
embedding_provider = "override"
"#,
    )
    .await
    .unwrap();
    fs::write(
        config_dir.join("routing.toml"),
        r#"
default_model = "fragment-model"

[[model_routes]]
hint = "beta"
provider = "anthropic"
model = "override-beta"
"#,
    )
    .await
    .unwrap();

    let loaded = Config::load_from_path(&config_path, dir.join("workspace")).unwrap();
    assert_eq!(loaded.default_model.as_deref(), Some("fragment-model"));
    assert_eq!(loaded.model_routes.len(), 1);
    assert_eq!(loaded.model_routes[0].hint, "beta");
    assert_eq!(loaded.memory.embedding_provider, "override");

    let _ = fs::remove_dir_all(&dir).await;
}

#[test]
async fn resolve_runtime_config_dirs_uses_env_workspace_first() {
    let _env_guard = env_override_lock().await;
    let default_config_dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
    let default_workspace_dir = default_config_dir.join("workspace");
    let workspace_dir = default_config_dir.join("profile-a");

    test_set_env("OPENPRX_WORKSPACE", &workspace_dir);
    let (config_dir, resolved_workspace_dir, source) =
        resolve_runtime_config_dirs(&default_config_dir, &default_workspace_dir, None)
            .await
            .unwrap();

    assert_eq!(source, ConfigResolutionSource::EnvWorkspace);
    assert_eq!(config_dir, workspace_dir);
    assert_eq!(resolved_workspace_dir, workspace_dir.join("workspace"));

    test_remove_env("OPENPRX_WORKSPACE");
    let _ = fs::remove_dir_all(default_config_dir).await;
}

#[test]
async fn resolve_runtime_config_dirs_uses_env_config_dir_first() {
    let _env_guard = env_override_lock().await;
    let default_config_dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
    let default_workspace_dir = default_config_dir.join("workspace");
    let explicit_config_dir = default_config_dir.join("explicit-config");
    let marker_config_dir = default_config_dir.join("profiles").join("alpha");
    let state_path = default_config_dir.join(ACTIVE_WORKSPACE_STATE_FILE);

    fs::create_dir_all(&default_config_dir).await.unwrap();
    let state = ActiveWorkspaceState {
        config_dir: marker_config_dir.to_string_lossy().into_owned(),
    };
    fs::write(&state_path, toml::to_string(&state).unwrap()).await.unwrap();

    test_set_env("OPENPRX_CONFIG_DIR", &explicit_config_dir);
    test_remove_env("OPENPRX_WORKSPACE");

    let (config_dir, resolved_workspace_dir, source) =
        resolve_runtime_config_dirs(&default_config_dir, &default_workspace_dir, None)
            .await
            .unwrap();

    assert_eq!(source, ConfigResolutionSource::EnvConfigDir);
    assert_eq!(config_dir, explicit_config_dir);
    assert_eq!(resolved_workspace_dir, explicit_config_dir.join("workspace"));

    test_remove_env("OPENPRX_CONFIG_DIR");
    let _ = fs::remove_dir_all(default_config_dir).await;
}

#[test]
async fn relative_runtime_overrides_are_anchored_before_subsystems_join_paths() {
    let _env_guard = env_override_lock().await;
    let defaults = std::env::temp_dir().join("prx-path-defaults");
    let relative = "prx-nonexistent-profile/config";
    let expected = std::env::current_dir().unwrap().join(relative);
    for source in ["flag", "OPENPRX_CONFIG_DIR", "OPENPRX_WORKSPACE"] {
        test_remove_env("OPENPRX_CONFIG_DIR");
        test_remove_env("OPENPRX_WORKSPACE");
        let flag = if source == "flag" {
            Some(relative)
        } else {
            test_set_env(source, relative);
            None
        };
        let (config, workspace, _) = resolve_runtime_config_dirs(&defaults, &defaults.join("workspace"), flag)
            .await
            .unwrap();
        assert_eq!(config, expected);
        assert_eq!(workspace, expected.join("workspace"));
        assert!(workspace.join("self/evolution/analysis").is_absolute());
    }
    test_remove_env("OPENPRX_CONFIG_DIR");
    test_remove_env("OPENPRX_WORKSPACE");
}

#[test]
async fn resolve_runtime_config_dirs_uses_active_workspace_marker() {
    let _env_guard = env_override_lock().await;
    let default_config_dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
    let default_workspace_dir = default_config_dir.join("workspace");
    let marker_config_dir = default_config_dir.join("profiles").join("alpha");
    let state_path = default_config_dir.join(ACTIVE_WORKSPACE_STATE_FILE);

    test_remove_env("OPENPRX_WORKSPACE");
    fs::create_dir_all(&default_config_dir).await.unwrap();
    let state = ActiveWorkspaceState {
        config_dir: marker_config_dir.to_string_lossy().into_owned(),
    };
    fs::write(&state_path, toml::to_string(&state).unwrap()).await.unwrap();

    let (config_dir, resolved_workspace_dir, source) =
        resolve_runtime_config_dirs(&default_config_dir, &default_workspace_dir, None)
            .await
            .unwrap();

    assert_eq!(source, ConfigResolutionSource::ActiveWorkspaceMarker);
    assert_eq!(config_dir, marker_config_dir);
    assert_eq!(resolved_workspace_dir, marker_config_dir.join("workspace"));

    let _ = fs::remove_dir_all(default_config_dir).await;
}

#[test]
async fn resolve_runtime_config_dirs_falls_back_to_default_layout() {
    let _env_guard = env_override_lock().await;
    let default_config_dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
    let default_workspace_dir = default_config_dir.join("workspace");

    test_remove_env("OPENPRX_WORKSPACE");
    let (config_dir, resolved_workspace_dir, source) =
        resolve_runtime_config_dirs(&default_config_dir, &default_workspace_dir, None)
            .await
            .unwrap();

    assert_eq!(source, ConfigResolutionSource::DefaultConfigDir);
    assert_eq!(config_dir, default_config_dir);
    assert_eq!(resolved_workspace_dir, default_workspace_dir);

    let _ = fs::remove_dir_all(default_config_dir).await;
}

#[test]
async fn load_or_init_workspace_override_uses_workspace_root_for_config() {
    let _env_guard = env_override_lock().await;
    let temp_home = std::env::temp_dir().join(format!("openprx_test_home_{}", uuid::Uuid::new_v4()));
    let workspace_dir = temp_home.join("profile-a");

    let original_home = std::env::var("HOME").ok();
    test_set_env("HOME", &temp_home);
    test_set_env("OPENPRX_WORKSPACE", &workspace_dir);

    let config = Config::load_or_init().await.unwrap();

    assert_eq!(config.workspace_dir, workspace_dir.join("workspace"));
    assert_eq!(config.config_path, workspace_dir.join("config.toml"));
    assert!(workspace_dir.join("config.toml").exists());

    test_remove_env("OPENPRX_WORKSPACE");
    if let Some(home) = original_home {
        test_set_env("HOME", home);
    } else {
        test_remove_env("HOME");
    }
    let _ = fs::remove_dir_all(temp_home).await;
}

#[test]
async fn load_or_init_workspace_suffix_uses_legacy_config_layout() {
    let _env_guard = env_override_lock().await;
    let temp_home = std::env::temp_dir().join(format!("openprx_test_home_{}", uuid::Uuid::new_v4()));
    let workspace_dir = temp_home.join("workspace");
    let legacy_config_path = temp_home.join(".openprx").join("config.toml");

    let original_home = std::env::var("HOME").ok();
    test_set_env("HOME", &temp_home);
    test_set_env("OPENPRX_WORKSPACE", &workspace_dir);

    let config = Config::load_or_init().await.unwrap();

    assert_eq!(config.workspace_dir, workspace_dir);
    assert_eq!(config.config_path, legacy_config_path);
    assert!(config.config_path.exists());

    test_remove_env("OPENPRX_WORKSPACE");
    if let Some(home) = original_home {
        test_set_env("HOME", home);
    } else {
        test_remove_env("HOME");
    }
    let _ = fs::remove_dir_all(temp_home).await;
}

#[test]
async fn load_or_init_workspace_override_keeps_existing_legacy_config() {
    let _env_guard = env_override_lock().await;
    let temp_home = std::env::temp_dir().join(format!("openprx_test_home_{}", uuid::Uuid::new_v4()));
    let workspace_dir = temp_home.join("custom-workspace");
    let legacy_config_dir = temp_home.join(".openprx");
    let legacy_config_path = legacy_config_dir.join("config.toml");

    fs::create_dir_all(&legacy_config_dir).await.unwrap();
    fs::write(
        &legacy_config_path,
        r#"default_temperature = 0.7
default_model = "legacy-model"
"#,
    )
    .await
    .unwrap();

    let original_home = std::env::var("HOME").ok();
    test_set_env("HOME", &temp_home);
    test_set_env("OPENPRX_WORKSPACE", &workspace_dir);

    let config = Config::load_or_init().await.unwrap();

    assert_eq!(config.workspace_dir, workspace_dir);
    assert_eq!(config.config_path, legacy_config_path);
    assert_eq!(config.default_model.as_deref(), Some("legacy-model"));

    test_remove_env("OPENPRX_WORKSPACE");
    if let Some(home) = original_home {
        test_set_env("HOME", home);
    } else {
        test_remove_env("HOME");
    }
    let _ = fs::remove_dir_all(temp_home).await;
}

#[test]
async fn load_or_init_uses_persisted_active_workspace_marker() {
    let _env_guard = env_override_lock().await;
    let temp_home = std::env::temp_dir().join(format!("openprx_test_home_{}", uuid::Uuid::new_v4()));
    let custom_config_dir = temp_home.join("profiles").join("agent-alpha");

    fs::create_dir_all(&custom_config_dir).await.unwrap();
    fs::write(
        custom_config_dir.join("config.toml"),
        "default_temperature = 0.7\ndefault_model = \"persisted-profile\"\n",
    )
    .await
    .unwrap();

    let original_home = std::env::var("HOME").ok();
    test_set_env("HOME", &temp_home);
    test_remove_env("OPENPRX_WORKSPACE");

    persist_active_workspace_config_dir(&custom_config_dir).await.unwrap();

    let config = Config::load_or_init().await.unwrap();

    assert_eq!(config.config_path, custom_config_dir.join("config.toml"));
    assert_eq!(config.workspace_dir, custom_config_dir.join("workspace"));
    assert_eq!(config.default_model.as_deref(), Some("persisted-profile"));

    if let Some(home) = original_home {
        test_set_env("HOME", home);
    } else {
        test_remove_env("HOME");
    }
    let _ = fs::remove_dir_all(temp_home).await;
}

#[test]
async fn load_or_init_env_workspace_override_takes_priority_over_marker() {
    let _env_guard = env_override_lock().await;
    let temp_home = std::env::temp_dir().join(format!("openprx_test_home_{}", uuid::Uuid::new_v4()));
    let marker_config_dir = temp_home.join("profiles").join("persisted-profile");
    let env_workspace_dir = temp_home.join("env-workspace");

    fs::create_dir_all(&marker_config_dir).await.unwrap();
    fs::write(
        marker_config_dir.join("config.toml"),
        "default_temperature = 0.7\ndefault_model = \"marker-model\"\n",
    )
    .await
    .unwrap();

    let original_home = std::env::var("HOME").ok();
    test_set_env("HOME", &temp_home);
    persist_active_workspace_config_dir(&marker_config_dir).await.unwrap();
    test_set_env("OPENPRX_WORKSPACE", &env_workspace_dir);

    let config = Config::load_or_init().await.unwrap();

    assert_eq!(config.workspace_dir, env_workspace_dir.join("workspace"));
    assert_eq!(config.config_path, env_workspace_dir.join("config.toml"));

    test_remove_env("OPENPRX_WORKSPACE");
    if let Some(home) = original_home {
        test_set_env("HOME", home);
    } else {
        test_remove_env("HOME");
    }
    let _ = fs::remove_dir_all(temp_home).await;
}

#[test]
async fn persist_active_workspace_marker_is_cleared_for_default_config_dir() {
    let _env_guard = env_override_lock().await;
    let temp_home = std::env::temp_dir().join(format!("openprx_test_home_{}", uuid::Uuid::new_v4()));
    let default_config_dir = temp_home.join(".openprx");
    let custom_config_dir = temp_home.join("profiles").join("custom-profile");
    let marker_path = default_config_dir.join(ACTIVE_WORKSPACE_STATE_FILE);

    let original_home = std::env::var("HOME").ok();
    test_set_env("HOME", &temp_home);

    persist_active_workspace_config_dir(&custom_config_dir).await.unwrap();
    assert!(marker_path.exists());

    persist_active_workspace_config_dir(&default_config_dir).await.unwrap();
    assert!(!marker_path.exists());

    if let Some(home) = original_home {
        test_set_env("HOME", home);
    } else {
        test_remove_env("HOME");
    }
    let _ = fs::remove_dir_all(temp_home).await;
}

#[cfg(unix)]
#[tokio::test]
async fn write_toml_string_atomic_rejects_symlink_target() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::TempDir::new().unwrap();
    let real_path = tmp.path().join("real.toml");
    let symlink_path = tmp.path().join("config.toml");
    std::fs::write(&real_path, "default_temperature = 0.7\n").unwrap();
    symlink(&real_path, &symlink_path).unwrap();

    let error = write_toml_string_atomic(&symlink_path, "default_temperature = 1.0\n")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("symlink"), "unexpected error: {error}");
}

#[test]
async fn migration_read_only_config_load_does_not_initialize_missing_directory() {
    let temp = tempfile::TempDir::new().unwrap();
    let config_dir = temp.path().join("missing-config");
    let config_dir_arg = config_dir.to_string_lossy().to_string();

    let error = Config::load_existing_read_only_with_config_dir(Some(&config_dir_arg))
        .await
        .expect_err("missing config must be an explicit non-success outcome");

    assert!(error.to_string().contains("existing config is required"));
    assert!(
        !config_dir.exists(),
        "read-only loading must not create the config directory"
    );
    assert!(
        !config_dir.join("workspace").exists(),
        "read-only loading must not create the workspace directory"
    );
}

#[test]
async fn init_with_config_dir_writes_there_and_leaves_the_default_dir_byte_identical() {
    let _env_guard = env_override_lock().await;
    let temp_home = std::env::temp_dir().join(format!("openprx_test_inithome_{}", uuid::Uuid::new_v4()));
    let default_dir = temp_home.join(".openprx");
    let fragments_dir = default_dir.join("config.d");
    fs::create_dir_all(&fragments_dir).await.unwrap();
    fs::write(&default_dir.join("config.toml"), "default_temperature = 0.31\n")
        .await
        .unwrap();
    fs::write(&fragments_dir.join("agent.toml"), "[agent]\nmax_iterations = 41\n")
        .await
        .unwrap();
    let before = snapshot_dir(&default_dir);
    assert_eq!(before.len(), 2, "fixture must contain the files the regression rewrote");

    let requested = temp_home.join("scoped-init");
    let requested_arg = requested.to_string_lossy().into_owned();
    let original_home = std::env::var("HOME").ok();
    test_set_env("HOME", &temp_home);
    test_remove_env("OPENPRX_CONFIG_DIR");
    test_remove_env("OPENPRX_WORKSPACE");

    let target = resolve_init_target_dir(None, Some(&requested_arg)).await.unwrap();

    // The directory does not exist yet: `init` semantics create it.
    assert!(!requested.exists());
    let report = crate::config::init::Spec::Minimal
        .generate_reported(&target, true)
        .await
        .unwrap();

    restore_home(original_home);

    assert_eq!(
        snapshot_dir(&default_dir),
        before,
        "the default config dir must not be archived or rewritten"
    );
    assert!(!report.was_reset_to_templates());
    assert_eq!(target, requested, "--config-dir must select the init target");
    assert!(requested.join("config.toml").is_file(), "init must populate the target");
    assert!(requested.join("workspace").is_dir());
    assert!(
        !default_dir.join("workspace").exists(),
        "init must not scaffold a workspace in the default config dir"
    );
    let archives = std::fs::read_dir(&default_dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().contains("pre-"))
        .count();
    assert_eq!(archives, 0, "no archive of the default config may be created");

    let _ = fs::remove_dir_all(temp_home).await;
}

#[test]
async fn init_rejects_dir_and_config_dir_naming_different_directories() {
    let error = resolve_init_target_dir(Some("/tmp/openprx-a"), Some("/tmp/openprx-b"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("conflicting init targets"), "{error}");
    assert!(error.contains("/tmp/openprx-a"), "{error}");
    assert!(error.contains("/tmp/openprx-b"), "{error}");
}

#[test]
async fn init_accepts_dir_and_config_dir_naming_the_same_directory() {
    let target = resolve_init_target_dir(Some("/tmp/openprx-same"), Some("/tmp/openprx-same"))
        .await
        .unwrap();
    assert_eq!(target, PathBuf::from("/tmp/openprx-same"));
}

#[test]
async fn init_target_precedence_is_dir_then_config_dir_then_environment_then_default() {
    let _env_guard = env_override_lock().await;
    let temp_home = std::env::temp_dir().join(format!("openprx_test_initprec_{}", uuid::Uuid::new_v4()));
    let env_dir = temp_home.join("from-env");
    let cli_dir = temp_home.join("from-cli");
    let dir_arg = temp_home.join("from-dir");
    let env_arg = env_dir.to_string_lossy().into_owned();
    let cli_arg = cli_dir.to_string_lossy().into_owned();
    let dir_argument = dir_arg.to_string_lossy().into_owned();

    let original_home = std::env::var("HOME").ok();
    test_set_env("HOME", &temp_home);
    test_remove_env("OPENPRX_WORKSPACE");
    test_set_env("OPENPRX_CONFIG_DIR", &env_arg);

    let cli_over_env = resolve_init_target_dir(None, Some(&cli_arg)).await.unwrap();
    let dir_over_cli = resolve_init_target_dir(Some(&dir_argument), None).await.unwrap();
    let env_only = resolve_init_target_dir(None, None).await.unwrap();
    test_remove_env("OPENPRX_CONFIG_DIR");
    let default_only = resolve_init_target_dir(None, None).await.unwrap();
    restore_home(original_home);

    assert_eq!(cli_over_env, cli_dir, "--config-dir outranks OPENPRX_CONFIG_DIR");
    assert_eq!(dir_over_cli, dir_arg, "--dir outranks everything below it");
    assert_eq!(env_only, env_dir, "OPENPRX_CONFIG_DIR outranks the default dir");
    assert_eq!(default_only, temp_home.join(".openprx"), "default is ~/.openprx");

    let _ = fs::remove_dir_all(temp_home).await;
}

#[test]
async fn init_target_rejects_blank_directory_arguments() {
    let dir_error = resolve_init_target_dir(Some("   "), None)
        .await
        .unwrap_err()
        .to_string();
    assert!(dir_error.contains("--dir cannot be empty"), "{dir_error}");
    let config_dir_error = resolve_init_target_dir(None, Some("")).await.unwrap_err().to_string();
    assert!(
        config_dir_error.contains("--config-dir cannot be empty"),
        "{config_dir_error}"
    );
}

#[test]
async fn explicit_config_dir_keeps_every_config_derived_runtime_path_inside_it() {
    let _env_guard = env_override_lock().await;
    let temp_home = std::env::temp_dir().join(format!("openprx_test_scoped_{}", uuid::Uuid::new_v4()));
    let scoped_dir = temp_home.join("scoped");
    let scoped_arg = scoped_dir.to_string_lossy().into_owned();
    fs::create_dir_all(&temp_home).await.unwrap();

    let original_home = std::env::var("HOME").ok();
    test_set_env("HOME", &temp_home);
    test_remove_env("OPENPRX_CONFIG_DIR");
    test_remove_env("OPENPRX_WORKSPACE");

    let config = Config::load_or_init_with_config_dir(Some(&scoped_arg)).await.unwrap();

    restore_home(original_home);

    assert!(config.config_path.starts_with(&scoped_dir), "{:?}", config.config_path);
    assert!(
        config.workspace_dir.starts_with(&scoped_dir),
        "{:?}",
        config.workspace_dir
    );
    // Every dispatched subcommand derives its writes from these two paths;
    // the daemon runtime files are the ones an operator notices first.
    assert!(crate::daemon::state_file_path(&config).starts_with(&scoped_dir));
    assert!(crate::daemon::lock_file_path(&config).starts_with(&scoped_dir));
    assert!(
        !temp_home.join(".openprx").exists(),
        "a scoped run must not create the default config dir"
    );

    let _ = fs::remove_dir_all(temp_home).await;
}

/// D-070, RED before: `Config::default()` and the key-material fallbacks
/// resolved the operator's real `~/.openprx`, so a lib test that did not
/// point its config at a temporary directory read the real auth profiles
/// and wrote the session-worker secret, the witness key, the audit log and
/// the tool ledger into the real home. After: the lib-test binary resolves
/// every default location under the system temp directory.
#[test]
async fn the_lib_tests_never_resolve_the_real_home() {
    let temp = std::env::temp_dir();
    let config = Config::default();
    assert!(
        config.config_path.starts_with(&temp),
        "{}",
        config.config_path.display()
    );
    assert!(
        config.workspace_dir.starts_with(&temp),
        "{}",
        config.workspace_dir.display()
    );
    let home = user_home_dir().expect("test: a home is always resolved");
    assert!(home.starts_with(&temp), "{}", home.display());
    // Meaningful under the guard too, which starts the run with `HOME`
    // itself under the temp directory (D-072 L-8).
    assert_ne!(
        Some(home.as_os_str()),
        original_process_home(),
        "the process's own HOME"
    );
    let openprx = default_openprx_dir().expect("test: a default dir is always resolved");
    assert!(openprx.starts_with(&temp), "{}", openprx.display());
    assert!(default_config_dir().is_ok_and(|dir| dir.starts_with(&temp)));
    // D-071: other tools' credential stores and `~` paths as well.
    assert!(expand_home(Path::new("~/.claude/.credentials.json")).starts_with(&temp));
    assert!(platform_config_dir().is_some_and(|dir| dir.starts_with(&temp)));
}

#[test]
async fn expand_home_replaces_only_a_leading_tilde_component() {
    let home = user_home_dir().expect("test: a home is always resolved");
    assert_eq!(expand_home(Path::new("~")), home);
    assert_eq!(
        expand_home(Path::new("~/.codex/auth.json")),
        home.join(".codex/auth.json")
    );
    assert_eq!(expand_home(Path::new("~user/x")), PathBuf::from("~user/x"));
    assert_eq!(expand_home(Path::new("/etc/~/x")), PathBuf::from("/etc/~/x"));
    assert_eq!(expand_home(Path::new("relative/~")), PathBuf::from("relative/~"));
}

/// D-070: the guard runs the suite with `HOME` pointed under the temp
/// directory. A test that restores that `HOME` must not install it as its
/// own home, or every later test would write into it.
#[test]
async fn restoring_the_process_home_never_installs_it() {
    let _env_guard = env_override_lock().await;
    let original = std::env::var_os("HOME");
    follow_test_home_env(original.as_deref());
    let resolved = user_home_dir();
    follow_test_home_env(None);
    assert!(
        original.is_none_or(|home| resolved != Some(PathBuf::from(home))),
        "{resolved:?}"
    );
}

#[test]
async fn process_config_dir_keeps_the_first_published_directory() {
    let _lock = process_config_dir_test_lock();
    let previous = swap_process_config_dir_for_tests(None);
    set_process_config_dir(PathBuf::from("/tmp/openprx-first"));
    set_process_config_dir(PathBuf::from("/tmp/openprx-second"));
    let observed = process_config_dir();
    swap_process_config_dir_for_tests(previous);
    assert_eq!(observed, Some(PathBuf::from("/tmp/openprx-first")));
}

/// INT-2B CONSOLE-OBS-2 (D-056 DC-3), RED before: the default
/// `auth.codex_auth_json_path` was the absolute home path, which a save
/// wrote into `identity.toml` and the raw file API served. After: the
/// default is stored as `~/.codex/auth.json` and expanded when read.
#[test]
async fn the_default_codex_auth_path_is_stored_home_relative() {
    let stored = Config::default().to_stored_toml_string().expect("test: stored form");
    assert!(
        stored.contains("codex_auth_json_path = \"~/.codex/auth.json\""),
        "{stored}"
    );
    if let Some(home) = directories::UserDirs::new().map(|dirs| dirs.home_dir().display().to_string()) {
        assert!(!stored.contains(&format!("{home}/.codex")), "{stored}");
    }
}
