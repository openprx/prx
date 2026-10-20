use super::{Config, Result};

/// The first files contain only safe shipped defaults. The operator's complete
/// selection then uses the shared PMM transaction, including revision/history
/// and compensation, for both quick setup and the interactive wizard.
pub(super) async fn persist_onboarding_config(config: &Config, split: bool) -> Result<()> {
    use crate::config::manage::{ApplyOptions, ConfigActor, ConfigChange, ConfigTransactions};
    if !config.config_path.exists() {
        let initial = Config {
            config_path: config.config_path.clone(),
            workspace_dir: config.workspace_dir.clone(),
            ..Config::default()
        };
        if split {
            crate::config::files::write_split_config(&initial, false).await?;
        } else {
            initial.save().await?;
        }
    }
    let current = Config::load_from_path(&config.config_path, config.workspace_dir.clone())?;
    let transactions = ConfigTransactions::new(crate::config::new_shared(current)).deferring_activation();
    let plan = transactions
        .plan(ConfigChange::Structured(Box::new(config.clone())))
        .await?;
    let revision = plan.planned_revision().map(str::to_string);
    // These are the inputs the operator just supplied to onboarding. Bind
    // confirmation to the actual plan revision, never to a wildcard.
    transactions
        .apply(
            plan,
            ConfigActor::Cli,
            ApplyOptions {
                expected_revision: revision.as_deref(),
                confirm_high_risk: true,
                confirmed_revision: revision.as_deref(),
            },
        )
        .await?;
    Ok(())
}
