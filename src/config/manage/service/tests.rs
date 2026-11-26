use super::*;
use crate::config::DelegateAgentConfig;

/// A credential-shaped sentinel. Not a real secret.
const SECRET: &str = "pmme-service-sentinel";

async fn loaded(dir: &std::path::Path, edit: impl FnOnce(&mut Config)) -> Config {
    let mut config = Config {
        config_path: dir.join("config.toml"),
        workspace_dir: dir.join("workspace"),
        default_provider: Some("openai".into()),
        default_model: Some("gpt-4o".into()),
        ..Config::default()
    };
    edit(&mut config);
    config.save().await.expect("test: save");
    Config::load_from_path(&config.config_path, config.workspace_dir.clone()).expect("test: load")
}

fn service_for(config: Config) -> ManagementService {
    ManagementService::new(crate::config::new_shared(config))
}

fn route(hint: &str, provider: &str, model: &str) -> ModelRouteConfig {
    ModelRouteConfig {
        settings: Default::default(),
        hint: hint.into(),
        provider: provider.into(),
        model: model.into(),
        api_key: None,
        credential_ref: None,
    }
}

/// D-094 C-3: the decision backend is a use of its provider (a removal
/// is refused naming it), and a reference it names is reported as used
/// by it; any other reference is not used.
#[tokio::test]
async fn the_decision_backend_uses_its_provider_and_its_reference() {
    const ENDPOINT: &str = "http://127.0.0.1:8900/v1";
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let config = loaded(dir.path(), |config| {
        config.decision_engine.enabled = true;
        config.decision_engine.backend.get_or_insert_with(Default::default).kind =
            crate::config::schema::decision_engine::BackendSelection::Fixed(
                crate::config::schema::decision_engine::BackendKind::CompatibleHttp,
            );
        config
            .decision_engine
            .backend
            .get_or_insert_with(Default::default)
            .endpoint = ENDPOINT.into();
        config
            .decision_engine
            .backend
            .get_or_insert_with(Default::default)
            .model = "decider-1".into();
        config
            .decision_engine
            .backend
            .get_or_insert_with(Default::default)
            .credential_ref =
            crate::config::schema::decision_engine::CredentialRefSetting::Profile("cred-decision-used".into());
        config.providers.models.push(
            serde_json::from_value(serde_json::json!({
                "provider": format!("custom:{ENDPOINT}"),
                "model": "decider-1",
                "operations": ["decision_endpoint"],
            }))
            .expect("test: declaration"),
        );
    })
    .await;
    assert_eq!(
        reference_use(&config, "cred-decision-used").expect("test: use"),
        "configured for the enabled decision backend (decision_engine.backend.credential_ref)"
    );
    assert_eq!(
        reference_use(&config, "cred-spare").expect("test: use"),
        REFERENCE_NOT_USED
    );
    let error = service_for(config)
        .plan(
            &ManagedChange::RemoveProvider {
                provider: format!("custom:{ENDPOINT}"),
            },
            ConfigActor::Cli,
        )
        .await
        .expect_err("test: the decision backend's provider is not removed");
    let ManageError::Referenced { referrers, .. } = &error else {
        panic!("expected Referenced, got {error}");
    };
    assert!(
        referrers.contains(&"the decision backend (decision_engine.backend)".to_string()),
        "{referrers:?}"
    );
}

