//! Read-only projection of the authority issued to a spawned run.

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnDelegationStatus {
    pub requested: bool,
    pub provider: String,
    pub model: String,
    pub remaining_depth: u8,
    pub max_cost_cents: u32,
    pub allowed_tools: Vec<String>,
}

impl SpawnDelegationStatus {
    pub(super) fn from_stored_payload(payload: Option<&str>) -> Option<Self> {
        let payload = payload?;
        // This optional display projection cannot block authoritative recovery
        // of an older launch. Warn without echoing untrusted stored values.
        let Ok(metadata) = serde_json::from_str::<serde_json::Value>(payload) else {
            tracing::warn!("stored spawn metadata is unavailable for delegation status");
            return None;
        };
        let value = metadata.get("delegation").filter(|value| !value.is_null())?;
        let Ok(mut status) = serde_json::from_value::<Self>(value.clone()) else {
            tracing::warn!("stored delegation status is unavailable");
            return None;
        };
        status.provider = crate::providers::redact_endpoint_userinfo(&status.provider).into_owned();
        Some(status)
    }

    pub(super) fn from_grant(grant: &crate::runtime::child_source::IssuedChildGrant) -> Option<Self> {
        let (manifest, _) = grant.scope().gate_inputs();
        let pins = manifest.authority.delegation.as_ref()?;
        let catalog = grant.catalog().view(&Default::default());
        Some(Self {
            requested: pins.requested,
            provider: crate::providers::redact_endpoint_userinfo(&pins.provider).into_owned(),
            model: pins.model.clone(),
            remaining_depth: manifest.authority.remaining_depth,
            max_cost_cents: manifest.authority.max_cost_cents,
            allowed_tools: manifest
                .allowed
                .iter()
                .map(|reference| {
                    // A status projection retains every sealed reference even
                    // if its display entry is unavailable; it grants nothing.
                    catalog
                        .entry_any(&reference.id)
                        .map_or_else(|| reference.id.as_str().to_owned(), |entry| entry.public_name.clone())
                })
                .collect(),
        })
    }
}

pub(super) fn request_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "boolean",
        "default": false,
        "description": "For 'spawn' and 'spawn_batch': explicitly request a strictly narrower, sealed sessions_spawn grant for this child. Requires operator permission; the child and its descendants retain the selected provider/model pins and bounded depth."
    })
}

pub(super) fn requested(
    args: &serde_json::Value,
    config: &crate::config::SessionsSpawnConfig,
    has_parent_grant: bool,
) -> anyhow::Result<bool> {
    let requested = match args.get("allow_delegation") {
        None | Some(serde_json::Value::Bool(false)) => false,
        Some(serde_json::Value::Bool(true)) => true,
        Some(_) => anyhow::bail!("allow_delegation must be a boolean"),
    };
    if requested {
        anyhow::ensure!(
            config.allow_recursive_delegation,
            "recursive child delegation is disabled by operator configuration"
        );
        anyhow::ensure!(
            has_parent_grant,
            "recursive child delegation requires an enabled, admitted parent grant"
        );
        config.validate()?;
    }
    Ok(requested)
}

/// Refuse invalid opt-ins before a batch journals or starts any member. A
/// failed batch's generic result would otherwise hide its per-member reason.
pub(super) fn validate_batch(
    members: &[(String, serde_json::Value)],
    grant: Option<&crate::runtime::child_source::IssuedChildGrant>,
) -> anyhow::Result<()> {
    for (_, args) in members {
        if let Some(grant) = grant {
            requested(args, &grant.generation().effective.sessions_spawn, true)?;
        } else {
            // Without a parent grant recursive work is unavailable, including
            // on ordinary or operator surfaces. Missing/false is unchanged.
            requested(args, &crate::config::SessionsSpawnConfig::default(), false)?;
        }
    }
    Ok(())
}

pub(super) fn append_metadata(
    metadata: &mut serde_json::Value,
    grant: Option<&crate::runtime::child_source::IssuedChildGrant>,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    if let Some(status) = grant.and_then(SpawnDelegationStatus::from_grant) {
        metadata
            .as_object_mut()
            .context("spawn input metadata must be an object")?
            .insert("delegation".to_owned(), serde_json::to_value(status)?);
    }
    Ok(())
}

pub(super) fn append_task(metadata: &mut serde_json::Value, task: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    metadata
        .as_object_mut()
        .context("spawn task metadata must be an object")?
        .insert("task".to_owned(), serde_json::Value::String(task.to_owned()));
    Ok(())
}

pub(super) fn status(
    process: &Option<std::sync::Arc<crate::runtime::child_source::IssuedChildGrant>>,
    task: &Option<std::sync::Arc<crate::runtime::child_source::IssuedChildGrant>>,
) -> Option<SpawnDelegationStatus> {
    process
        .as_deref()
        .or(task.as_deref())
        .and_then(SpawnDelegationStatus::from_grant)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursive_request_requires_operator_permission_and_an_admitted_parent() {
        let mut config = crate::config::SessionsSpawnConfig::default();
        assert!(!requested(&serde_json::json!({}), &config, false).unwrap());
        assert!(!requested(&serde_json::json!({"allow_delegation": false}), &config, false).unwrap());
        assert!(requested(&serde_json::json!({"allow_delegation": "true"}), &config, true).is_err());
        let request = serde_json::json!({"allow_delegation": true});
        assert!(requested(&request, &config, true).is_err());
        config.allow_recursive_delegation = true;
        assert!(requested(&request, &config, false).is_err());
        assert!(requested(&request, &config, true).unwrap());
        config.max_recursive_depth = 0;
        assert!(requested(&request, &config, true).is_err());
    }

    #[test]
    fn stored_status_restores_the_projection_and_redacts_provider_userinfo() {
        let status = SpawnDelegationStatus {
            requested: true,
            provider: ["custom:http://", "prx_user", ":", "fixture-password", "@example.com/v1"].concat(),
            model: "prx-model".to_owned(),
            remaining_depth: 1,
            max_cost_cents: 7,
            allowed_tools: vec!["sessions_spawn".to_owned()],
        };
        let payload = serde_json::json!({"delegation": status}).to_string();
        let restored = SpawnDelegationStatus::from_stored_payload(Some(&payload)).unwrap();
        assert!(restored.requested);
        assert_eq!(restored.remaining_depth, 1);
        assert_eq!(restored.model, "prx-model");
        assert_eq!(restored.allowed_tools, ["sessions_spawn"]);
        assert!(!restored.provider.contains("fixture-password"));
        assert!(!restored.provider.contains("prx_user"));
        assert!(SpawnDelegationStatus::from_stored_payload(None).is_none());
        assert!(SpawnDelegationStatus::from_stored_payload(Some("{}")).is_none());
        assert!(SpawnDelegationStatus::from_stored_payload(Some("{\"delegation\":null}")).is_none());
        assert!(SpawnDelegationStatus::from_stored_payload(Some("legacy non-JSON metadata")).is_none());
        assert!(SpawnDelegationStatus::from_stored_payload(Some("{\"delegation\":true}")).is_none());
    }
}
