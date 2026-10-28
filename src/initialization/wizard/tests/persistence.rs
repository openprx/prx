#![cfg(test)]

use super::*;

#[tokio::test]
async fn onboarding_model_and_level_commit_through_history_in_both_file_layouts() {
    use crate::config::manage::{ConfigActor, ConfigTransactions, RecordStatus};
    for split in [false, true] {
        let dir = TempDir::new().unwrap();
        let settings = serde_json::from_value(json!({"level":"high","reasoning_effort":"low"})).unwrap();
        let config = Config {
            config_path: dir.path().join("config.toml"),
            workspace_dir: dir.path().join("workspace"),
            default_provider: Some("openai".into()),
            default_model: Some("gpt-5.6".into()),
            model_settings: [("openai/gpt-5.6".into(), settings)].into(),
            ..Default::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        persist_onboarding_config(&config, split).await.unwrap();
        let loaded = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).unwrap();
        assert_eq!(loaded.default_model, config.default_model);
        assert_eq!(loaded.model_settings, config.model_settings);
        let transactions = ConfigTransactions::new(crate::config::new_shared(loaded));
        let records = transactions.history().unwrap().list().unwrap();
        assert_eq!(records.len(), 1);
        let record = records.first().unwrap();
        assert_eq!(record.actor, ConfigActor::Cli);
        assert_eq!(record.status, RecordStatus::ActivationDeferred);
        assert!(record.committed_revision.is_some());
    }
}

#[tokio::test]
async fn quick_setup_model_override_persists_to_config_toml() {
    let tmp = TempDir::new().unwrap();

    let config = run_quick_setup_with_home(
        Some("sk-issue946"),
        Some("openrouter"),
        Some("openai/gpt-5-mini"),
        Some("sqlite"),
        None,
        tmp.path(),
    )
    .await
    .unwrap();

    assert_eq!(config.default_provider.as_deref(), Some("openrouter"));
    assert_eq!(config.default_model.as_deref(), Some("openai/gpt-5-mini"));
    assert_eq!(config.api_key.as_deref(), Some("sk-issue946"));

    let config_raw = tokio::fs::read_to_string(config.config_path).await.unwrap();
    assert!(config_raw.contains("default_provider = \"openrouter\""));
    assert!(config_raw.contains("default_model = \"openai/gpt-5-mini\""));
}

/// INT-2B B5 (D-070 amendment), RED before: `prx init --provider
/// llamacpp` without `-m` chose `ggml-org/gpt-oss-20b-GGUF`, which the
/// provider/model check refused ("not compatible with provider
/// 'llamacpp'"), so quick setup failed. llama.cpp serves whatever model it
/// loaded, under a Hugging Face `org/model` id, so the default and every
/// curated choice are accepted now.
#[tokio::test]
async fn quick_setup_for_llamacpp_without_a_model_chooses_a_compatible_default() {
    let tmp = TempDir::new().unwrap();
    let config = run_quick_setup_with_home(None, Some("llamacpp"), None, Some("sqlite"), None, tmp.path())
        .await
        .expect("test: llamacpp quick setup without -m succeeds");
    assert_eq!(config.default_provider.as_deref(), Some("llamacpp"));
    let model = config.default_model.unwrap_or_default();
    assert_eq!(model, default_model_for_provider("llamacpp"));
    for (curated, _) in catalog_models_for_provider("llamacpp") {
        crate::providers::validate_provider_model("llamacpp", &curated)
            .unwrap_or_else(|error| panic!("curated llama.cpp model {curated} is refused: {error:#}"));
    }
}

/// R-PMMCh C-2: a padded `--model` is stored as the resolver reads it.
#[tokio::test]
async fn quick_setup_stores_the_model_trimmed() {
    let tmp = TempDir::new().unwrap();
    let config = run_quick_setup_with_home(
        Some("sk-issue946"),
        Some("openrouter"),
        Some("  openai/gpt-5-mini \t"),
        Some("sqlite"),
        None,
        tmp.path(),
    )
    .await
    .unwrap();
    assert_eq!(config.default_model.as_deref(), Some("openai/gpt-5-mini"));
    let config_raw = tokio::fs::read_to_string(config.config_path).await.unwrap();
    assert!(
        config_raw.contains("default_model = \"openai/gpt-5-mini\""),
        "{config_raw}"
    );
}

/// D-034: quick setup is an operator selection. A model PRX has no
/// evidence for is accepted and marked unverified (a fresh install has
/// no catalog yet); only negative evidence refuses, and nothing is then
/// written.
#[tokio::test]
async fn quick_setup_accepts_an_unverified_model_and_refuses_only_negative_evidence() {
    let tmp = TempDir::new().unwrap();
    let config = run_quick_setup_with_home(
        Some("sk-issue946"),
        Some("openrouter"),
        Some("custom-model-946"),
        Some("sqlite"),
        None,
        tmp.path(),
    )
    .await
    .expect("an unverified model is accepted at onboarding");
    assert_eq!(config.default_model.as_deref(), Some("custom-model-946"));

    let local = TempDir::new().unwrap();
    run_quick_setup_with_home(
        None,
        Some("custom:http://127.0.0.1:9/v1"),
        Some("my-local-model"),
        Some("sqlite"),
        None,
        local.path(),
    )
    .await
    .expect("a local endpoint's model is accepted");

    let refused = TempDir::new().unwrap();
    let error = run_quick_setup_with_home(
        Some("sk-issue946"),
        Some("anthropic"),
        Some("openai/gpt-4o"),
        Some("sqlite"),
        None,
        refused.path(),
    )
    .await
    .expect_err("a model the provider cannot serve is refused");
    let text = format!("{error:#}");
    assert!(
        !text.contains("prx models refresh"),
        "no chicken-and-egg advice on a fresh install: {text}"
    );
    assert!(!text.contains("sk-issue946"), "{text}");
    assert!(
        !default_user_config_dir(refused.path()).join("config.toml").exists(),
        "a refused quick setup wrote config.toml"
    );
}

#[tokio::test]
async fn quick_setup_without_model_uses_provider_default_model() {
    let tmp = TempDir::new().unwrap();

    let config = run_quick_setup_with_home(
        Some("sk-issue946"),
        Some("anthropic"),
        None,
        Some("sqlite"),
        None,
        tmp.path(),
    )
    .await
    .unwrap();

    let expected = default_model_for_provider("anthropic");
    assert_eq!(config.default_provider.as_deref(), Some("anthropic"));
    assert_eq!(config.default_model.as_deref(), Some(expected.as_str()));
}