/// D-078 carried: a provider a selection still uses is not removed; the
/// refusal names every referrer and nothing is written.
#[tokio::test]
async fn removing_a_provider_a_selection_uses_is_refused_naming_the_referrers() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let config = loaded(dir.path(), |config| {
        config.reliability.fallback_providers = vec!["openrouter".into()];
        config.model_routes = vec![route("fast", "openrouter", "openai/gpt-4o-mini")];
        config.agents.insert(
            "coder".into(),
            serde_json::from_value::<DelegateAgentConfig>(serde_json::json!({
                "provider": "OpenRouter",
                "model": "openai/gpt-4o",
            }))
            .expect("test: agent"),
        );
    })
    .await;
    let before = std::fs::read(&config.config_path).expect("test: read");
    let service = service_for(config);

    let error = service
        .plan(
            &ManagedChange::RemoveProvider {
                provider: "openrouter".into(),
            },
            ConfigActor::Cli,
        )
        .await
        .expect_err("test: a used provider is not removed");
    let ManageError::Referenced { object, referrers } = &error else {
        panic!("expected Referenced, got {error}");
    };
    assert_eq!(object, "provider 'openrouter'");
    assert!(
        referrers.contains(&"model route `hint:fast`".to_string()),
        "{referrers:?}"
    );
    assert!(referrers.contains(&"agent `coder`".to_string()), "{referrers:?}");
    assert!(error.to_string().contains("nothing was written"), "{error}");
    assert_eq!(
        std::fs::read(&service.config().config_path).expect("test: read"),
        before
    );

    // The default provider is referenced by the default model.
    let error = service
        .plan(
            &ManagedChange::RemoveProvider {
                provider: "openai".into(),
            },
            ConfigActor::Cli,
        )
        .await
        .expect_err("test: the default provider is not removed");
    assert!(
        matches!(&error, ManageError::Referenced { referrers, .. } if referrers.contains(&"default model".to_string())),
        "{error}"
    );
}

#[tokio::test]
async fn an_unused_fallback_provider_is_removed_with_its_declarations() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let config = loaded(dir.path(), |config| {
        config.reliability.fallback_providers = vec!["openrouter".into()];
    })
    .await;
    let service = service_for(config);
    let plan = service
        .plan(
            &ManagedChange::RemoveProvider {
                provider: "openrouter".into(),
            },
            ConfigActor::Cli,
        )
        .await
        .expect("test: plan");
    assert!(
        plan.text()
            .contains("  - reliability.fallback_providers[0] (was \"openrouter\")\n"),
        "{}",
        plan.text()
    );
    let outcome = service.apply(plan, ConfigActor::Cli, None).await.expect("test: apply");
    assert!(outcome.record_id.is_some());
    let stored = std::fs::read_to_string(&service.config().config_path).expect("test: read");
    assert!(!stored.contains("openrouter"), "{stored}");
}

/// D-078 carried: a credential reference the stored configuration still
/// names (in any section, modelled or not) is not removed.
#[tokio::test]
async fn removing_a_credential_the_configuration_names_is_refused() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let config = loaded(dir.path(), |_| {}).await;
    let references = CredentialReferences::for_config(&config).expect("test: references");
    references.set("openai", "route-key", SECRET).expect("test: set");
    references.set("openai", "spare", SECRET).expect("test: set");
    // A section that names the reference (a hosted decision backend, or a
    // route once routes take references).
    let mut stored = std::fs::read_to_string(&config.config_path).expect("test: read");
    stored.push_str("\n[decision.backends.hosted]\ncredential_ref = \"cred-route-key\"\n");
    std::fs::write(&config.config_path, stored).expect("test: write");
    let service = service_for(config);

    let error = service
        .remove_credential("cred-route-key")
        .await
        .expect_err("test: a named reference is not removed");
    let ManageError::Referenced { object, referrers } = &error else {
        panic!("expected Referenced, got {error}");
    };
    assert_eq!(object, "credential reference 'cred-route-key'");
    assert_eq!(referrers, &["decision.backends.hosted.credential_ref"]);
    assert!(!error.to_string().contains(SECRET));
    let references = service.credentials().expect("test: references");
    assert!(references.status("cred-route-key").expect("test: status").is_some());

    assert!(service.remove_credential("cred-spare").await.expect("test: remove"));
    assert!(references.status("cred-spare").expect("test: status").is_none());
}

