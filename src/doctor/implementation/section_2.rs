fn check_config_semantics(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "config";

    // A neutral statement of the active level; limits being absent is the
    // default and never a warning. `acknowledge_unrestricted_profile` is kept
    // for existing configs and no longer changes any output.
    items.push(DiagItem::configured(
        cat,
        format!("autonomy level: {:?}", config.autonomy.level),
    ));

    // Config file exists
    if config.config_path.exists() {
        items.push(DiagItem::configured(
            cat,
            format!("config file: {}", config.config_path.display()),
        ));
    } else {
        items.push(DiagItem::error(
            cat,
            format!("config file not found: {}", config.config_path.display()),
        ));
    }

    // Provider validity
    if let Some(ref provider) = config.default_provider {
        if let Some(reason) = provider_validation_error(
            config,
            provider,
            crate::providers::selection::default_model(config),
            crate::providers::selection::credential_for(config, provider),
        ) {
            items.push(DiagItem::error(
                cat,
                format!("default provider \"{provider}\" is invalid: {reason}"),
            ));
        } else {
            items.push(DiagItem::configured(cat, format!("provider \"{provider}\" is valid")));
        }
    } else {
        items.push(DiagItem::error(cat, "no default_provider configured"));
    }

    // API key / auth profile presence
    if config.default_provider.as_deref() != Some("ollama") {
        if config.api_key.is_some() {
            items.push(DiagItem::configured(cat, "API key configured"));
        } else if let Some(provider) = config.default_provider.as_deref() {
            match active_auth_profile_credential_status(config, provider) {
                AuthProfileCredentialStatus::Present { profile_id } => {
                    items.push(DiagItem::configured(
                        cat,
                        format!("auth profile credential configured for \"{provider}\" ({profile_id})"),
                    ));
                }
                AuthProfileCredentialStatus::Unreadable(error) => {
                    items.push(DiagItem::warn(
                        cat,
                        format!("auth profile check failed for \"{provider}\": {error}"),
                    ));
                }
                AuthProfileCredentialStatus::Missing => {
                    // A credential stored through `PUT /api/providers/{id}/credential`
                    // or `prx providers credential set` is a `cred-<name>` reference in
                    // the same encrypted store. It is used only when the configuration
                    // references it (`credential_ref`), the same test `prx providers
                    // credential status` applies. Presence only; no value is read out.
                    match stored_credential_status(config, provider) {
                        StoredCredentialStatus::Referenced { id, users } => {
                            items.push(DiagItem::configured(
                                cat,
                                format!(
                                    "stored credential {id} for \"{provider}\" is referenced by {}",
                                    users.join(", ")
                                ),
                            ));
                        }
                        StoredCredentialStatus::Unreferenced { id } => {
                            items.push(DiagItem::warn(
                                cat,
                                format!(
                                    "stored credential {id} exists for \"{provider}\" but is not referenced by any \
                                     model route, embedding route or agent (`credential_ref`), so it is not used"
                                ),
                            ));
                        }
                        StoredCredentialStatus::None => {
                            items.push(DiagItem::warn(cat, "no api_key or active auth profile credential set"));
                        }
                    }
                }
            }
        } else {
            items.push(DiagItem::warn(cat, "no api_key or default provider set"));
        }
    }

    // Model configured
    if config.default_model.is_some() {
        items.push(DiagItem::configured(
            cat,
            format!("default model: {}", config.default_model.as_deref().unwrap_or("?")),
        ));
    } else {
        items.push(DiagItem::warn(cat, "no default_model configured"));
    }

    // Temperature range
    if config.default_temperature >= 0.0 && config.default_temperature <= 2.0 {
        items.push(DiagItem::configured(
            cat,
            format!("temperature {:.1} (valid range 0.0–2.0)", config.default_temperature),
        ));
    } else {
        items.push(DiagItem::error(
            cat,
            format!(
                "temperature {:.1} is out of range (expected 0.0–2.0)",
                config.default_temperature
            ),
        ));
    }

    // Gateway port range
    let port = config.gateway.port;
    if port > 0 {
        items.push(DiagItem::configured(cat, format!("gateway port: {port}")));
    } else {
        items.push(DiagItem::error(cat, "gateway port is 0 (invalid)"));
    }

    // Reliability: fallback providers
    for fb in &config.reliability.fallback_providers {
        // The runtime builds a fallback member other than the primary with no
        // released credential (its own env/OAuth store, providers/mod.rs
        // `create_provider_with_options(name, None, ..)`), so the check uses
        // the same; a member that is the default provider runs on the primary.
        let is_default = crate::providers::ProviderId::of(fb)
            == crate::providers::ProviderId::of(crate::providers::selection::default_provider(config));
        let credential = if is_default {
            crate::providers::selection::credential_for(config, fb)
        } else {
            crate::providers::credentials::CredentialRef::ProviderStore
        };
        if let Some(reason) = provider_validation_error(
            config,
            fb,
            crate::providers::selection::default_model(config),
            credential,
        ) {
            items.push(DiagItem::warn(
                cat,
                format!("fallback provider \"{fb}\" is invalid: {reason}"),
            ));
        }
        // A self-hosted fallback answers only `model_fallbacks` entries that
        // name a model repository (D-076 N-1); without one it is never used.
        let default_is_same = config
            .default_provider
            .as_deref()
            .is_some_and(|default| default.trim().eq_ignore_ascii_case(fb.trim()));
        if crate::providers::is_self_hosted_model_server(fb)
            && !default_is_same
            && !config
                .reliability
                .model_fallbacks
                .values()
                .flatten()
                .any(|model| crate::providers::names_model_repository(model))
        {
            items.push(DiagItem::warn(
                cat,
                format!(
                    "fallback provider \"{fb}\" is never used: as a fallback it answers only \
                     reliability.model_fallbacks entries that name one of its model repositories \
                     (org/repository), and none does"
                ),
            ));
        }
    }
    let route_providers: HashSet<String> = config
        .model_routes
        .iter()
        .map(|route| route.provider.trim().to_string())
        .filter(|provider| !provider.is_empty())
        .collect();
    for (source_model, fallback_chain) in &config.reliability.model_fallbacks {
        if fallback_chain.is_empty() {
            items.push(DiagItem::warn(
                cat,
                format!(
                    "model_fallbacks entry \"{source_model}\" has an empty fallback chain; it will never take effect"
                ),
            ));
            continue;
        }

        for fallback_model in fallback_chain {
            if !model_has_reachable_provider(
                crate::providers::selection::default_provider(config),
                &config.reliability.fallback_providers,
                &route_providers,
                fallback_model,
            ) {
                items.push(DiagItem::warn(
                    cat,
                    format!(
                        "model_fallbacks route mismatch: fallback model \"{fallback_model}\" has no compatible provider in default/fallback/model_routes"
                    ),
                ));
            }
        }
    }

    let availability = crate::providers::summarize_provider_availability(
        crate::providers::selection::default_provider(config),
        config.api_key.as_deref(),
        &config.reliability,
        &crate::providers::provider_runtime_options_from_config(config),
    );
    if availability.degraded && !config.reliability.acknowledge_single_provider_risk {
        items.push(DiagItem::warn(
            cat,
            format!(
                "provider resilience degraded: only {} configured+available provider(s) (need >=2). available=[{}]",
                availability.available.len(),
                availability.available.join(", ")
            ),
        ));
    } else {
        items.push(DiagItem::ready(
            cat,
            format!(
                "provider resilience ready: {} configured+available providers [{}]",
                availability.available.len(),
                availability.available.join(", ")
            ),
        ));
    }
    // A provider whose stored credential the configuration references is not
    // missing one, whatever the availability summary (which looks at the root
    // key and the environment) says; the credential line above names it.
    let unavailable: Vec<String> = availability
        .unavailable
        .iter()
        .filter(|(name, _)| {
            !matches!(
                stored_credential_status(config, name),
                StoredCredentialStatus::Referenced { .. }
            )
        })
        .map(|(name, reason)| format!("{name}: {reason}"))
        .collect();
    if !unavailable.is_empty() {
        items.push(DiagItem::warn(
            cat,
            format!("unavailable providers: {}", unavailable.join("; ")),
        ));
    }

    // Model routes validation
    for route in &config.model_routes {
        if route.hint.is_empty() {
            items.push(DiagItem::warn(cat, "model route with empty hint"));
        }
        let credential = route.credential_ref.as_deref().map_or_else(
            || {
                if route.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) {
                    crate::providers::credentials::CredentialRef::LegacyInline(
                        crate::providers::credentials::LegacySlot::ModelRoute(route.hint.clone()),
                    )
                } else {
                    crate::providers::selection::credential_for(config, &route.provider)
                }
            },
            |id| crate::providers::credentials::CredentialRef::Profile { id: id.to_string() },
        );
        if let Some(reason) = provider_validation_error(config, &route.provider, &route.model, credential) {
            items.push(DiagItem::warn(
                cat,
                format!(
                    "model route \"{}\" uses invalid provider \"{}\": {}",
                    route.hint, route.provider, reason
                ),
            ));
        }
        if route.model.is_empty() {
            items.push(DiagItem::warn(
                cat,
                format!("model route \"{}\" has empty model", route.hint),
            ));
        }
    }

    // Embedding routes validation
    for route in &config.embedding_routes {
        if route.hint.trim().is_empty() {
            items.push(DiagItem::warn(cat, "embedding route with empty hint"));
        }
        if let Some(reason) = embedding_provider_validation_error(&route.provider) {
            items.push(DiagItem::warn(
                cat,
                format!(
                    "embedding route \"{}\" uses invalid provider \"{}\": {}",
                    route.hint, route.provider, reason
                ),
            ));
        }
        if route.model.trim().is_empty() {
            items.push(DiagItem::warn(
                cat,
                format!("embedding route \"{}\" has empty model", route.hint),
            ));
        }
        if route.dimensions.is_some_and(|value| value == 0) {
            items.push(DiagItem::warn(
                cat,
                format!("embedding route \"{}\" has invalid dimensions=0", route.hint),
            ));
        }
    }

    if let Some(hint) = config
        .memory
        .embedding_model
        .strip_prefix("hint:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !config.embedding_routes.iter().any(|route| route.hint.trim() == hint) {
            items.push(DiagItem::warn(
                cat,
                format!(
                    "memory.embedding_model uses hint \"{hint}\" but no matching [[embedding_routes]] entry exists"
                ),
            ));
        }
    }

    // Channel: at least one configured
    let cc = &config.channels_config;
    let has_channel = cc.telegram.is_some()
        || cc.discord.is_some()
        || cc.slack.is_some()
        || cc.imessage.is_some()
        || cc.whatsapp.is_some()
        || cc.wacli.is_some()
        || cc.nextcloud_talk.is_some()
        || cc.email.is_some()
        || cc.irc.is_some()
        || cc.lark.is_some()
        || cc.webhook.is_some();

    if has_channel {
        items.push(DiagItem::configured(cat, "at least one channel configured"));
    } else {
        items.push(DiagItem::warn(
            cat,
            "no channels configured — run `prx init` to set one up",
        ));
    }

    // Delegate agents: provider validity
    let mut agent_names: Vec<_> = config.agents.keys().collect();
    agent_names.sort();
    for name in agent_names {
        let Some(agent) = config.agents.get(name) else {
            continue;
        };
        let credential = agent.credential_ref.as_deref().map_or_else(
            || {
                if agent.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) {
                    crate::providers::credentials::CredentialRef::LegacyInline(
                        crate::providers::credentials::LegacySlot::Agent(name.clone()),
                    )
                } else {
                    crate::providers::selection::credential_for(config, &agent.provider)
                }
            },
            |id| crate::providers::credentials::CredentialRef::Profile { id: id.to_string() },
        );
        if let Some(reason) = provider_validation_error(config, &agent.provider, &agent.model, credential) {
            items.push(DiagItem::warn(
                cat,
                format!(
                    "agent \"{name}\" uses invalid provider \"{}\": {}",
                    agent.provider, reason
                ),
            ));
        }
        if agent.agentic && agent.allowed_tools.iter().any(|tool| tool.trim() == "*") {
            items.push(DiagItem::warn(
                cat,
                format!("agent \"{name}\" explicitly inherits all eligible parent tools"),
            ));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthProfileCredentialStatus {
    Present { profile_id: String },
    Missing,
    Unreadable(String),
}

/// What the credential store holds for a provider, by reference id only.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StoredCredentialStatus {
    /// A stored credential some part of the configuration references.
    Referenced { id: String, users: Vec<String> },
    /// A stored credential nothing references: it is never sent.
    Unreferenced { id: String },
    /// No stored credential for the provider, or the store is unreadable.
    None,
}

