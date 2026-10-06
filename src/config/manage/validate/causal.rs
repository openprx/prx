//! The decision backend check (SP-CDP-A 5.5 `CausalBackendValidator`;
//! CDP-12).
//!
//! One check, run by every configuration plan (the transaction layer's
//! selection check, rollbacks included) and by every reload of the
//! generation manager, so a hand edit is held to the same rule: an invalid
//! decision backend leaves the running generation active (R-CXR P0-B t2).
//!
//! It runs when the decision plane's section or the operator's model
//! declarations changed, and refuses:
//!
//! - `remote_enabled = true` with no remote backend configured (the
//!   builtin, or `auto` without an endpoint): it would enable nothing, and
//!   saying otherwise would mislead;
//! - a configured backend the strict selection refuses: a model not
//!   declared for `decision_endpoint`, an unpinned hosted model, a hosted
//!   backend without an auth-profile credential, or a credential reference
//!   that is missing or owned by another provider (V2.02). This is the
//!   decision-credential check of the hosted services too (CDP-13): a
//!   TypeSafe backend's `cred-` reference must be owned by
//!   `decision:typesafe`, an OpenRouter backend's by `openrouter`, and the
//!   root or provider-store key is never accepted.
//!
//! Only a change that owns the problem is refused (D-094 C-1): one that
//! touches `[causal_tree]`, or one that makes a backend that passes the
//! check now fail it (a declaration removed, say). A change elsewhere that
//! finds the section already refused, or the credential store unreadable,
//! is accepted with a warning: the decision plane then runs builtin-only,
//! as it does at startup ([`startup_check`]), and never blocks unrelated
//! configuration.
//!
//! It resolves locally: the configuration, its declarations and the
//! credential store's owner of a reference. While the plane is disabled the
//! credential is not resolved: only the rules the configuration alone
//! decides run, so a disabled plane never opens the credential store (D-082
//! N-9); enabling it is a `[causal_tree]` change, checked in full. Nothing
//! is sent anywhere.

use crate::causal_tree::config::BackendSelection;
use crate::config::Config;
use crate::decision::BackendKind;

/// Whether the decision plane of `candidate` differs from `current` in
/// what the check reads.
fn changed(current: Option<&Config>, candidate: &Config) -> bool {
    current.is_none_or(|current| {
        current.causal_tree != candidate.causal_tree
            || super::rendering_changed(&current.providers, &candidate.providers)
    })
}

/// The check of one configuration's decision section: one sentence naming
/// the setting and the reason; never a credential.
fn check_section(config: &Config) -> Result<(), String> {
    let section = &config.causal_tree;
    let backend = &section.backend;
    let nothing_remote = match backend.kind {
        BackendSelection::Fixed(BackendKind::Builtin) => true,
        BackendSelection::Auto => backend.endpoint.trim().is_empty(),
        BackendSelection::Fixed(_) => false,
    };
    if backend.remote_enabled && nothing_remote {
        return Err(
            "causal_tree.backend.remote_enabled is on but no remote decision backend is configured; it would \
             enable nothing: pair the change with SetRemote(false) (causal_tree.backend.remote_enabled = false)"
                .to_string(),
        );
    }
    let Some(selection) = crate::decision::factory::configured_selection(section) else {
        return Ok(());
    };
    let catalog = crate::providers::catalog::CatalogView::from_config(config);
    let checked = if section.enabled {
        crate::providers::selection::check_decision_backend(
            config,
            &catalog,
            &selection.provider,
            backend.model.trim(),
            selection.credential,
        )
        .map(|_| ())
    } else {
        crate::providers::selection::check_decision_model(
            &catalog,
            &selection.provider,
            backend.model.trim(),
            &selection.credential,
        )
    };
    checked.map_err(|error| format!("the decision backend (causal_tree.backend) is refused: {error}"))
}

/// Check `candidate`'s decision backend when the change from `current`
/// touches it (`None`: nothing is stored yet). `Ok(Some(warning))` accepts
/// a change that does not own a refusal (see the module documentation).
///
/// # Errors
///
/// One sentence naming the refused setting and the reason; never a
/// credential.
pub(crate) fn check_decision_plane(current: Option<&Config>, candidate: &Config) -> Result<Option<String>, String> {
    if !changed(current, candidate) {
        return Ok(None);
    }
    let Err(refusal) = check_section(candidate) else {
        return Ok(None);
    };
    let Some(current) = current.filter(|current| current.causal_tree == candidate.causal_tree) else {
        return Err(refusal);
    };
    // An unreadable credential store fails the current configuration too,
    // so it never blocks a change that does not touch the section.
    if check_section(current).is_ok() {
        return Err(format!(
            "{refusal}; this change makes the configured decision backend invalid"
        ));
    }
    Ok(Some(format!(
        "{refusal}; the change does not touch [causal_tree] and is accepted, and the decision plane runs \
         builtin-only until the section is fixed"
    )))
}