/// PMM-H: a model route, an embedding route and an agent that name a
/// reference in `credential_ref` are its referrers, so the reference is
/// not removed under them, and each is listed.
#[tokio::test]
async fn every_credential_ref_holder_is_a_referrer() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let seed = loaded(dir.path(), |_| {}).await;
    CredentialReferences::for_config(&seed)
        .expect("test: references")
        .set("openai", "shared", SECRET)
        .expect("test: set");
    let config = loaded(dir.path(), |config| {
        config.model_routes.push(ModelRouteConfig {
            credential_ref: Some("cred-shared".into()),
            ..route("fast", "openai", "gpt-4o-mini")
        });
        config.embedding_routes.push(EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "openai".into(),
            model: "text-embedding-3-small".into(),
            dimensions: None,
            api_key: None,
            credential_ref: Some("cred-shared".into()),
        });
        config.agents.insert(
            "coder".into(),
            serde_json::from_value(serde_json::json!({
                "provider": "openai",
                "model": "gpt-4o",
                "credential_ref": "cred-shared",
            }))
            .expect("test: agent"),
        );
    })
    .await;
    let service = service_for(config);

    let error = service
        .remove_credential("cred-shared")
        .await
        .expect_err("test: a reference in use is not removed");
    let ManageError::Referenced { referrers, .. } = &error else {
        panic!("expected Referenced, got {error}");
    };
    let mut referrers = referrers.clone();
    referrers.sort();
    assert_eq!(
        referrers,
        [
            "agents.coder.credential_ref",
            "embedding_routes[0].credential_ref",
            "model_routes[0].credential_ref"
        ]
    );
    let credentials = provider_credentials(&service.config(), "openai").expect("test: status");
    assert_eq!(
        credentials.reference_uses.get("cred-shared").map(Vec::len),
        Some(3),
        "{credentials:?}"
    );
    let mut combined = (*service.config()).clone();
    combined
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .kind = crate::config::schema::decision_engine::BackendSelection::Fixed(
        crate::config::schema::decision_engine::BackendKind::CompatibleHttp,
    );
    combined
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .endpoint = "http://127.0.0.1:8900/v1".into();
    combined
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .model = "decider-1".into();
    combined
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .credential_ref = crate::config::schema::decision_engine::CredentialRefSetting::Profile("cred-shared".into());
    let disabled = reference_use(&combined, "cred-shared").expect("test: disabled status");
    assert!(
        disabled.contains("decision_engine.backend.credential_ref"),
        "{disabled}"
    );
    assert!(
        !disabled.contains("configured for the enabled decision backend"),
        "{disabled}"
    );
    combined.decision_engine.enabled = true;
    let active = reference_use(&combined, "cred-shared").expect("test: active status");
    assert!(
        active.contains("configured for the enabled decision backend"),
        "{active}"
    );
    assert!(active.contains("model_routes[0].credential_ref"), "{active}");
    assert!(active.contains("embedding_routes[0].credential_ref"), "{active}");
    assert!(active.contains("agents.coder.credential_ref"), "{active}");
    combined
        .decision_engine
        .backend
        .get_or_insert_with(Default::default)
        .endpoint = "https://example.com/v1".into();
    let remote_disabled = reference_use(&combined, "cred-shared").expect("test: remote disabled status");
    assert!(
        !remote_disabled.contains("configured for the enabled decision backend"),
        "{remote_disabled}"
    );
}