fn stored_credential_status(config: &Config, provider: &str) -> StoredCredentialStatus {
    let credentials = match crate::config::manage::service::provider_credentials(config, provider) {
        Ok(credentials) => credentials,
        Err(error) => {
            tracing::warn!(%error, "doctor could not read the stored credential references");
            return StoredCredentialStatus::None;
        }
    };
    let mut unreferenced = None;
    for reference in &credentials.references {
        match credentials.reference_uses.get(&reference.id) {
            Some(users) if !users.is_empty() => {
                return StoredCredentialStatus::Referenced {
                    id: reference.id.clone(),
                    users: users.clone(),
                };
            }
            _ => {
                unreferenced.get_or_insert_with(|| reference.id.clone());
            }
        }
    }
    unreferenced.map_or(StoredCredentialStatus::None, |id| StoredCredentialStatus::Unreferenced { id })
}

fn active_auth_profile_credential_status(config: &Config, provider: &str) -> AuthProfileCredentialStatus {
    let Some(openprx_dir) = config.config_path.parent() else {
        return AuthProfileCredentialStatus::Missing;
    };
    let canonical_provider = crate::providers::canonical_china_provider_name(provider)
        .unwrap_or(provider)
        .to_string();
    // Read-only: a diagnosis never rewrites the profiles file (D-084 E-3).
    let store = crate::auth::profiles::AuthProfilesStore::new(openprx_dir, config.secrets.encrypt);
    let data = match store.load_read_only() {
        Ok(data) => data,
        Err(error) => return AuthProfileCredentialStatus::Unreadable(error.to_string()),
    };
    let Some(profile_id) = data.active_profiles.get(&canonical_provider) else {
        return AuthProfileCredentialStatus::Missing;
    };
    let Some(profile) = data.profiles.get(profile_id) else {
        return AuthProfileCredentialStatus::Missing;
    };
    let credential = profile
        .token
        .as_deref()
        .or_else(|| profile.token_set.as_ref().map(|tokens| tokens.access_token.as_str()))
        .map(str::trim);
    if credential.is_some_and(|token| !token.is_empty()) {
        AuthProfileCredentialStatus::Present {
            profile_id: profile_id.clone(),
        }
    } else {
        AuthProfileCredentialStatus::Missing
    }
}