/// The decision credentials nothing references, as one warning naming
/// their ids (D-094 C-5); `None` when there are none. A store that cannot
/// be read is said so.
#[must_use]
pub(crate) fn orphaned_credentials_warning(config: &Config) -> Option<String> {
    match crate::config::manage::mutation::causal::orphaned_decision_credentials(config) {
        Ok(orphans) if orphans.is_empty() => None,
        Ok(orphans) => Some(format!(
            "{} decision credential(s) nothing references are left in the credential store ({}): a process \
             stopped between storing one and applying it, or a removal failed; they are never sent, and no \
             rotation removes them: remove each with `prx providers credential remove <id>`",
            orphans.len(),
            orphans
                .iter()
                .map(|status| status.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Err(error) => Some(format!(
            "the decision credentials could not be checked for orphans: {error}"
        )),
    }
}

/// The startup check (D-094 C-1): an enabled decision plane whose backend
/// the check refuses runs builtin-only, with a warning; startup never
/// fails for it. `None` when the plane is disabled or passes.
#[must_use]
pub(crate) fn startup_check(config: &Config) -> Option<String> {
    if !config.causal_tree.enabled {
        return None;
    }
    check_section(config)
        .err()
        .map(|refusal| format!("{refusal}; the decision plane runs builtin-only until the section is fixed"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// `remote_enabled` with no remote backend configured is refused: it
    /// would enable nothing.
    #[test]
    fn remote_enabled_without_a_remote_backend_is_refused() {
        let mut config = Config::default();
        config.causal_tree.backend.remote_enabled = true;
        config.causal_tree.backend.kind = BackendSelection::Fixed(BackendKind::Builtin);
        let refusal = check_decision_plane(None, &config).unwrap_err();
        assert!(refusal.contains("remote_enabled"), "{refusal}");
        config.causal_tree.backend.kind = BackendSelection::Auto;
        assert!(check_decision_plane(None, &config).is_err(), "auto without an endpoint");
        config.causal_tree.backend.remote_enabled = false;
        assert!(check_decision_plane(None, &config).is_ok());
        // Unchanged, it is not checked again.
        let mut current = config;
        current.causal_tree.backend.remote_enabled = true;
        assert!(check_decision_plane(Some(&current), &current).is_ok());
    }

    const LOOPBACK: &str = "http://127.0.0.1:8900/v1";

    fn declaration(provider: &str, model: &str, operations: &[&str]) -> crate::config::schema::ProviderModelConfig {
        serde_json::from_value(serde_json::json!({
            "provider": provider,
            "model": model,
            "operations": operations,
        }))
        .unwrap()
    }

    /// An enabled loopback backend whose model is declared.
    fn loopback_plane() -> Config {
        let mut config = Config::default();
        config.causal_tree.enabled = true;
        config.causal_tree.backend.kind = BackendSelection::Fixed(BackendKind::CompatibleHttp);
        config.causal_tree.backend.endpoint = LOOPBACK.to_string();
        config.causal_tree.backend.model = "decider-1".to_string();
        config.providers.models.push(declaration(
            &format!("custom:{LOOPBACK}"),
            "decider-1",
            &["decision_endpoint"],
        ));
        config
    }

    /// D-094 C-1: a section already refused does not hold back a change
    /// that does not touch `[causal_tree]`; the change is accepted with a
    /// warning naming the refusal.
    #[test]
    fn an_invalid_section_does_not_block_an_unrelated_change() {
        let mut current = Config::default();
        current.causal_tree.backend.remote_enabled = true;
        let mut candidate = current.clone();
        candidate
            .providers
            .models
            .push(declaration("openrouter", "vendor/chat-model", &["chat"]));
        let warning = check_decision_plane(Some(&current), &candidate)
            .expect("an unrelated change is accepted")
            .expect("with a warning");
        assert!(
            warning.contains("remote_enabled") && warning.contains("builtin-only"),
            "{warning}"
        );
        // The same section in a change that touches it is refused.
        let mut touching = candidate.clone();
        touching.causal_tree.policy.max_candidates = 4;
        assert!(check_decision_plane(Some(&current), &touching).is_err());
    }

    /// D-094 C-1: a change outside `[causal_tree]` that makes a valid
    /// backend invalid (its declaration removed) is refused, naming the
    /// decision backend.
    #[test]
    fn a_change_that_invalidates_a_valid_backend_is_refused() {
        let current = loopback_plane();
        assert_eq!(check_decision_plane(None, &current), Ok(None));
        let mut candidate = current.clone();
        candidate.providers.models.clear();
        let refusal = check_decision_plane(Some(&current), &candidate).unwrap_err();
        assert!(
            refusal.contains("causal_tree.backend")
                && refusal.contains("makes the configured decision backend invalid"),
            "{refusal}"
        );
    }

    /// D-094 C-1: an unreadable credential store never blocks a change that
    /// does not touch `[causal_tree]`; a change that does is refused.
    #[test]
    fn an_unreadable_credential_store_never_blocks_unrelated_changes() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut current = loopback_plane();
        current.config_path = temp.path().join("config.toml");
        current.causal_tree.backend.credential_ref =
            crate::causal_tree::config::CredentialRefSetting::Profile("cred-decider".to_string());
        std::fs::write(temp.path().join("auth-profiles.json"), "not json").unwrap();
        let mut candidate = current.clone();
        candidate
            .providers
            .models
            .push(declaration("openrouter", "vendor/chat-model", &["chat"]));
        let warning = check_decision_plane(Some(&current), &candidate)
            .expect("not blocked")
            .expect("with a warning");
        assert!(warning.contains("causal_tree.backend"), "{warning}");
        let mut touching = candidate.clone();
        touching.causal_tree.backend.timeout_ms = 300;
        assert!(check_decision_plane(Some(&current), &touching).is_err());
    }

    /// CDP-13 (V2.02): an enabled hosted backend passes only with its own
    /// service's `cred-` reference: one owned by another provider, a missing
    /// one and the provider's own store are refused, naming the reference,
    /// never the secret.
    #[test]
    fn a_hosted_backend_needs_its_own_services_credential_reference() {
        let temp = tempfile::TempDir::new().unwrap();
        for (kind, provider, model, snapshot) in [
            (BackendKind::TypeSafe, "decision:typesafe", "jev-1.13.0", ""),
            (
                BackendKind::OpenRouter,
                "openrouter",
                "typesafe/jev-1.13",
                "typesafe/jev-1.13-20260917",
            ),
        ] {
            let mut config = Config {
                config_path: temp.path().join("config.toml"),
                ..Config::default()
            };
            config.causal_tree.enabled = true;
            config.causal_tree.backend.kind = BackendSelection::Fixed(kind);
            config.causal_tree.backend.model = model.to_string();
            config.causal_tree.backend.pinned_snapshot = snapshot.to_string();
            config.causal_tree.validate().unwrap();
            config
                .providers
                .models
                .push(declaration(provider, model, &["decision_endpoint"]));
            let references = crate::config::manage::credentials::CredentialReferences::for_config(&config).unwrap();
            let own = references
                .set(
                    provider,
                    &format!("decision-{}", kind.as_str()),
                    "sk-HOSTED-OWN-SENTINEL-01",
                )
                .unwrap();
            let other = references
                .set("openai", "chat-key", "sk-HOSTED-OTHER-SENTINEL-02")
                .unwrap();
            let with = |reference: crate::causal_tree::config::CredentialRefSetting| {
                let mut config = config.clone();
                config.causal_tree.backend.credential_ref = reference;
                check_decision_plane(None, &config)
            };
            use crate::causal_tree::config::CredentialRefSetting as Ref;
            assert_eq!(with(Ref::Profile(own.id.clone())), Ok(None), "{provider}");
            for refused in [
                Ref::Profile(other.id.clone()),
                Ref::Profile("cred-decision-missing".to_string()),
                Ref::Unset,
                Ref::None,
            ] {
                let message = with(refused.clone()).expect_err("refused");
                assert!(message.contains("causal_tree.backend"), "{refused:?}: {message}");
                assert!(!message.contains("SENTINEL"), "{message}");
            }
        }
    }

    /// D-094 C-1: startup reports a refused enabled plane, which then runs
    /// builtin-only, and says nothing of a disabled one.
    #[test]
    fn startup_reports_a_refused_plane_and_never_fails() {
        let mut config = loopback_plane();
        assert_eq!(startup_check(&config), None);
        config.providers.models.clear();
        let warning = startup_check(&config).expect("a refused backend is reported");
        assert!(warning.contains("builtin-only"), "{warning}");
        config.causal_tree.enabled = false;
        assert_eq!(startup_check(&config), None);
    }
}