/// D-078 carried, PMM-H: a rollback that would restore a route naming a
/// credential reference removed since is refused, naming the route and
/// the reference, with nothing written; a change naming a reference of
/// another provider is refused the same way.
#[tokio::test]
async fn a_rollback_to_a_missing_credential_reference_is_refused() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let seed = loaded(dir.path(), |_| {}).await;
    let references = CredentialReferences::for_config(&seed).expect("test: references");
    references.set("openai", "route-key", SECRET).expect("test: set");
    references.set("anthropic", "other", SECRET).expect("test: set");
    let config = loaded(dir.path(), |config| {
        config.model_routes.push(ModelRouteConfig {
            credential_ref: Some("cred-route-key".into()),
            ..route("fast", "openai", "gpt-4o-mini")
        });
    })
    .await;
    let service = service_for(config);

    let plan = service
        .plan(
            &ManagedChange::RemoveRoute {
                operation: SelectionOperation::Chat,
                hint: "fast".into(),
            },
            ConfigActor::Cli,
        )
        .await
        .expect("test: plan the removal");
    let record = service
        .apply(plan, ConfigActor::Cli, None)
        .await
        .expect("test: remove the route")
        .record_id
        .expect("test: recorded");
    assert!(service.remove_credential("cred-route-key").await.expect("test: remove"));
    let before = std::fs::read(&service.config().config_path).expect("test: read");

    let current = crate::config::files::current_config_revision(&service.config().config_path).expect("test: revision");
    let error = service
        .transactions()
        .rollback(super::super::RollbackRequest {
            record_id: &record,
            expected_revision: &current,
            confirm: true,
            restore_credentials: true,
            disable_encryption: false,
        })
        .await
        .expect_err("test: the reference is gone");
    let text = error.to_string();
    assert!(
        text.contains("model route `hint:fast`") && text.contains("cred-route-key") && text.contains("does not exist"),
        "{text}"
    );
    assert_eq!(
        std::fs::read(&service.config().config_path).expect("test: read"),
        before
    );

    // A route naming another provider's reference is refused by the
    // same check, before anything is written.
    let mut candidate = service.config().as_ref().clone();
    candidate.model_routes.push(ModelRouteConfig {
        credential_ref: Some("cred-other".into()),
        ..route("stolen", "openai", "gpt-4o")
    });
    let plan = service
        .transactions()
        .plan(ConfigChange::Structured(Box::new(candidate)))
        .await
        .expect("test: the candidate stages");
    let error = service
        .transactions()
        .check_selections(&plan, ConfigActor::Cli)
        .await
        .expect_err("test: another provider's reference is refused");
    assert!(error.to_string().contains("belongs to provider"), "{error}");
    assert_eq!(
        std::fs::read(&service.config().config_path).expect("test: read"),
        before
    );
}

/// RED before PMM-H (D-089 11): replacing a credential reference's value
/// left no trace in the configuration history. It is now recorded, by
/// reference id only; creating a new reference is not.
#[tokio::test]
async fn replacing_a_credential_reference_is_recorded_in_history() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let service = service_for(loaded(dir.path(), |_| {}).await);
    let history = service.transactions().history().expect("test: history");

    service
        .set_credential("openai".into(), "route-key".into(), SECRET.into(), ConfigActor::Cli)
        .await
        .expect("test: create");
    assert!(
        history.list().expect("test: list").is_empty(),
        "creating is not recorded"
    );

    service
        .set_credential(
            "openai".into(),
            "route-key".into(),
            "pmmh-rotated-sentinel".into(),
            ConfigActor::GatewayManagement,
        )
        .await
        .expect("test: replace");
    let records = history.list().expect("test: list");
    assert_eq!(records.len(), 1, "{records:?}");
    let record = records.first().expect("test: the record");
    assert_eq!(record.status, super::super::history::RecordStatus::CredentialReplaced);
    assert_eq!(record.actor, ConfigActor::GatewayManagement);
    assert_eq!(
        record.summary.changes.first().map(|change| change.path.as_str()),
        Some("credential_references.cred-route-key")
    );
    assert_eq!(record.summary.risk, super::super::plan::ConfigChangeRisk::High);
    let shown = serde_json::to_string(record).expect("test: json");
    assert!(
        !shown.contains(SECRET) && !shown.contains("pmmh-rotated-sentinel"),
        "{shown}"
    );
}