fn provider_validation_error(
    config: &Config,
    name: &str,
    model: &str,
    credential: crate::providers::credentials::CredentialRef,
) -> Option<String> {
    let catalog = crate::providers::selection::catalog_for(config);
    let resolved = match crate::providers::selection::check_with(
        config,
        &catalog,
        name,
        model,
        credential,
        &crate::providers::catalog::Requirement::chat(),
        crate::providers::catalog::UnknownPolicy::Invocation,
    ) {
        Ok(resolved) => resolved,
        Err(error @ crate::providers::catalog::ResolutionError::UnknownProvider { .. }) => {
            return Some(format!("Unknown provider: {error}"));
        }
        Err(error) => return Some(error.to_string()),
    };
    let mut reliability = config.reliability.clone();
    reliability.fallback_providers.clear();
    reliability.api_keys.clear();
    reliability.model_fallbacks.clear();
    match crate::providers::create_provider_for(
        &resolved,
        &catalog,
        &reliability,
        &config.cost,
        &crate::providers::provider_runtime_options_from_config(config),
    ) {
        Ok(_) => None,
        // A missing credential is reported by the credential checks, not
        // as an invalid provider here.
        Err(error)
            if error
                .downcast_ref::<crate::providers::MissingProviderCredential>()
                .is_some() =>
        {
            None
        }
        Err(error) => Some(error.to_string().lines().next().unwrap_or("invalid provider").into()),
    }
}

