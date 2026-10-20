use super::{Config, Context, Result};

/// Whether a stored tree asks for secrets to be encrypted (`secrets.encrypt`,
/// default `true`).
pub(super) fn stored_tree_encrypts_secrets(tree: &toml::Value) -> bool {
    tree.get("secrets")
        .and_then(|secrets| secrets.get("encrypt"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true)
}

/// Scan all config secret fields for legacy `enc:` values and migrate them to the secure
/// `enc2:` (ChaCha20-Poly1305) format in-place, then persist the updated config to disk.
///
/// This is called once at startup so that secrets which have never been accessed are
/// upgraded eagerly instead of remaining in the vulnerable XOR-cipher format indefinitely.
/// Individual failures are logged as warnings and do not abort the migration.
pub(crate) async fn migrate_config_legacy_secrets(config: &Config) -> Result<()> {
    let openprx_dir = config
        .config_path
        .parent()
        .context("Config path must have a parent directory")?;
    // Never a downgrade: with encryption off, an upgrade would rewrite a
    // legacy `enc:` value as plaintext (D-101 F-5). The value keeps loading
    // (it is decrypted on load); it is upgraded once encryption is on.
    if !config.secrets.encrypt {
        tracing::debug!("secrets.encrypt is off; legacy enc: values are not rewritten");
        return Ok(());
    }
    let store = crate::security::SecretStore::new(openprx_dir, true);

    // One writer at a time in this process, and the files are committed only
    // if nothing changed them since they were read (a guarded commit), so a
    // concurrent write is never overwritten with older content; on a
    // conflict the upgrade waits for the next start (D-101 F-5).
    let lock = crate::config::manage::transaction::transaction_lock(&config.config_path);
    let _serialized = lock.lock().await;
    let revision = crate::config::files::current_config_revision(&config.config_path)
        .context("Failed to read the configuration revision for legacy-secret migration")?;

    // Re-read every stored file — `config.toml` and each `config.d`
    // fragment — to obtain the still-encrypted forms of secrets (the loaded
    // `config` already holds them decrypted). Each tree is migrated in place
    // and a changed file is written back as it was otherwise, so no default
    // is injected into a file that shares its settings with fragments
    // (PMMA F-10: fragments were decrypted on load but never upgraded).
    let mut image = crate::config::files::current_before_image(&config.config_path)
        .context("Failed to read the configuration for legacy-secret migration")?;
    let mut total_migrated = 0_usize;
    for file in &mut image.files {
        let Some(raw_toml) = file.contents.as_deref() else {
            continue;
        };
        let mut stored: toml::Value = toml::from_str(raw_toml)
            .with_context(|| format!("Failed to parse {} for legacy-secret migration", file.relative_path))?;
        // Every encrypted-at-rest path of the secret registry, the provider
        // route and rotation keys included.
        let migrated = crate::config::secret_paths::migrate_legacy_at_rest(&mut stored, &store)?;
        if migrated == 0 {
            continue;
        }
        total_migrated += migrated;
        // The values are already in enc2: form, so this must not go through
        // to_stored_toml_value(); write the migrated tree as it is.
        file.contents = Some(
            toml::to_string_pretty(&stored)
                .with_context(|| format!("Failed to render the migrated {}", file.relative_path))?,
        );
    }

    if total_migrated == 0 {
        tracing::debug!("No legacy enc: secrets found — skipping migration write");
        return Ok(());
    }

    // Unchanged files are written back byte for byte; the whole tree is
    // staged and validated, and committed through the journaled writer.
    let plan = crate::config::files::plan_before_image_restore(&config.config_path, &config.workspace_dir, &image)
        .context("Failed to plan migrated config")?;
    match crate::config::files::commit_mutation_guarded(plan, Some(&revision), |_| Ok(())).await {
        Ok(_) => {
            tracing::info!(
                count = total_migrated,
                "Migrated legacy XOR-encrypted secrets to ChaCha20-Poly1305"
            );
            Ok(())
        }
        Err(error)
            if error
                .downcast_ref::<crate::config::files::ConfigRevisionConflict>()
                .is_some() =>
        {
            tracing::info!("The configuration changed while legacy secrets were upgraded; retrying at the next start");
            Ok(())
        }
        Err(error) => Err(error).context("Failed to write migrated config"),
    }
}

/// Every `credential_ref` names a credential reference id (`cred-<name>`),
/// and a route, embedding route or agent sets either an inline `api_key` or a
/// `credential_ref`, never both: which one is sent would otherwise depend on
/// the reader. Whether the reference exists, and was issued for the
/// selection's provider, is checked when a change is planned (the store is
/// not read at load).
pub(super) fn validate_credential_references(config: &Config) -> Result<()> {
    let holders = config
        .model_routes
        .iter()
        .enumerate()
        .map(|(index, route)| {
            (
                format!("model_routes[{index}]"),
                route.api_key.as_deref(),
                route.credential_ref.as_deref(),
            )
        })
        .chain(config.embedding_routes.iter().enumerate().map(|(index, route)| {
            (
                format!("embedding_routes[{index}]"),
                route.api_key.as_deref(),
                route.credential_ref.as_deref(),
            )
        }))
        .chain(config.agents.iter().map(|(name, agent)| {
            (
                format!("agents.{name}"),
                agent.api_key.as_deref(),
                agent.credential_ref.as_deref(),
            )
        }));
    for (path, api_key, reference) in holders {
        let Some(reference) = reference else {
            continue;
        };
        crate::config::manage::credentials::CredentialReferences::reference(reference.trim())
            .map_err(|error| anyhow::anyhow!("{path}.credential_ref: {error}"))?;
        anyhow::ensure!(
            api_key.is_none_or(|key| key.trim().is_empty()),
            "{path} sets both api_key and credential_ref; keep one (the reference keeps the key in the \
             encrypted credential store)"
        );
    }
    Ok(())
}

/// Upgrade notice for configurations written before the local vector recall
/// default flip. Emitted by `prx doctor` and by the long-running runtimes.
pub const EMBEDDING_DEFAULT_UPGRADE_NOTICE: &str = "embedding_provider not set; defaulting to local vector recall since 0.8.10x, \
     set memory.embedding_provider = \"none\" to disable";