/// The root connection belongs to the default provider: switching the
/// default never hands it over implicitly.
#[tokio::test]
async fn switching_the_default_provider_never_hands_the_root_key_over_implicitly() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let config = loaded(dir.path(), |config| config.api_key = Some(SECRET.into())).await;
    let service = service_for(config);
    let switch = |root_credential| ManagedChange::SetDefault {
        settings: None,
        operation: SelectionOperation::Chat,
        provider: "anthropic".into(),
        model: "claude-sonnet-4-5".into(),
        root_credential,
    };
    let error = service
        .plan(&switch(RootCredential::Refuse), ConfigActor::Cli)
        .await
        .expect_err("test: refused");
    assert!(
        error
            .to_string()
            .contains("belongs to the current default provider 'openai'"),
        "{error}"
    );
    assert!(!error.to_string().contains(SECRET));

    // Keeping the root connection for the new provider is the operator's
    // explicit choice, and still a high-risk change (D-085 F-3).
    let kept = service
        .plan(&switch(RootCredential::Keep), ConfigActor::Cli)
        .await
        .expect("test: plan");
    assert!(
        kept.summary()
            .risk_reasons
            .contains(&super::super::plan::RiskReason::CredentialOwnerChanged {
                path: "default_provider".into()
            }),
        "{}",
        kept.text()
    );
    let plan = service
        .plan(&switch(RootCredential::Clear), ConfigActor::Cli)
        .await
        .expect("test: plan");
    let text = plan.text();
    assert!(text.contains("  - api_key (was \"***\")\n"), "{text}");
    assert!(
        text.contains("  ~ default_provider: \"openai\" -> \"anthropic\"\n"),
        "{text}"
    );
    assert!(!text.contains(SECRET), "{text}");
    assert_eq!(plan.text(), render_plan(plan.summary(), &plan.warnings));
}

#[tokio::test]
async fn routes_and_declarations_are_added_updated_and_removed_by_key() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let config = loaded(dir.path(), |config| {
        config.model_routes = vec![route("fast", "openai", "gpt-4o-mini")];
    })
    .await;
    let service = service_for(config.clone());
    let duplicate = service
        .plan(
            &ManagedChange::AddRoute {
                operation: SelectionOperation::Chat,
                hint: "hint:fast".into(),
                provider: "openai".into(),
                model: "gpt-4o".into(),
            },
            ConfigActor::Cli,
        )
        .await
        .expect_err("test: duplicate");
    assert!(
        duplicate
            .to_string()
            .starts_with("model route `hint:fast` already exists"),
        "{duplicate}"
    );
    let Err(missing) = candidate(
        &config,
        &ManagedChange::UpdateRoute {
            operation: SelectionOperation::Embedding,
            hint: "fast".into(),
            provider: None,
            model: Some("text-embedding-3-small".into()),
        },
    ) else {
        panic!("test: an embedding route that does not exist is not updated");
    };
    assert!(matches!(missing, ManageError::NotFound(_)), "{missing}");

    let declared = candidate(
        &config,
        &ManagedChange::AddModel(ProviderModelConfig {
            provider: "custom:http://127.0.0.1:18086/v1".into(),
            model: Some("local-model".into()),
            operations: Some(vec![crate::config::schema::ModelOperationSetting::Chat]),
            native_tools: Some(true),
            vision_input: None,
            streaming: None,
            structured_output: None,
            tool_schema_dialect: None,
            tool_call_dialect: None,
            context_window: None,
        }),
    )
    .unwrap_or_else(|error| panic!("test: add model: {error}"));
    let updated = candidate(
        &declared,
        &ManagedChange::UpdateModel(ProviderModelConfig {
            provider: "CUSTOM:http://127.0.0.1:18086/v1/".into(),
            model: Some("LOCAL-MODEL".into()),
            operations: None,
            native_tools: Some(false),
            vision_input: None,
            streaming: None,
            structured_output: None,
            tool_schema_dialect: None,
            tool_call_dialect: None,
            context_window: Some(32_768),
        }),
    )
    .unwrap_or_else(|error| panic!("test: update model: {error}"));
    let entry = updated.providers.models.first().expect("test: the declaration");
    assert_eq!(entry.native_tools, Some(false));
    assert_eq!(entry.context_window, Some(32_768));
    assert!(entry.operations.is_some(), "an undeclared field is kept");
    let removed = candidate(
        &updated,
        &ManagedChange::RemoveModel {
            provider: "custom:http://127.0.0.1:18086/v1".into(),
            model: Some("local-model".into()),
        },
    )
    .unwrap_or_else(|error| panic!("test: remove model: {error}"));
    assert!(removed.providers.models.is_empty(), "expected an empty value");
}