fn model_has_reachable_provider(
    default_provider: &str,
    fallback_providers: &[String],
    route_providers: &HashSet<String>,
    model: &str,
) -> bool {
    // As the failover chain decides (D-075): a self-hosted model server
    // serves any repository on its own route (the default provider, a model
    // route) and never answers as a fallback for another provider's route.
    let serves_own_route = |provider: &str| {
        if crate::providers::is_self_hosted_model_server(provider) {
            crate::providers::provider_serves_configured_model(provider, model)
        } else {
            crate::providers::provider_matches_model_prefix(provider, model)
        }
    };
    if serves_own_route(default_provider) {
        return true;
    }
    if fallback_providers.iter().any(|provider| {
        if crate::providers::is_self_hosted_model_server(provider) {
            // A fallback model is a `model_fallbacks` entry (D-076 N-1).
            crate::providers::names_model_repository(model)
        } else {
            crate::providers::provider_matches_model_prefix(provider, model)
        }
    }) {
        return true;
    }
    route_providers.iter().any(|provider| serves_own_route(provider))
}

fn embedding_provider_validation_error(name: &str) -> Option<String> {
    let normalized = name.trim();
    if normalized.eq_ignore_ascii_case("local")
        || normalized.eq_ignore_ascii_case("none")
        || normalized.eq_ignore_ascii_case("openai")
    {
        return None;
    }

    let Some(url) = normalized.strip_prefix("custom:") else {
        return Some("supported values: local, none, openai, custom:<url>".into());
    };

    let url = url.trim();
    if url.is_empty() {
        return Some("custom provider requires a non-empty URL after 'custom:'".into());
    }

    match reqwest::Url::parse(url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => None,
        Ok(parsed) => Some(format!(
            "custom provider URL must use http/https, got '{}'",
            parsed.scheme()
        )),
        Err(err) => Some(format!("invalid custom provider URL: {err}")),
    }
}

// ── Workspace integrity ──────────────────────────────────────────

fn check_workspace(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "workspace";
    let ws = &config.workspace_dir;

    if ws.exists() {
        items.push(DiagItem::ok(cat, format!("directory exists: {}", ws.display())));
    } else {
        items.push(DiagItem::error(cat, format!("directory missing: {}", ws.display())));
        return;
    }

    // Doctor is diagnostic-only: prove readability and report declared write
    // permission without creating a probe file.
    match std::fs::read_dir(ws) {
        Ok(_) => items.push(DiagItem::ready(cat, "directory is readable; no write probe performed")),
        Err(error) => items.push(DiagItem::error(cat, format!("directory is not readable: {error}"))),
    }
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(ws) {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o222 == 0 {
            items.push(DiagItem::with_state(
                Severity::Warn,
                DiagnosticState::Configured,
                cat,
                "directory has no declared write permission bits; write capability was not exercised",
            ));
        } else {
            items.push(DiagItem::configured(
                cat,
                "directory declares write permission; capability was not exercised",
            ));
        }
    }

    // Disk space (best-effort via `df`)
    if let Some(avail_mb) = disk_available_mb(ws) {
        if avail_mb >= 100 {
            items.push(DiagItem::ok(cat, format!("disk space: {avail_mb} MB available")));
        } else {
            items.push(DiagItem::warn(
                cat,
                format!("low disk space: only {avail_mb} MB available"),
            ));
        }
    }

    // Key workspace files
    for name in ["SOUL.md", "AGENTS.md"] {
        check_file_exists(
            &crate::identity::root::file_directory(ws, name),
            name,
            false,
            cat,
            items,
        );
    }
}

