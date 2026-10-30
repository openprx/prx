//! Opt-in fixture preparation only. Real CLI/model execution is recorded separately.
use super::*;
fn copy_tree(source: &std::path::Path, target: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    std::fs::set_permissions(target, std::fs::metadata(source)?.permissions())?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let path = target.join(entry.file_name());
        let kind = entry.file_type()?;
        anyhow::ensure!(!kind.is_symlink(), "fixture copy refuses symlinks");
        if kind.is_dir() {
            copy_tree(&entry.path(), &path)?;
        } else {
            std::fs::copy(entry.path(), path)?;
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "prepares an isolated fixture at PRX_M1_FIXTURE_DIR; not live acceptance"]
async fn experiment_prepare_real_cli_fixture() -> Result<()> {
    let destination = std::path::PathBuf::from(std::env::var("PRX_M1_FIXTURE_DIR")?);
    anyhow::ensure!(!destination.exists(), "fixture destination must be new");
    let workspace = destination.join("workspace");
    let f = fixture();
    let pattern = usable_pattern(&f);
    let server = Server::start(false).await?;
    let mut config = config(&f, &server);
    server.finish().await?;
    let provider = "custom:http://127.0.0.1:18086/v1";
    let model = "qwen38-27b-splash";
    config.workspace_dir = workspace.clone();
    config.config_path = destination.join("config.toml");
    config.default_provider = Some(provider.into());
    config.default_model = Some(model.into());
    config.api_key = None;
    config.cost.prices.clear();
    config.cost.prices.insert(
        format!("{provider}/{model}"),
        crate::config::schema::ModelPricing {
            input: 0.0,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
    );
    config.providers.models.clear();
    config.providers.models.push(toml::from_str(&format!(
        "provider = {provider:?}\nmodel = {model:?}\noperations = [\"chat\"]\nnative_tools = true\n"
    ))?);
    let mut suite = suite_mode(&f, true).suite().clone();
    for case in &mut suite.cases {
        case.prompt = format!(
            "Case {}: Use skill_read to read Deploy helper as reference only. Do not execute its deployment checks. Use file_write to create artifact.txt containing exactly DRAFT. Use file_edit to replace DRAFT with VERIFIED. Use file_read to verify artifact.txt. Do not use shell or delegate. Report the actual file results.",
            case.case_id
        );
        case.validators = vec![Validator::ArtifactContains {
            path: "artifact.txt".into(),
            text: "VERIFIED".into(),
        }];
        case.max_elapsed_ms = 120_000;
    }
    suite.budget.max_total_elapsed_ms = 1_500_000;
    let suite = suite.freeze()?;
    let service = service(&f, &suite);
    config.skills_lifecycle.enabled = true;
    config.skills_lifecycle.mode = crate::config::skills_lifecycle::SkillsLifecycleModeConfig::Shadow;
    config.skills_lifecycle.owner_scope = Some(f.store.owner_scope().to_string());
    config.skills_lifecycle.suites = vec![crate::config::skills_lifecycle::SkillSuiteRegistration {
        skill_id: f.pin.skill_id.to_string(),
        suite_digest: suite.digest().into(),
    }];
    let (plan, inputs) = plan(&f, &service, &config, vec![pattern.pattern_id])?;
    let resources = ExperimentResources {
        calls: 256,
        tokens: 4_000_000,
        micro_usd: 1_000_000,
        tool_calls: 256,
        output_bytes: 8_000_000,
        scratch_bytes: 128 * 1024 * 1024,
    };
    let draft = ExperimentDraft {
        id: ExperimentId::parse("experiment-live-cli")?,
        skill_id: f.pin.skill_id.clone(),
        verification_split: EvalSplit::Validation,
        verify_holdout: false,
        lesson_query: None,
        resources,
        elapsed_ms: 1_800_000,
        max_concurrent: 1,
        stages: plan
            .spec()
            .stages
            .iter()
            .map(|stage| {
                let mut allocation = stage.allocation;
                if stage.id == "evaluation" {
                    allocation.calls = 128;
                    allocation.tokens = 3_000_000;
                    allocation.tool_calls = 128;
                    allocation.output_bytes = 4_000_000;
                    allocation.scratch_bytes = 64 * 1024 * 1024;
                }
                ExperimentDraftStage {
                    id: stage.id.clone(),
                    dependencies: stage.dependencies.clone(),
                    allocation,
                    operation: inputs[&stage.id].clone(),
                }
            })
            .collect(),
    };
    copy_tree(f.tmp.path(), &workspace)?;
    std::fs::write(&config.config_path, toml::to_string_pretty(&config)?)?;
    std::fs::write(destination.join("draft.json"), serde_json::to_vec_pretty(&draft)?)?;
    for (name, operation) in inputs {
        std::fs::write(
            destination.join(format!("{name}.json")),
            serde_json::to_vec_pretty(&operation)?,
        )?;
    }
    std::fs::write(
        destination.join("fixture.json"),
        serde_json::to_vec_pretty(&json!({
            "prepared_only":true,"model_requests":0,"expected_quality":"neutral baseline and candidate may both pass; no benefit claim",
            "baseline_generation":f.pin.generation,"skill_id":f.pin.skill_id,
        }))?,
    )?;
    Ok(())
}