/// D-085 F-6 (new read model): every known model — configured
/// selections, declarations and the catalog cache — with the resolver's
/// evidence, filtered by operation and by proven executability.
#[tokio::test]
async fn the_model_catalog_lists_known_models_by_operation_and_evidence() {
    use crate::providers::catalog::ModelOperation;
    use crate::providers::catalog::discovery::{DiscoveredModel, record_discovered};
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let config = loaded(dir.path(), |config| {
        config.providers.models.push(
            toml::from_str(
                "provider = \"openrouter\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n",
            )
            .expect("test: declaration"),
        );
        config.providers.models.push(
            toml::from_str("provider = \"openai\"\nmodel = \"gpt-4o\"\noperations = [\"chat\"]\n")
                .expect("test: declaration"),
        );
    })
    .await;
    let mut embedder = DiscoveredModel::id_only("text-embedding-3-small");
    embedder.output_modalities = Some(vec!["embeddings".into()]);
    record_discovered(
        &config.workspace_dir,
        "openai",
        &[embedder, DiscoveredModel::id_only("gpt-4o-mini")],
        1_700_000_000,
    )
    .expect("test: cache");

    let all = model_catalog(&config, &ModelFilter::default());
    let names = |entries: &[ModelEntry]| {
        entries
            .iter()
            .map(|entry| format!("{}/{}", entry.provider, entry.model))
            .collect::<Vec<_>>()
    };
    let listed = names(&all);
    for expected in ["openai/gpt-4o", "openai/gpt-4o-mini", "openrouter/jev-decision-test"] {
        assert!(listed.contains(&expected.to_string()), "{expected}: {listed:?}");
    }

    let chat = model_catalog(
        &config,
        &ModelFilter {
            operation: Some(ModelOperation::Chat),
            ..ModelFilter::default()
        },
    );
    assert!(
        !names(&chat).contains(&"openrouter/jev-decision-test".to_string()),
        "a decision-only model is ruled out for chat: {:?}",
        names(&chat)
    );
    let executable = model_catalog(
        &config,
        &ModelFilter {
            operation: Some(ModelOperation::Chat),
            executable: true,
            ..ModelFilter::default()
        },
    );
    let executable_names = names(&executable);
    assert!(
        executable_names.contains(&"openai/gpt-4o".to_string()),
        "the declared chat model remains executable: {executable_names:?}"
    );
    assert!(
        executable_names.contains(&"openai/gpt-5.6".to_string()),
        "the embedded release catalog supplies executable chat models: {executable_names:?}"
    );
    assert!(
        !executable_names.contains(&"openrouter/jev-decision-test".to_string()),
        "a declared decision-only model remains excluded: {executable_names:?}"
    );
    assert!(
        executable.iter().all(|entry| {
            entry
                .operations
                .get("chat")
                .is_some_and(|support| support == "supported")
        }),
        "every executable row must carry positive chat evidence: {executable:?}"
    );
    let release_entry = executable
        .iter()
        .find(|entry| entry.provider == "openai" && entry.model == "gpt-5.6")
        .expect("test: embedded catalog model is listed");
    assert_eq!(release_entry.source, "embedded release catalog");

    let shown = model_entry(&config, "openrouter", "jev-decision-test").expect("test: show");
    // The declaration lists the decision endpoint only, and OpenRouter's
    // decision adapter executes it (CDP-02): chat is negative evidence,
    // the decision endpoint positive.
    assert!(
        shown
            .operations
            .get("decision_endpoint")
            .is_some_and(|support| support.starts_with("supported")),
        "{:?}",
        shown.operations
    );
    assert!(
        shown
            .operations
            .get("chat")
            .is_some_and(|support| support.starts_with("unsupported"))
    );
    assert_eq!(shown.source, "operator declaration");
    assert!(parse_operation("teleport").is_err());
    assert_eq!(
        parse_operation("EMBEDDING").expect("test: parse"),
        ModelOperation::Embedding
    );
}