fn check_file_exists(base: &Path, name: &str, required: bool, cat: &'static str, items: &mut Vec<DiagItem>) {
    let path = base.join(name);
    if path.is_file() {
        items.push(DiagItem::ok(cat, format!("{name} present")));
    } else if required {
        items.push(DiagItem::error(cat, format!("{name} missing")));
    } else {
        items.push(DiagItem::warn(cat, format!("{name} not found (optional)")));
    }
}

fn disk_available_mb(path: &Path) -> Option<u64> {
    let output = std::process::Command::new("df").arg("-m").arg(path).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_df_available_mb(&stdout)
}

fn parse_df_available_mb(stdout: &str) -> Option<u64> {
    let line = stdout.lines().rev().find(|line| !line.trim().is_empty())?;
    let avail = line.split_whitespace().nth(3)?;
    avail.parse::<u64>().ok()
}

// ── Daemon state (original logic, preserved) ─────────────────────

fn check_daemon_state(config: &Config, items: &mut Vec<DiagItem>) {
    let cat = "daemon";
    let state_file = crate::daemon::state_file_path(config);

    if !state_file.exists() {
        // No state file means no daemon ever ran here (a fresh state
        // directory): report it as not running, not as a failure. A file that
        // exists but is unreadable, corrupt or stale stays an error below.
        items.push(DiagItem::warn(
            cat,
            format!(
                "daemon not running (no state file at {}); start it with `prx daemon`",
                state_file.display()
            ),
        ));
        return;
    }

    let raw = match std::fs::read_to_string(&state_file) {
        Ok(r) => r,
        Err(e) => {
            items.push(DiagItem::error(cat, format!("cannot read state file: {e}")));
            return;
        }
    };

    let snapshot: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            items.push(DiagItem::error(cat, format!("invalid state JSON: {e}")));
            return;
        }
    };

    // Daemon heartbeat freshness
    let updated_at = snapshot
        .get("updated_at")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");

    if let Ok(ts) = DateTime::parse_from_rfc3339(updated_at) {
        let age = Utc::now().signed_duration_since(ts.with_timezone(&Utc)).num_seconds();
        if age <= DAEMON_STALE_SECONDS {
            items.push(DiagItem::ok(cat, format!("heartbeat fresh ({age}s ago)")));
        } else {
            items.push(DiagItem::error(cat, format!("heartbeat stale ({age}s ago)")));
        }
    } else {
        items.push(DiagItem::error(cat, format!("invalid daemon timestamp: {updated_at}")));
    }

    if let Some(generation) = snapshot.get("config_generation") {
        let active = generation.get("active_generation").and_then(serde_json::Value::as_u64);
        let desired_revision = generation
            .get("desired_source_revision")
            .and_then(|revision| revision.get("fingerprint_sha256"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let in_progress = generation
            .get("reload_in_progress")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        items.push(DiagItem::ok(
            "config-generation",
            format!(
                "active generation {} (desired revision {}, reload_in_progress={in_progress})",
                active.map_or_else(|| "unknown".to_string(), |value| value.to_string()),
                desired_revision
            ),
        ));

        if let Some(failure) = generation.get("last_failure").filter(|failure| !failure.is_null()) {
            let error = failure
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown reload failure");
            items.push(DiagItem::error(
                "config-generation",
                format!("last config reload failed: {error}"),
            ));
        } else if let Some(restart_required) = generation
            .get("last_report")
            .and_then(|report| report.get("restart_required"))
            .and_then(serde_json::Value::as_array)
            .filter(|fields| !fields.is_empty())
        {
            let fields = restart_required
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            items.push(DiagItem::warn(
                "config-generation",
                format!("desired config requires process restart for: {fields}"),
            ));
        }
    } else {
        items.push(DiagItem::unknown(
            "config-generation",
            "daemon state does not expose config generation status",
        ));
    }

    // Components
    if let Some(components) = snapshot.get("components").and_then(serde_json::Value::as_object) {
        // Do not hard-code runtime truth to scheduler and channel:* only. Any
        // tracked active owner that reports unhealthy must be visible in doctor.
        for (name, component) in components {
            if name == "scheduler" || name.starts_with("channel:") {
                continue;
            }
            let status = component
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            if status == "ok" || status == "disabled" {
                continue;
            }
            let detail = component
                .get("last_error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("runtime component is not healthy");
            items.push(DiagItem::error(cat, format!("{name} unhealthy ({status}): {detail}")));
        }

        // Scheduler
        if let Some(scheduler) = components.get("scheduler") {
            {
                let scheduler_ok = scheduler
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|s| s == "ok");
                let scheduler_age = scheduler
                    .get("last_ok")
                    .and_then(serde_json::Value::as_str)
                    .and_then(parse_rfc3339)
                    .map_or(i64::MAX, |dt| Utc::now().signed_duration_since(dt).num_seconds());

                if scheduler_ok && scheduler_age <= SCHEDULER_STALE_SECONDS {
                    items.push(DiagItem::ok(
                        cat,
                        format!("scheduler healthy (last ok {scheduler_age}s ago)"),
                    ));
                } else {
                    items.push(DiagItem::error(
                        cat,
                        format!("scheduler unhealthy (ok={scheduler_ok}, age={scheduler_age}s)"),
                    ));
                }
            }
        } else {
            items.push(DiagItem::unknown(cat, "scheduler component not tracked yet"));
        }

        // Channels
        let mut channel_count = 0u32;
        let mut stale = 0u32;
        for (name, component) in components {
            if !name.starts_with("channel:") {
                continue;
            }
            channel_count += 1;
            let status_ok = component
                .get("status")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| s == "ok");
            let age = component
                .get("last_ok")
                .and_then(serde_json::Value::as_str)
                .and_then(parse_rfc3339)
                .map_or(i64::MAX, |dt| Utc::now().signed_duration_since(dt).num_seconds());

            // Measured listener activity, when the channel publishes it. The
            // supervisor derives health from real traffic rather than from its own
            // heartbeat timer, so a wedged listener shows up here as a stall with
            // the silence duration attached. Purely informational: `prx doctor`
            // reports, it never restarts anything.
            let activity = component.get("activity");
            let stalled = activity
                .and_then(|activity| activity.get("stalled"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let idle_seconds = activity
                .and_then(|activity| activity.get("idle_seconds"))
                .and_then(serde_json::Value::as_u64);
            let liveness = activity
                .and_then(|activity| activity.get("liveness"))
                .and_then(serde_json::Value::as_str);

            if stalled {
                stale += 1;
                let idle = idle_seconds.unwrap_or_default();
                items.push(DiagItem::error(
                    cat,
                    format!("{name} listener stalled (no receive activity for {idle}s)"),
                ));
            } else if status_ok && age <= CHANNEL_STALE_SECONDS {
                let detail = match (liveness, idle_seconds) {
                    (Some("passive"), Some(idle)) => {
                        // No cadence to measure against, so "fresh" here means the
                        // supervisor is alive, not that the channel proved itself.
                        format!("{name} fresh ({age}s ago, push-only, idle {idle}s)")
                    }
                    (_, Some(idle)) => format!("{name} fresh ({age}s ago, idle {idle}s)"),
                    (_, None) => format!("{name} fresh ({age}s ago)"),
                };
                items.push(DiagItem::ok(cat, detail));
            } else {
                stale += 1;
                items.push(DiagItem::error(
                    cat,
                    format!("{name} stale (ok={status_ok}, age={age}s)"),
                ));
            }
        }

        if channel_count == 0 {
            items.push(DiagItem::unknown(cat, "no channel components tracked yet"));
        } else if stale > 0 {
            items.push(DiagItem::warn(cat, format!("{channel_count} channels, {stale} stale")));
        }
    }
}

// ── Daemon process PATH ──────────────────────────────────────────
//
// A hand-written unit without `Environment="PATH=…"` leaves the daemon with
// systemd's default PATH, which does not contain `~/.cargo/bin`. The model
// then gets `sh: prx: not found` from the shell tool while `prx doctor`, run
// from an interactive shell, sees a perfectly healthy PATH. The daemon's own
// environment is the only place that truth is visible, so read it from
// `/proc/<pid>/environ` instead of trusting the doctor process.

const DAEMON_PATH_CATEGORY: &str = "daemon";

/// Non-failing outcome: the daemon PATH could not be inspected at all.
/// Reported as `Ok`/`Unknown` so an uninspectable daemon never turns
/// `prx doctor` red.
fn daemon_path_skipped(msg: impl Into<String>) -> DiagItem {
    DiagItem::with_state(Severity::Ok, DiagnosticState::Unknown, DAEMON_PATH_CATEGORY, msg)
}

#[cfg(any(target_os = "linux", test))]
fn canonical_or_owned(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// True for a Cargo build output directory (`…/<output>/debug`,
/// `…/target/release`, `…/target/<triple>/debug/deps`, …).
///
/// Such a directory must never enter the required set: `cargo run -- doctor`,
/// `./target/release/prx doctor` and one-shot CI copies all run from one, and
/// the repair text would then tell the operator to put a build directory on the
/// daemon's `PATH` — an operational step that outlives the build tree.
#[cfg(any(target_os = "linux", test))]
fn is_cargo_build_dir(path: &Path) -> bool {
    let profile_dir = if path.file_name().is_some_and(|name| name == "deps") {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    if !profile_dir
        .file_name()
        .is_some_and(|name| name == "debug" || name == "release")
    {
        return false;
    }
    // `deps` is a Cargo artifact directory even when a copied layout no
    // longer has its siblings. A profile directory itself needs layout proof.
    path != profile_dir
        || ["deps", "build", ".fingerprint"]
            .iter()
            .any(|name| profile_dir.join(name).is_dir())
}

/// The deployed daemon binary as the daemon itself recorded it in
/// `daemon_state.json`, when that field is present.
///
/// This is the authoritative answer to "which directory must the daemon's PATH
/// contain": it is the daemon's own binary, not the doctor's.
#[cfg(any(target_os = "linux", test))]
fn daemon_binary_from_state(config: &Config) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(crate::daemon::state_file_path(config)).ok()?;
    let snapshot: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let recorded = snapshot.get("binary_path").and_then(serde_json::Value::as_str)?;
    let trimmed = recorded.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// Directories the daemon PATH must contain for a shell tool to resolve `prx`.
///
/// The daemon's own binary directory (from `daemon_state.json`) when it is
/// recorded, otherwise this doctor's directory as a best-effort stand-in — and
/// then only when it is not a build output directory. Plus `~/.cargo/bin` when
/// it exists, the default `cargo install` target.
#[cfg(any(target_os = "linux", test))]
fn required_daemon_path_dirs(daemon_binary: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let recorded_parent = daemon_binary.and_then(Path::parent).map(canonical_or_owned);
    if let Some(parent) = recorded_parent {
        dirs.push(parent);
    } else if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent().map(canonical_or_owned) {
            if !is_cargo_build_dir(&parent) {
                dirs.push(parent);
            }
        }
    }
    if let Some(home) = crate::config::user_home_dir() {
        let cargo_bin = home.join(".cargo").join("bin");
        if cargo_bin.is_dir() {
            dirs.push(canonical_or_owned(&cargo_bin));
        }
    }
    let mut unique: Vec<PathBuf> = Vec::with_capacity(dirs.len());
    for dir in dirs {
        if !unique.contains(&dir) {
            unique.push(dir);
        }
    }
    unique
}

/// True when a process identity (its `exe` link and/or its `cmdline` payload)
/// belongs to a prx binary.
///
/// Only the *file name* is matched, never the whole path: a prx checkout lives
/// in a directory called `prx`, so a path match would call every process
/// started from that tree a daemon.
#[cfg(any(target_os = "linux", test))]
fn process_identity_looks_like_prx(exe: Option<&Path>, cmdline: &[u8]) -> bool {
    let named_prx = |path: &Path| {
        path.file_name()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|name| name.contains("prx"))
    };
    if exe.is_some_and(named_prx) {
        return true;
    }
    cmdline
        .split(|byte| *byte == 0)
        .find(|entry| !entry.is_empty())
        .and_then(|argv0| std::str::from_utf8(argv0).ok())
        .is_some_and(|argv0| named_prx(Path::new(argv0)))
}

/// Extract `PATH` from a NUL-separated `/proc/<pid>/environ` payload.
#[cfg(any(target_os = "linux", test))]
fn daemon_path_from_environ(bytes: &[u8]) -> Option<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix("PATH=").map(str::to_owned))
}

