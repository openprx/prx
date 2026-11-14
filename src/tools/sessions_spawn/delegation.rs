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
}

pub(super) fn request_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "boolean",
        "default": false,
        "description": "For 'spawn' and 'spawn_batch': explicitly request a strictly narrower, sealed sessions_spawn grant for this child. Requires operator permission; supports one further level with one child per recursive grant, retaining the selected provider/model pins."
    })
}

/// Refuse a recursive delegation opt-in. A recursive grant was issued only
/// by the removed decision plane (P2-G4), so no parent can issue one; a
/// missing or `false` opt-in is unchanged.
pub(super) fn refuse_recursive_request(args: &serde_json::Value) -> anyhow::Result<()> {
    match args.get("allow_delegation") {
        None | Some(serde_json::Value::Bool(false)) => Ok(()),
        Some(serde_json::Value::Bool(true)) => {
            anyhow::bail!(
                "recursive child delegation requires a decision-issued parent grant, which is no longer issued"
            )
        }
        Some(_) => anyhow::bail!("allow_delegation must be a boolean"),
    }
}

/// Refuse invalid opt-ins before a batch journals or starts any member. A
/// failed batch's generic result would otherwise hide its per-member reason.
pub(super) fn validate_batch(members: &[(String, serde_json::Value)]) -> anyhow::Result<()> {
    for (_, args) in members {
        refuse_recursive_request(args)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursive_request_is_refused_without_a_decision_issued_grant() {
        assert!(refuse_recursive_request(&serde_json::json!({})).is_ok());
        assert!(refuse_recursive_request(&serde_json::json!({"allow_delegation": false})).is_ok());
        assert!(refuse_recursive_request(&serde_json::json!({"allow_delegation": "true"})).is_err());
        assert!(refuse_recursive_request(&serde_json::json!({"allow_delegation": true})).is_err());
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