#[test]
fn provider_text_redaction_masks_query_values_and_userinfo() {
    assert_eq!(
        redact_provider_text("custom:https://h.example/v1?region=eu&api_key=abc-1"),
        "custom:https://h.example/v1?region=[redacted]&api_key=[redacted]"
    );
    assert_eq!(
        redact_provider_text("fallback provider 'custom:http://h/v1?t=xyz' refused: what?"),
        "fallback provider 'custom:http://h/v1?t=[redacted]' refused: what?"
    );
    assert!(!redact_provider_text("custom:https://u:pw-sentinel@h/v1").contains("pw-sentinel"));
    assert_eq!(redact_provider_text("openai"), "openai");
}

/// RED before PMME-g (D-088 C-2): the guidance for the root connection
/// named `prx init --from-stdin`, which regenerates the whole
/// configuration outside the transaction.
#[tokio::test]
async fn the_root_connection_guidance_names_a_non_destructive_path() {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let config = loaded(dir.path(), |config| config.api_key = Some(SECRET.into())).await;
    let credentials = provider_credentials(&config, "openai").expect("test: status");
    assert_eq!(credentials.in_use, CredentialInUse::RootConnection);
    assert!(!credentials.guidance.contains("prx init"), "{}", credentials.guidance);
    assert!(
        credentials.guidance.contains("PUT /api/config/secrets"),
        "{}",
        credentials.guidance
    );
    assert!(!credentials.guidance.contains(SECRET));
}

/// D-085 F-9: keeping or clearing the root connection means nothing for
/// the embedding default and is refused rather than ignored.
#[test]
fn root_connection_flags_are_refused_for_the_embedding_default() {
    let config = Config::default();
    for root_credential in [RootCredential::Keep, RootCredential::Clear] {
        let Err(error) = candidate(
            &config,
            &ManagedChange::SetDefault {
                settings: None,
                operation: SelectionOperation::Embedding,
                provider: "openai".into(),
                model: "text-embedding-3-small".into(),
                root_credential,
            },
        ) else {
            panic!("test: refused");
        };
        assert!(error.to_string().contains("chat default only"), "{error}");
    }
}

/// RED before PMME-f (D-085 F-10): a credential in a provider URL's
/// query string came back in referrer names and messages.
#[tokio::test]
async fn a_query_string_credential_in_a_provider_is_never_echoed() {
    const QUERY: &str = "custom:https://llm.example/v1?api_key=sentinel-query-token-123456";
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let config = loaded(dir.path(), |config| {
        config.reliability.fallback_providers = vec![QUERY.into()];
        config.model_routes = vec![route("fast", QUERY, "local-model")];
    })
    .await;
    let service = service_for(config.clone());
    let error = service
        .plan(
            &ManagedChange::RemoveProvider { provider: QUERY.into() },
            ConfigActor::Cli,
        )
        .await
        .expect_err("test: still used");
    let text = error.to_string();
    assert!(text.contains("model route `hint:fast`"), "{text}");
    assert!(!text.contains("sentinel-query-token"), "{text}");
    let listed = serde_json::to_string(&configured_providers(&config)).expect("test: json");
    assert!(!listed.contains("sentinel-query-token"), "{listed}");
}

#[tokio::test]
async fn incompatible_fallback_error_names_model_fallback_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let config = loaded(dir.path(), |config| {
        config.default_provider = Some("openrouter".into());
        config.default_model = Some("anthropic/claude-sonnet-5".into());
    })
    .await;
    let service = service_for(config);
    let error = service
        .plan(
            &ManagedChange::AddProvider {
                provider: "kimi-code".into(),
            },
            ConfigActor::Cli,
        )
        .await
        .expect_err("incompatible primary is refused");
    let shown = error.to_string();
    assert!(shown.contains("not compatible"), "{shown}");
    assert!(shown.contains("[reliability.model_fallbacks]"), "{shown}");
}