#[cfg(any(target_os = "linux", test))]
fn path_contains_dir(path: &str, dir: &Path) -> bool {
    std::env::split_paths(path)
        .filter(|entry| !entry.as_os_str().is_empty())
        .any(|entry| same_executable_path(&entry, dir))
}

/// Pure judgement over a daemon PATH value, so the decision is testable
/// without a live daemon.
#[cfg(any(target_os = "linux", test))]
fn evaluate_daemon_path(path: &str, required_dirs: &[PathBuf]) -> DiagItem {
    let missing: Vec<String> = required_dirs
        .iter()
        .filter(|dir| !path_contains_dir(path, dir))
        .map(|dir| dir.display().to_string())
        .collect();

    if missing.is_empty() {
        return DiagItem::ok(
            DAEMON_PATH_CATEGORY,
            format!("daemon process PATH resolves the deployed prx binary: {path}"),
        );
    }

    let missing_list = missing.join(", ");
    let suggested = if path.is_empty() {
        missing.join(":")
    } else {
        format!("{}:{path}", missing.join(":"))
    };
    DiagItem::warn(
        DAEMON_PATH_CATEGORY,
        format!(
            "daemon PATH does not include {missing_list}; shell tool cannot find prx — \
             add Environment=\"PATH={suggested}\" to the unit or re-run `prx service install` \
             (daemon PATH: {path})"
        ),
    )
}

