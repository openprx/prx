use super::{
    Config, Context, EMBEDDING_DEFAULT_UPGRADE_NOTICE, EuAiActRiskClassification, InteractionNoticeApplicability,
    IntoDeserializer, MAX_EVOLUTION_JUDGE_TIMEOUT_SECS, Path, PathBuf, REPORTED_DUPLICATE_EMBEDDING_HINTS,
    REPORTED_TOOL_DIALECTS_DEPRECATION, REPORTED_UNUSED_TOOL_DIALECTS, Result, build_split_tables, codex_effort,
    config_dir_creation_error, default_config_and_workspace_dirs, duplicate_embedding_route_hints, fs,
    migrate_config_legacy_secrets, migrate_legacy_config_keys, read_merged_toml_with_gate, resolve_runtime_config_dirs,
    stored_tree_encrypts_secrets, validate_credential_references,
};

impl Config {
    /// Return the upgrade notice when this configuration inherited local vector
    /// recall from the flipped default instead of asking for it.
    ///
    /// `None` once `memory.embedding_provider` is written explicitly, or when
    /// the effective provider is disabled anyway.
    pub fn embedding_default_upgrade_notice(&self) -> Option<&'static str> {
        if self.memory.embedding_provider_explicit {
            return None;
        }
        if self.memory.embedding_provider.trim().eq_ignore_ascii_case("none") {
            return None;
        }
        Some(EMBEDDING_DEFAULT_UPGRADE_NOTICE)
    }

    /// Warn about inherited legacy ceilings below the release recommendation.
    /// Explicit operator ceilings remain unchanged and do not emit this warning.
    pub(crate) fn decision_latency_budget_warning(&self) -> Option<String> {
        let section = &self.causal_tree;
        if !section.enabled || section.policy.extra_latency_budget_ms_explicit {
            return None;
        }
        let selection = crate::decision::factory::configured_selection(section)?;
        let catalog = crate::providers::catalog::ReleaseCatalog::embedded_ref()
            .inspect_err(|error| tracing::warn!(%error, "cannot read the catalog latency recommendation"))
            .ok()?;
        let entry = catalog.model(
            &crate::providers::ProviderId::of(&selection.provider),
            section.backend.model.trim(),
        )?;
        let recommended = entry.transport.extra_latency_budget_ms?;
        let configured = section.policy.extra_latency_budget_ms;
        (configured < recommended).then(|| {
            format!(
                "causal_tree.policy.extra_latency_budget_ms={configured} is below the catalog recommendation {recommended} ms for {}; the configured ceiling is unchanged",
                entry.id
            )
        })
    }

    /// Turn a merged TOML tree into a `Config`.
    ///
    /// `config_path` is only used to tell an operator which file and line a
    /// retired key sits on; deserialization itself does not depend on it.
    pub(super) fn deserialize_merged(mut merged: toml::Value, config_path: Option<&Path>) -> Result<Self> {
        migrate_legacy_config_keys(&mut merged, config_path)?;

        let mut unknown_paths = Vec::new();
        let config: Self = serde_ignored::deserialize(merged.into_deserializer(), |path| {
            unknown_paths.push(path.to_string());
        })
        .context("Failed to deserialize merged config")?;

        unknown_paths.sort();
        unknown_paths.dedup();
        if !unknown_paths.is_empty() {
            // Deliberately removed keys were already absorbed with a warning
            // above, so anything still here is a name this build has never
            // known — almost always a typo. Say so, rather than implying the
            // operator may have configured something that used to exist.
            anyhow::bail!(
                "Unknown configuration path(s): {}. These names are not recognised by this build \
                 (keys removed by a prx upgrade are ignored with a warning instead, so this is most \
                 likely a typo); configuration is never silently ignored.",
                unknown_paths.join(", ")
            );
        }
        crate::config::model_settings::validate_keys(&config)?;
        for note in crate::config::manage::lifecycle::diagnostics(&config)? {
            if note.warning {
                tracing::warn!(warning = %note.message, "configured model lifecycle warning");
            } else {
                tracing::info!(note = %note.message, "configured model is absent from the release catalog");
            }
        }
        for warning in crate::config::model_settings::warnings(&config)? {
            tracing::warn!(%warning, "saved model settings need repair; using runtime compatibility values");
        }
        if let Some(warning) = config.decision_latency_budget_warning() {
            tracing::warn!(warning = %warning, "decision latency budget is below the catalog recommendation");
        }
        Ok(config)
    }

    /// Validate a stored configuration tree without decrypting secrets or
    /// performing any mutation. Used for staging multi-file config commits.
    pub(crate) fn validate_stored_from_path(config_path: &Path, workspace_dir: PathBuf) -> Result<()> {
        let merged = read_merged_toml_with_gate(config_path)?;
        Self::validate_stored_merged(merged, config_path, workspace_dir)
    }

    pub(crate) fn validate_stored_from_path_unchecked_generation(
        config_path: &Path,
        workspace_dir: PathBuf,
    ) -> Result<()> {
        let merged = crate::config::files::read_merged_toml_with_gate_once(config_path)?;
        Self::validate_stored_merged(merged, config_path, workspace_dir)
    }

    pub(super) fn validate_stored_merged(
        merged: toml::Value,
        config_path: &Path,
        workspace_dir: PathBuf,
    ) -> Result<()> {
        let compaction_max_context_explicit = agent_compaction_max_context_tokens_present(&merged);
        let embedding_provider_explicit = memory_embedding_provider_present(&merged);
        let mut config = Self::deserialize_merged(merged, Some(config_path))?;
        config.config_path = config_path.to_path_buf();
        config.workspace_dir = workspace_dir;
        config.agent.compaction.max_context_tokens_explicit = compaction_max_context_explicit;
        config.memory.embedding_provider_explicit = embedding_provider_explicit;
        config.validate()
    }

    pub(crate) fn load_from_path(config_path: &Path, workspace_dir: PathBuf) -> Result<Self> {
        let merged = read_merged_toml_with_gate(config_path)?;
        Self::from_stored_tree(merged, config_path, workspace_dir)
    }

    /// Load a merged stored tree that belongs to `config_path` exactly as
    /// [`load_from_path`](Self::load_from_path) loads the files: secrets are
    /// decrypted with that directory's key, then the tree is deserialized
    /// and validated. Used to check a staged candidate before it is written.
    pub(crate) fn from_stored_tree(
        mut merged: toml::Value,
        config_path: &Path,
        workspace_dir: PathBuf,
    ) -> Result<Self> {
        let openprx_dir = config_path
            .parent()
            .context("Config path must have a parent directory")?;
        let store = crate::security::SecretStore::new(openprx_dir, stored_tree_encrypts_secrets(&merged));
        crate::config::secret_paths::decrypt_at_rest(&mut merged, &store)?;
        crate::config::secret_paths::warn_on_stored_masks(&merged);

        let compaction_max_context_explicit = agent_compaction_max_context_tokens_present(&merged);
        let embedding_provider_explicit = memory_embedding_provider_present(&merged);
        let mut config = Self::deserialize_merged(merged, Some(config_path))?;
        config.config_path = config_path.to_path_buf();
        config.workspace_dir = workspace_dir;
        config.agent.compaction.max_context_tokens_explicit = compaction_max_context_explicit;
        config.memory.embedding_provider_explicit = embedding_provider_explicit;
        config.validate()?;
        Ok(config)
    }

    pub async fn load_or_init() -> Result<Self> {
        Self::load_or_init_with_config_dir(None).await
    }

    pub async fn load_existing_read_only() -> Result<Self> {
        Self::load_existing_read_only_with_config_dir(None).await
    }

    /// Load an already-existing configuration without creating or migrating
    /// the config directory, workspace, configuration file, or secrets.
    pub async fn load_existing_read_only_with_config_dir(explicit_config_dir: Option<&str>) -> Result<Self> {
        let (default_openprx_dir, default_workspace_dir) = default_config_and_workspace_dirs()?;
        let (openprx_dir, workspace_dir, resolution_source) =
            resolve_runtime_config_dirs(&default_openprx_dir, &default_workspace_dir, explicit_config_dir).await?;
        let config_path = openprx_dir.join("config.toml");
        if !config_path.is_file() {
            anyhow::bail!(
                "existing config is required for read-only inspection at {}; no config or workspace was created",
                config_path.display()
            );
        }

        let config = Self::load_from_path(&config_path, workspace_dir)?;
        tracing::info!(
            path = %config.config_path.display(),
            workspace = %config.workspace_dir.display(),
            source = resolution_source.as_str(),
            "Config loaded read-only"
        );
        Ok(config)
    }

    /// The configuration for a command that only reads (D-101 F-5): the
    /// existing one, loaded read-only; with none yet, the built-in defaults
    /// in memory, at the paths a first write would use. Nothing is written
    /// and no directory is created either way; the caller says the defaults
    /// are shown when `config_path` does not exist.
    ///
    /// # Errors
    ///
    /// When the directories cannot be resolved, or an existing configuration
    /// does not load.
    pub async fn load_existing_or_defaults_read_only(explicit_config_dir: Option<&str>) -> Result<Self> {
        let (default_openprx_dir, default_workspace_dir) = default_config_and_workspace_dirs()?;
        let (openprx_dir, workspace_dir, resolution_source) =
            resolve_runtime_config_dirs(&default_openprx_dir, &default_workspace_dir, explicit_config_dir).await?;
        let config_path = openprx_dir.join("config.toml");
        if config_path.is_file() {
            return Self::load_existing_read_only_with_config_dir(explicit_config_dir).await;
        }
        tracing::info!(
            path = %config_path.display(),
            source = resolution_source.as_str(),
            "No config yet; showing the built-in defaults read-only"
        );
        Ok(Self {
            workspace_dir,
            config_path,
            ..Self::default()
        })
    }

    pub async fn load_or_init_with_config_dir(explicit_config_dir: Option<&str>) -> Result<Self> {
        let (default_openprx_dir, default_workspace_dir) = default_config_and_workspace_dirs()?;

        let (openprx_dir, workspace_dir, resolution_source) =
            resolve_runtime_config_dirs(&default_openprx_dir, &default_workspace_dir, explicit_config_dir).await?;

        let config_path = openprx_dir.join("config.toml");

        fs::create_dir_all(&openprx_dir)
            .await
            .with_context(|| config_dir_creation_error(&openprx_dir))?;
        fs::create_dir_all(&workspace_dir)
            .await
            .context("Failed to create workspace directory")?;

        if config_path.exists() {
            crate::config::files::recover_unfinished_config_generation(&config_path, &workspace_dir)
                .await
                .context("Failed to recover unfinished config generation")?;
            // Warn if config file is world-readable (may contain API keys)
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = fs::metadata(&config_path).await {
                    if meta.permissions().mode() & 0o004 != 0 {
                        tracing::warn!(
                            "Config file {:?} is world-readable (mode {:o}). \
                             Consider restricting with: chmod 600 {:?}",
                            config_path,
                            meta.permissions().mode() & 0o777,
                            config_path,
                        );
                    }
                }
            }

            let config = Self::load_from_path(&config_path, workspace_dir)?;
            tracing::info!(
                path = %config.config_path.display(),
                workspace = %config.workspace_dir.display(),
                source = resolution_source.as_str(),
                initialized = false,
                "Config loaded"
            );
            // Eagerly migrate any remaining legacy enc: secrets so they are upgraded
            // even if they have never been accessed since the XOR-cipher era.
            if let Err(e) = migrate_config_legacy_secrets(&config).await {
                tracing::warn!(error = %e, "Legacy secret migration encountered an error");
            }
            Ok(config)
        } else {
            let mut config = Self::default();
            config.config_path = config_path.clone();
            config.workspace_dir = workspace_dir;
            config.save().await?;

            // Restrict permissions on newly created config file (may contain API keys)
            #[cfg(unix)]
            {
                use std::{fs::Permissions, os::unix::fs::PermissionsExt};
                let _ = fs::set_permissions(&config_path, Permissions::from_mode(0o600)).await;
            }

            config.validate()?;
            tracing::info!(
                path = %config.config_path.display(),
                workspace = %config.workspace_dir.display(),
                source = resolution_source.as_str(),
                initialized = true,
                "Config loaded"
            );
            Ok(config)
        }
    }

    /// `[cost].prices` keys that spell a provider with URL credentials must stay
    /// distinct once those credentials are redacted (D-027).
    ///
    /// Usage records name the provider redacted, so the price of a turn is
    /// found by the redacted spelling. Two keys that redact alike (two
    /// accounts on one endpoint, same model) would make that lookup ambiguous
    /// and would show as one entry in `GET /api/config`, so a round trip could
    /// copy one price onto the other. Like a duplicate tool-dialect entry, this
    /// is an error, never "last wins". The error names the keys only in their
    /// redacted form.
    pub(super) fn validate_cost_price_keys(&self) -> Result<()> {
        let mut entries: Vec<_> = self.cost.prices.iter().collect();
        entries.sort_by(|left, right| left.0.cmp(right.0));
        for (index, (_, price)) in entries.into_iter().enumerate() {
            for (field, value) in [
                ("input", price.input),
                ("output", price.output),
                ("cache_write", price.cache_write),
                ("cache_read", price.cache_read),
            ] {
                if !value.is_finite() || value < 0.0 {
                    anyhow::bail!("cost.prices entry {index} {field} must be finite and nonnegative");
                }
            }
        }
        let mut by_redacted: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for key in self.cost.prices.keys() {
            if let std::borrow::Cow::Owned(redacted) = crate::providers::redact_endpoint_userinfo(key) {
                *by_redacted.entry(redacted).or_insert(0) += 1;
            }
        }
        if let Some((redacted, count)) = by_redacted.into_iter().find(|(_, count)| *count > 1) {
            let listed = vec![format!("`{redacted}`"); count].join(" and ");
            anyhow::bail!(
                "cost.prices: the keys {listed} name the same provider and model once their URL userinfo is \
                 redacted, so a turn's price would be ambiguous. Keep one key per endpoint and model (prefer a \
                 key without credentials; put them in `api_key`)."
            );
        }
        Ok(())
    }

    /// Refuse a `[[providers.tool_dialects]]` entry that would be ignored,
    /// ambiguous or silently widened, naming the entry by index.
    ///
    /// Only providers whose constructor reads the declaration are accepted, so
    /// an accepted entry is never a silently ignored key. Two entries are the
    /// same when the provider identity (C1's canonical key) and the model
    /// (ignoring ASCII case) match; a duplicate is an error, never "last wins".
    pub(super) fn validate_provider_tool_dialects(&self) -> Result<()> {
        let mut seen: Vec<(String, Option<String>, usize)> = Vec::with_capacity(self.providers.tool_dialects.len());
        for (index, entry) in self.providers.tool_dialects.iter().enumerate() {
            let at = format!("providers.tool_dialects[{index}]");
            let provider = entry.provider.trim();
            if provider.is_empty() {
                anyhow::bail!("{at}.provider must not be empty");
            }
            if !crate::providers::accepts_tool_dialect_declarations(provider) {
                // The URL may carry `user:password@`; it never reaches an error.
                let provider = crate::providers::redact_endpoint_userinfo(provider);
                anyhow::bail!(
                    "{at}.provider '{provider}' does not take a tool dialect declaration: only a \
                     `custom:<http(s) url>` endpoint and the catalog entries for a server the operator \
                     runs ({}) read one in this release",
                    crate::providers::TOOL_DIALECT_DECLARABLE_CATALOG.join(", ")
                );
            }
            if entry.tool_schema.is_none() && entry.tool_call.is_none() {
                anyhow::bail!("{at} declares nothing: set `tool_schema`, `tool_call`, or both");
            }
            let model = match entry.model.as_deref().map(str::trim) {
                Some("") => anyhow::bail!("{at}.model must not be empty; omit it to cover every model"),
                Some(model) => Some(model.to_ascii_lowercase()),
                None => None,
            };
            if model.is_some() && entry.tool_call.is_some() {
                anyhow::bail!(
                    "{at}: `tool_call` applies to every model of an endpoint (the response parser cannot tell \
                     models apart), so it must be declared on an entry without `model`"
                );
            }
            let key = crate::providers::canonical_provider_key(provider);
            if let Some((_, _, first)) = seen
                .iter()
                .find(|(seen_key, seen_model, _)| *seen_key == key && *seen_model == model)
            {
                let shown = crate::providers::redact_endpoint_userinfo(provider);
                anyhow::bail!(
                    "{at} duplicates providers.tool_dialects[{first}]: both declare provider '{shown}' for {}",
                    model
                        .as_deref()
                        .map_or_else(|| "every model".to_string(), |m| format!("model '{m}'"))
                );
            }
            seen.push((key, model, index));
        }
        let unused: Vec<(usize, String)> = crate::providers::unused_tool_dialect_entries(self)
            .into_iter()
            .filter_map(|index| {
                self.providers.tool_dialects.get(index).map(|entry| {
                    (
                        index,
                        crate::providers::redact_endpoint_userinfo(entry.provider.trim()).into_owned(),
                    )
                })
            })
            .collect();
        // The C2-K key keeps loading for one release; its rows belong in
        // `[[providers.models]]` (SP-PMM §2.7, PMM-H).
        if !self.providers.tool_dialects.is_empty()
            && !REPORTED_TOOL_DIALECTS_DEPRECATION.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(
                entries = self.providers.tool_dialects.len(),
                "[[providers.tool_dialects]] is deprecated and will stop loading in a later release; \
                 `prx providers migrate --apply` moves each entry into [[providers.models]] \
                 (tool_schema -> tool_schema_dialect, tool_call -> tool_call_dialect)"
            );
        }
        // A configuration is validated at every load and reload; the same set
        // of unused entries is reported once per process (INT-2B a37).
        if !unused.is_empty() && REPORTED_UNUSED_TOOL_DIALECTS.lock().insert(unused.clone()) {
            for (index, provider) in &unused {
                tracing::warn!(
                    entry = %format!("providers.tool_dialects[{index}]"),
                    provider = %provider,
                    "tool dialect declaration names a provider nothing in this configuration uses \
                     (not the default, a fallback, a model route, a router model or an agent); \
                     check the URL, port and path, or ignore this if the provider is given on the \
                     command line (`--provider`)"
                );
            }
        }
        Ok(())
    }

    /// Refuse a `[[providers.models]]` entry that would be ignored, ambiguous
    /// or in conflict with `[[providers.tool_dialects]]`, naming it by index.
    ///
    /// Two entries are the same when the provider identity and the model
    /// (ignoring ASCII case) match; a duplicate is an error, never "last
    /// wins". A dialect may be declared for one `(provider, model)` in only one
    /// of the two tables. Every error redacts URL userinfo.
    pub(super) fn validate_provider_models(&self) -> Result<()> {
        type Key = (crate::providers::ProviderId, Option<String>);
        let mut seen: Vec<(Key, usize)> = Vec::with_capacity(self.providers.models.len());
        for (index, entry) in self.providers.models.iter().enumerate() {
            let at = format!("providers.models[{index}]");
            let provider = entry.provider.trim();
            if provider.is_empty() {
                anyhow::bail!("{at}.provider must not be empty");
            }
            let shown = crate::providers::redact_endpoint_userinfo(provider);
            let id = crate::providers::ProviderId::of(provider);
            if !id.is_resolvable() {
                anyhow::bail!(
                    "{at}.provider '{shown}' names no provider this build can construct; use a name from \
                     `prx providers` or `custom:<url>`"
                );
            }
            let model = match entry.model.as_deref().map(str::trim) {
                Some("") => anyhow::bail!("{at}.model must not be empty; omit it to cover every model"),
                Some(model) => Some(model.to_ascii_lowercase()),
                None => None,
            };
            if !entry.declares_anything() {
                anyhow::bail!(
                    "{at} declares nothing: set `operations`, a feature (`native_tools`, `vision_input`, \
                     `streaming`, `structured_output`), a dialect or `context_window`"
                );
            }
            if entry.operations.as_ref().is_some_and(Vec::is_empty) {
                anyhow::bail!("{at}.operations must list at least one operation; omit it to declare none");
            }
            if entry.context_window == Some(0) {
                anyhow::bail!("{at}.context_window must be greater than zero");
            }
            let declares_dialect = entry.tool_schema_dialect.is_some() || entry.tool_call_dialect.is_some();
            if declares_dialect && !crate::providers::accepts_tool_dialect_declarations(provider) {
                anyhow::bail!(
                    "{at}.provider '{shown}' does not take a tool dialect declaration: only a \
                     `custom:<http(s) url>` endpoint and the catalog entries for a server the operator \
                     runs ({}) read one in this release",
                    crate::providers::TOOL_DIALECT_DECLARABLE_CATALOG.join(", ")
                );
            }
            if model.is_some() && entry.tool_call_dialect.is_some() {
                anyhow::bail!(
                    "{at}: `tool_call_dialect` applies to every model of an endpoint (the response parser \
                     cannot tell models apart), so it must be declared on an entry without `model`"
                );
            }
            // Compared ignoring case, shown as the operator wrote it.
            let scope = entry
                .model
                .as_deref()
                .map(str::trim)
                .map_or_else(|| "every model".to_string(), |m| format!("model '{m}'"));
            let key: Key = (id, model);
            if let Some((_, first)) = seen.iter().find(|(seen_key, _)| *seen_key == key) {
                anyhow::bail!("{at} duplicates providers.models[{first}]: both declare provider '{shown}' for {scope}");
            }
            for (other, dialects) in self.providers.tool_dialects.iter().enumerate() {
                let same_key = crate::providers::ProviderId::of(&dialects.provider) == key.0
                    && dialects.model.as_deref().map(|m| m.trim().to_ascii_lowercase()) == key.1;
                let overlaps = (entry.tool_schema_dialect.is_some() && dialects.tool_schema.is_some())
                    || (entry.tool_call_dialect.is_some() && dialects.tool_call.is_some());
                if same_key && overlaps {
                    anyhow::bail!(
                        "{at} and providers.tool_dialects[{other}] both declare a tool dialect for provider \
                         '{shown}' and {scope}; declare it in one table"
                    );
                }
            }
            seen.push((key, index));
        }
        Ok(())
    }

    /// Validate configuration values that would cause runtime failures.
    ///
    /// Called after TOML deserialization and env-override application to catch
    /// obviously invalid values early instead of failing at arbitrary runtime points.
    pub fn validate(&self) -> Result<()> {
        codex_effort::validate(self.runtime.codex_reasoning_effort.as_deref())?;
        self.causal_tree.validate()?;
        // The JWKS URI is published in the public, signed Agent Card: a
        // `user:password@` there would be handed to every peer.
        if let Some(uri) = self.a2a.card_jwks_uri.as_deref() {
            anyhow::ensure!(
                matches!(
                    crate::providers::redact_endpoint_userinfo(uri),
                    std::borrow::Cow::Borrowed(_)
                ),
                "a2a.card_jwks_uri is published in the public Agent Card and must not carry user:password@"
            );
        }
        // Runtime-interpreted enumerations are read once, canonically, and an
        // unknown value is refused here rather than falling back silently
        // (D-050). `autonomy.scopes.default` is typed (`ScopeDefault`).
        // A zero size rotates the audit trail on every record, keeping only
        // the last few (D-052).
        anyhow::ensure!(
            self.security.audit.max_size_mb >= 1,
            "security.audit.max_size_mb must be at least 1"
        );
        // The audit log is written only through real directories (every
        // component is opened without following a link, D-055), so a
        // `log_path` through a symlinked directory skips every record. This
        // is a warning, not a load error (D-063 G-1): the check reads the
        // workspace, which the agent can write, and a planted link must not
        // be able to block every configuration load, reload and write. The
        // audit logger itself still refuses to write through the link.
        #[cfg(unix)]
        if let Some(link) =
            crate::security::audit::symlinked_log_directory(&self.workspace_dir, &self.security.audit.log_path)
        {
            tracing::warn!(
                log_path = %self.security.audit.log_path,
                link = %link.display(),
                "security.audit.log_path runs through a symbolic link; the audit log is written only through \
                 real directories inside the workspace, so audit records are skipped until log_path names the \
                 real directory"
            );
        }
        // `tunnel.provider` is compared literally by the gateway's public-bind
        // guard (`== "none"`) and by the tunnel factory, so only the canonical
        // spellings are accepted: an empty or padded value would pass the
        // guard as "a tunnel" and start none.
        anyhow::ensure!(
            matches!(
                self.tunnel.provider.as_str(),
                "none" | "cloudflare" | "tailscale" | "ngrok" | "custom"
            ),
            "tunnel.provider must be one of none, cloudflare, tailscale, ngrok, custom"
        );
        if let Some(lark) = &self.channels_config.lark {
            lark.validate()?;
        }
        validate_credential_references(self)?;
        for (index, policy) in self.user_policies.iter().enumerate() {
            anyhow::ensure!(
                crate::memory::principal::Role::parse(&policy.role).is_some(),
                "user_policies[{index}].role must be one of owner, member, guest, anonymous"
            );
            anyhow::ensure!(
                crate::memory::principal::Visibility::parse(&policy.visibility_ceiling).is_some(),
                "user_policies[{index}].visibility_ceiling must be one of system, owner, private, user, group, \
                 project, public"
            );
        }
        self.self_system
            .fitness
            .timezone
            .parse::<chrono_tz::Tz>()
            .with_context(|| {
                format!(
                    "invalid self_system.fitness.timezone: {}",
                    self.self_system.fitness.timezone
                )
            })?;
        chrono::NaiveTime::parse_from_str(self.self_system.fitness.run_at.trim(), "%H:%M").with_context(|| {
            format!(
                "invalid self_system.fitness.run_at '{}'; expected HH:MM",
                self.self_system.fitness.run_at
            )
        })?;
        anyhow::ensure!(
            (1..=MAX_EVOLUTION_JUDGE_TIMEOUT_SECS).contains(&self.self_system.evolution_judge_timeout_secs),
            "self_system.evolution_judge_timeout_secs must be between 1 and {MAX_EVOLUTION_JUDGE_TIMEOUT_SECS}, got {}",
            self.self_system.evolution_judge_timeout_secs
        );
        anyhow::ensure!(
            self.self_system.fitness.min_samples > 0,
            "self_system.fitness.min_samples must be greater than zero"
        );
        anyhow::ensure!(
            self.self_system.evolution_model_max_tokens <= 1_048_576
                && i64::try_from(self.self_system.evolution_model_max_micro_usd).is_ok(),
            "self_system evolution model allowance exceeds the supported range"
        );
        anyhow::ensure!(
            self.self_system.fitness.retention_days > 0,
            "self_system.fitness.retention_days must be greater than zero"
        );
        anyhow::ensure!(
            self.self_system.fitness.max_backfill_days > 0,
            "self_system.fitness.max_backfill_days must be greater than zero"
        );
        anyhow::ensure!(
            self.self_system.fitness.min_coverage.is_finite()
                && (0.0..=1.0).contains(&self.self_system.fitness.min_coverage),
            "self_system.fitness.min_coverage must be between 0 and 1"
        );
        let notice = &self.compliance.interaction_notice;
        if notice.version.trim().is_empty() {
            anyhow::bail!("compliance.interaction_notice.version must not be empty");
        }
        if notice.applicability == InteractionNoticeApplicability::Required && notice.text.trim().is_empty() {
            anyhow::bail!("compliance.interaction_notice.text must not be empty when the notice is required");
        }
        if notice.applicability == InteractionNoticeApplicability::NotApplicable
            && (notice
                .exception_owner
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
                || notice
                    .exception_reviewed_at
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty()))
        {
            anyhow::bail!(
                "compliance.interaction_notice not_applicable requires exception_owner and exception_reviewed_at"
            );
        }
        let classification = &self.compliance.eu_ai_act.classification;
        if classification.status != EuAiActRiskClassification::Unclassified
            && (classification
                .owner
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
                || classification
                    .assessed_at
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty()))
        {
            anyhow::bail!(
                "compliance.eu_ai_act.classification requires owner and assessed_at for a legal applicability decision"
            );
        }
        // Storage
        if self.storage.provider.config.pool_max_size == 0 {
            anyhow::bail!("storage.provider.config.pool_max_size must be greater than 0");
        }

        // Channels
        if let Some(whatsapp) = self.channels_config.whatsapp.as_ref() {
            if whatsapp.read_pool_size == Some(0) {
                anyhow::bail!("channels_config.whatsapp.read_pool_size must be greater than 0");
            }
        }
        if self.memory.sqlite_read_pool_size == Some(0) {
            anyhow::bail!("memory.sqlite_read_pool_size must be greater than 0");
        }

        // Gateway
        if self.gateway.host.trim().is_empty() {
            anyhow::bail!("gateway.host must not be empty");
        }
        // A zero dedup window would remember nothing, so every platform retry of
        // an already-running delivery would start a second copy of the turn.
        if self.gateway.channel_webhook_dedup_ttl_secs == 0 {
            anyhow::bail!("gateway.channel_webhook_dedup_ttl_secs must be greater than 0");
        }
        if self.webhook.configured() {
            if self.webhook.bind.trim().is_empty() {
                anyhow::bail!("webhook.bind must not be empty when a webhook token is configured");
            }
            self.webhook
                .bind
                .parse::<std::net::SocketAddr>()
                .with_context(|| format!("invalid webhook.bind socket address: {}", self.webhook.bind))?;
            if self.webhook.token.as_deref().map(str::trim).is_none_or(str::is_empty) {
                anyhow::bail!("webhook.token must not be empty");
            }
            let webhook_backend = if self.storage.provider.config.provider.trim().is_empty() {
                self.memory.backend.as_str()
            } else {
                self.storage.provider.config.provider.as_str()
            }
            .trim()
            .to_ascii_lowercase();
            if !matches!(webhook_backend.as_str(), "sqlite" | "lucid" | "postgres") {
                anyhow::bail!(
                    "standalone webhook durable ingestion requires memory backend 'sqlite', 'lucid', or 'postgres' (got '{webhook_backend}')"
                );
            }
        }

        super::RuntimeConfig::validate_shell_env_allowlist(&self.runtime.shell_env_allowlist)?;

        // Runtime. A zero-sized blocking pool cannot run any blocking work, so
        // every `spawn_blocking` would queue forever instead of failing fast.
        if let Some(max_blocking_threads) = self.runtime.max_blocking_threads {
            if max_blocking_threads == 0 {
                anyhow::bail!("runtime.max_blocking_threads must be greater than 0");
            }
        }
        // The long-task warning is a notification, never a timeout, so any
        // positive threshold is legitimate. The only rejected range is a
        // non-zero value too small to distinguish a wedged task from an
        // ordinary tool call, which would bury the signal it exists to raise.
        if let Some(long_task_warn_secs) = self.runtime.long_task_warn_secs {
            if long_task_warn_secs > 0 && long_task_warn_secs < crate::runtime::registry::MIN_LONG_TASK_WARN_SECS {
                anyhow::bail!(
                    "runtime.long_task_warn_secs must be 0 (disabled) or at least {} seconds",
                    crate::runtime::registry::MIN_LONG_TASK_WARN_SECS
                );
            }
        }
        // Idle hang detection is the one mechanism here that *does* terminate,
        // so its bounds are stricter than the warning's. A window shorter than
        // `MIN_IDLE_HANG_SECS` could not tell a wedged turn from an ordinary
        // tool call that happens to be quiet while it works, and would start
        // killing healthy work.
        if let Some(idle_hang_secs) = self.runtime.idle_hang_secs {
            if idle_hang_secs > 0 && idle_hang_secs < crate::agent::idle::MIN_IDLE_HANG_SECS {
                anyhow::bail!(
                    "runtime.idle_hang_secs must be 0 (disabled) or at least {} seconds",
                    crate::agent::idle::MIN_IDLE_HANG_SECS
                );
            }
        }

        // Scheduler
        if self.scheduler.max_tasks == 0 {
            anyhow::bail!("scheduler.max_tasks must be greater than 0");
        }
        if self.scheduler.claim_lease_secs < 3 {
            anyhow::bail!("scheduler.claim_lease_secs must be at least 3");
        }

        // Flattened model settings accept canonical parameter names only.
        crate::config::model_settings::validate_keys(self)?;
        // Model routes
        for (i, route) in self.model_routes.iter().enumerate() {
            if route.hint.trim().is_empty() {
                anyhow::bail!("model_routes[{i}].hint must not be empty");
            }
            // The router looks a hint up exactly while the model check trims,
            // so a hint with whitespace is matched unreliably: `hint:fast`
            // passes every check against a route "fast " and then fails on
            // each request (R-PMMCh H-1).
            if route.hint.chars().any(char::is_whitespace) {
                anyhow::bail!(
                    "model_routes[{i}].hint '{}' contains whitespace, which `hint:<name>` does not reliably select; \
                     write the hint without whitespace",
                    route.hint.escape_debug()
                );
            }
            if route.provider.trim().is_empty() {
                anyhow::bail!("model_routes[{i}].provider must not be empty");
            }
            if route.model.trim().is_empty() {
                anyhow::bail!("model_routes[{i}].model must not be empty");
            }
            // Two routes with one hint are ambiguous: the model check and the
            // router could pick different ones (D-035).
            if let Some(first) = self
                .model_routes
                .iter()
                .take(i)
                .position(|earlier| earlier.hint.trim() == route.hint.trim())
            {
                anyhow::bail!(
                    "model_routes[{i}] and model_routes[{first}] both use hint '{}'; each hint must name one route",
                    route.hint.trim()
                );
            }
        }

        for (model, tools) in &self.tool_tiering.model_allowlists {
            if model.trim().is_empty() {
                anyhow::bail!("tool_tiering.model_allowlists model key must not be empty");
            }
            if let Some((index, _)) = tools.iter().enumerate().find(|(_, tool)| tool.trim().is_empty()) {
                anyhow::bail!("tool_tiering.model_allowlists[{model:?}][{index}] must not be empty");
            }
        }

        self.router.validate()?;
        self.validate_provider_tool_dialects()?;
        self.validate_provider_models()?;
        self.validate_cost_price_keys()?;

        // Embedding routes
        for (i, route) in self.embedding_routes.iter().enumerate() {
            if route.hint.trim().is_empty() {
                anyhow::bail!("embedding_routes[{i}].hint must not be empty");
            }
            if route.provider.trim().is_empty() {
                anyhow::bail!("embedding_routes[{i}].provider must not be empty");
            }
            if route.model.trim().is_empty() {
                anyhow::bail!("embedding_routes[{i}].model must not be empty");
            }
        }
        // One hint should name one embedding route, as for model routes
        // (D-035). An existing configuration with two still loads (an upgrade
        // never stops PRX from starting over it): it is reported once per
        // process here and by `prx doctor`, and a change to the embedding
        // routes that keeps a duplicate is refused (D-104 C-1).
        let duplicates = duplicate_embedding_route_hints(self);
        if !duplicates.is_empty() && REPORTED_DUPLICATE_EMBEDDING_HINTS.lock().insert(duplicates.clone()) {
            for note in &duplicates {
                tracing::warn!("{note}");
            }
        }

        for (name, agent) in &self.agents {
            // Checked for every agent, agentic or not: an unrecognised value is
            // a typo, and without this the typo only surfaces the first time
            // that agent is spawned — deep inside `sessions_spawn`, after the
            // run has already been announced to the caller.
            crate::tools::sessions_spawn::parse_memory_scope(agent.memory_scope.as_deref())
                .with_context(|| format!("agents.{name}.memory_scope"))?;
            if !agent.agentic {
                continue;
            }
            let allowed_tools = agent
                .allowed_tools
                .iter()
                .map(|tool| tool.trim())
                .filter(|tool| !tool.is_empty())
                .collect::<Vec<_>>();
            if allowed_tools.is_empty() {
                anyhow::bail!("agents.{name}.allowed_tools must not be empty when agentic=true");
            }
            if allowed_tools.contains(&"*") && allowed_tools.len() != 1 {
                anyhow::bail!("agents.{name}.allowed_tools must use '*' exclusively");
            }
        }

        // Proxy (delegate to existing validation)
        self.proxy.validate()?;

        // Memory subtree (FIX-P2-09): previously the [memory] subtree was not
        // validated at all; wire it into the top-level validator.
        self.memory.validate()?;
        self.sessions_spawn.validate()?;

        // Smart group-reply pre-gate numeric ranges.
        self.smart_group.validate()?;

        // wacli channel (webhook + CLI) settings.
        if let Some(wacli) = self.channels_config.wacli.as_ref() {
            wacli.validate().context("invalid channels_config.wacli")?;
        }

        // Scope ACLs, including the outbound recipient allow/deny entries.
        self.autonomy.scopes.validate()?;

        // Chat's link to the daemon it borrows channels from.
        self.chat.validate()?;

        // Operator-owned skill lifecycle policy: a weak policy is refused.
        self.skills_lifecycle.validate()?;

        // A stdio MCP server's `working_dir` is absolute (D-095 N-4); whether
        // it exists is checked when a change sets it (the configuration
        // transaction), not at every load.
        let mut servers = self.mcp.servers.iter().collect::<Vec<_>>();
        servers.sort_by(|left, right| left.0.cmp(right.0));
        for (name, server) in servers {
            if let Some(dir) = server.working_dir.as_deref() {
                anyhow::ensure!(
                    std::path::Path::new(dir.trim()).is_absolute(),
                    "mcp.servers.{}.working_dir must be an absolute path",
                    name.escape_default()
                );
            }
        }

        // The MCP caller identity is `agent:{issuer}:{subject}` and the owner id
        // wraps it, and that string is an authorization boundary. The issuer
        // component is only ever one of the fixed origin names or this single
        // configured value, and the components are joined with `:` without
        // escaping (escaping them would move every real caller's owner id).
        // Two distinct callers can therefore only compose to the same id if the
        // configured issuer collides with a fixed origin name, so refuse that
        // here rather than letting the ambiguity exist at runtime.
        if let Some(jwt_issuer) = self.mcp_server.jwt_issuer.as_deref().map(str::trim) {
            for fixed in crate::gateway::FIXED_EXTERNAL_ISSUERS {
                if jwt_issuer == fixed || jwt_issuer.starts_with(&format!("{fixed}:")) {
                    anyhow::bail!(
                        "mcp_server.jwt_issuer must not be '{fixed}' or start with '{fixed}:' \
                         (configured: '{jwt_issuer}'): the MCP caller identity is \
                         agent:{{issuer}}:{{subject}} and '{fixed}' is the reserved issuer of a \
                         built-in origin, so such a value would let a bearer caller and a {fixed} \
                         caller resolve to the same owner id"
                    );
                }
            }
        }

        // MCP discovery advertises what `exposed_tools` names. A side-effecting
        // entry is allowed but must be a visible choice, not a silent one.
        let side_effecting = self.mcp_server.side_effecting_exposed_tools();
        if !side_effecting.is_empty() {
            tracing::warn!(
                tools = side_effecting.join(", "),
                "mcp_server.exposed_tools exposes tools with side effects; they are advertised with their real effect and need an Idempotency-Key to be retry-safe"
            );
        }
        // A warning, not an error: an entry that became inert must not stop an
        // existing daemon from starting.
        let refused = self.mcp_server.refused_exposed_tools();
        if !refused.is_empty() {
            tracing::warn!(
                tools = refused.join(", "),
                "mcp_server.exposed_tools lists tools that do not enforce per-owner isolation; external MCP callers are refused them"
            );
        }

        Ok(())
    }

    pub async fn save(&self) -> Result<()> {
        let plan = crate::config::files::plan_structured_save(self)?;
        crate::config::files::commit_mutation_atomically(plan).await
    }

    /// The stored form of this configuration: every encrypted-at-rest path of
    /// the secret registry is encrypted when `secrets.encrypt = true`.
    pub fn to_stored_toml_value(&self) -> Result<toml::Value> {
        let openprx_dir = self
            .config_path
            .parent()
            .context("Config path must have a parent directory")?;
        let store = crate::security::SecretStore::new(openprx_dir, self.secrets.encrypt);
        let mut value = toml::Value::try_from(self).context("Failed to convert config into TOML value")?;
        crate::config::secret_paths::encrypt_at_rest(&mut value, &store)?;
        Ok(value)
    }

    pub(crate) fn to_stored_toml_string(&self) -> Result<String> {
        let value = self.to_stored_toml_value()?;
        toml::to_string_pretty(&value).context("Failed to serialize config")
    }

    pub fn to_split_toml_strings(&self) -> Result<(String, Vec<(String, String)>)> {
        let value = self.to_stored_toml_value()?;
        let (main_value, fragment_values) = build_split_tables(&value)?;
        let main = toml::to_string_pretty(&main_value).context("Failed to serialize main config")?;
        let mut fragments = Vec::with_capacity(fragment_values.len());
        for (name, value) in fragment_values {
            let rendered = toml::to_string_pretty(&value)
                .with_context(|| format!("Failed to serialize split fragment: {name}"))?;
            fragments.push((name, rendered));
        }
        Ok((main, fragments))
    }
}

/// Whether the stored configuration writes `memory.embedding_provider` (or its
/// migrated legacy spelling `[memory.embedding].provider`) at all.
pub(super) fn memory_embedding_provider_present(config: &toml::Value) -> bool {
    let Some(memory) = config.get("memory") else {
        return false;
    };
    memory.get("embedding_provider").is_some()
        || memory
            .get("embedding")
            .and_then(|embedding| embedding.get("provider"))
            .is_some()
}

pub(super) fn agent_compaction_max_context_tokens_present(config: &toml::Value) -> bool {
    config
        .get("agent")
        .and_then(|agent| agent.get("compaction"))
        .and_then(|compaction| compaction.get("max_context_tokens"))
        .is_some()
}