#[tokio::test]
async fn selecting_a_subscription_default_does_not_require_a_nominal_price() {
    let dir = tempfile::tempdir().unwrap();
    let config = loaded(dir.path(), |_| {}).await;
    let service = service_for(config);
    let plan = service
        .plan(
            &ManagedChange::SetDefault {
                operation: SelectionOperation::Chat,
                provider: "kimi-code".into(),
                model: "kimi-for-coding".into(),
                settings: None,
                root_credential: RootCredential::Refuse,
            },
            ConfigActor::Cli,
        )
        .await
        .unwrap();
    assert!(
        plan.warnings()
            .iter()
            .all(|warning| { !warning.contains("G6 budget admission refuses") && !warning.contains("[cost.prices.") })
    );
}

#[tokio::test]
async fn selecting_unpriced_default_warns_without_changing_admission() {
    let dir = tempfile::tempdir().unwrap();
    let config = loaded(dir.path(), |_| {}).await;
    let service = service_for(config);
    let unpriced = service
        .plan(
            &ManagedChange::SetDefault {
                operation: SelectionOperation::Chat,
                provider: "openrouter".into(),
                model: "deepseek/deepseek-v3.2".into(),
                settings: None,
                root_credential: RootCredential::Refuse,
            },
            ConfigActor::Cli,
        )
        .await
        .unwrap();
    assert!(unpriced.warnings().iter().any(|warning| {
        warning.contains("G6 budget admission refuses unpriced model")
            && warning.contains(r#"[cost.prices."deepseek/deepseek-v3.2"]"#)
    }));
}

#[test]
fn an_unresolvable_provider_is_refused_without_echoing_it() {
    let config = Config::default();
    let Err(error) = candidate(
        &config,
        &ManagedChange::AddProvider {
            provider: SECRET.into(),
        },
    ) else {
        panic!("test: an unresolvable provider is refused");
    };
    assert!(!error.to_string().contains(SECRET), "{error}");
}
#[tokio::test]
async fn model_settings_are_planned_atomically_persisted_and_revision_guarded() {
    use crate::config::model_settings::ModelSettings;
    let dir = tempfile::TempDir::new().unwrap();
    let config = loaded(dir.path(), |_| {}).await;
    let service = ManagementService::deferred(crate::config::new_shared(config.clone()));
    let settings: ModelSettings =
        serde_json::from_value(serde_json::json!({"level":"high","max_output_tokens":64000})).unwrap();
    let change = ManagedChange::SetDefault {
        operation: SelectionOperation::Chat,
        provider: "openai".into(),
        model: "gpt-5.6".into(),
        root_credential: RootCredential::Refuse,
        settings: Some(settings.clone()),
    };
    let before = std::fs::read(&config.config_path).unwrap();
    let first = service.plan(&change, ConfigActor::Cli).await.unwrap();
    let stale = service.plan(&change, ConfigActor::Cli).await.unwrap();
    assert!(first.text().contains("model_settings"));
    assert_eq!(
        std::fs::read(&config.config_path).unwrap(),
        before,
        "planning must not write"
    );
    let outcome = service.apply(first, ConfigActor::Cli, None).await.unwrap();
    assert!(outcome.record_id.is_some());
    let written = Config::load_from_path(&config.config_path, config.workspace_dir.clone()).unwrap();
    assert_eq!(written.default_model.as_deref(), Some("gpt-5.6"));
    assert_eq!(written.model_settings.get("openai/gpt-5.6"), Some(&settings));
    let committed = std::fs::read(&config.config_path).unwrap();
    assert!(service.apply(stale, ConfigActor::Cli, None).await.is_err());
    assert_eq!(std::fs::read(&config.config_path).unwrap(), committed);
    let invalid = ManagedChange::SetModelSettings {
        model: "openai/gpt-5.6".into(),
        settings: serde_json::from_value(serde_json::json!({"max_output_tokens":0})).unwrap(),
    };
    assert!(service.plan(&invalid, ConfigActor::Cli).await.is_err());
    assert_eq!(std::fs::read(&config.config_path).unwrap(), committed);
}