#[cfg(any(target_os = "linux", test))]
fn evaluate_daemon_environ(bytes: &[u8], required_dirs: &[PathBuf]) -> DiagItem {
    daemon_path_from_environ(bytes).map_or_else(
        || {
            DiagItem::warn(
                DAEMON_PATH_CATEGORY,
                "daemon process environment carries no PATH entry; shell tool cannot find prx — \
                 add Environment=\"PATH=…\" to the unit or re-run `prx service install`",
            )
        },
        |path| evaluate_daemon_path(&path, required_dirs),
    )
}

fn daemon_pid_from_state(config: &Config) -> Option<u32> {
    let raw = std::fs::read_to_string(crate::daemon::state_file_path(config)).ok()?;
    let snapshot: serde_json::Value = serde_json::from_str(&raw).ok()?;
    snapshot
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
}

#[cfg(target_os = "linux")]
fn read_daemon_environ(pid: u32) -> std::io::Result<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/environ"))
}

/// Best-effort ownership check for the pid recorded in `daemon_state.json`.
///
/// A pid outlives the daemon that wrote it: after a crash the number is reused
/// by an unrelated process, and reading *that* process' environment would make
/// `prx doctor` judge the daemon PATH from a stranger's `PATH`. Unreadable
/// `/proc` entries (the process is gone, or is owned by another user) answer
/// `false`, which routes the check to its skip branch rather than a verdict.
#[cfg(target_os = "linux")]
fn pid_belongs_to_prx(pid: u32) -> bool {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    process_identity_looks_like_prx(exe.as_deref(), &cmdline)
}

fn check_daemon_process_path(config: &Config, items: &mut Vec<DiagItem>) {
    let Some(pid) = daemon_pid_from_state(config) else {
        items.push(daemon_path_skipped(
            "daemon state exposes no pid; skipped daemon PATH inspection",
        ));
        return;
    };

    #[cfg(target_os = "linux")]
    if pid_belongs_to_prx(pid) {
        let required = required_daemon_path_dirs(daemon_binary_from_state(config).as_deref());
        match read_daemon_environ(pid) {
            Ok(bytes) => items.push(evaluate_daemon_environ(&bytes, &required)),
            Err(error) => items.push(daemon_path_skipped(format!(
                "cannot read /proc/{pid}/environ ({error}); skipped daemon PATH inspection"
            ))),
        }
    } else {
        items.push(daemon_path_skipped(format!(
            "daemon state pid {pid} is not a live prx process (pid reuse or a stopped daemon); \
             skipped reading /proc/{pid}/environ"
        )));
    }

    #[cfg(not(target_os = "linux"))]
    items.push(daemon_path_skipped(format!(
        "daemon PATH inspection is only supported on Linux; skipped for daemon pid {pid}"
    )));
}

// ── Environment checks ───────────────────────────────────────────

fn check_environment(items: &mut Vec<DiagItem>) {
    let cat = "environment";

    // git
    check_command_available("git", &["--version"], cat, items);

    // Shell
    let shell = std::env::var("SHELL").unwrap_or_default();
    if shell.is_empty() {
        items.push(DiagItem::warn(cat, "$SHELL not set"));
    } else {
        items.push(DiagItem::ok(cat, format!("shell: {shell}")));
    }

    // HOME
    if std::env::var("HOME").is_ok() || std::env::var("USERPROFILE").is_ok() {
        items.push(DiagItem::ok(cat, "home directory env set"));
    } else {
        items.push(DiagItem::error(cat, "neither $HOME nor $USERPROFILE is set"));
    }

    // Optional tools
    check_command_available("curl", &["--version"], cat, items);
}

fn check_command_available(cmd: &str, args: &[&str], cat: &'static str, items: &mut Vec<DiagItem>) {
    match std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(output) if output.status.success() => {
            let ver = String::from_utf8_lossy(&output.stdout);
            let first_line = ver.lines().next().unwrap_or("").trim();
            let display = truncate_for_display(first_line, COMMAND_VERSION_PREVIEW_CHARS);
            items.push(DiagItem::ok(cat, format!("{cmd}: {display}")));
        }
        Ok(_) => {
            items.push(DiagItem::warn(cat, format!("{cmd} found but returned non-zero")));
        }
        Err(_) => {
            items.push(DiagItem::warn(cat, format!("{cmd} not found in PATH")));
        }
    }
}

fn format_error_chain(error: &anyhow::Error) -> String {
    let mut parts = Vec::new();
    for cause in error.chain() {
        let message = cause.to_string();
        if !message.is_empty() {
            parts.push(message);
        }
    }

    if parts.is_empty() {
        return String::new();
    }

    parts.join(": ")
}

fn truncate_for_display(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

// ── Helpers ──────────────────────────────────────────────────────

fn parse_rfc3339(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw).ok().map(|dt| dt.with_timezone(&Utc))
}
