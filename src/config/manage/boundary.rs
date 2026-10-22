//! Write-boundary checks every configuration transaction shares.
//!
//! These rules used to live in the raw file handler only. They run in the
//! plan step now, so every surface, rollback included, enforces them:
//!
//! - a secret path never receives the redaction mask `***`, and a provider
//!   or endpoint URL (or a `cost.prices` key) never receives the redacted
//!   userinfo placeholder `[redacted]@` (the masks `secret_paths::scan`
//!   reports);
//! - with `secrets.encrypt = true`, an encrypted-at-rest slot never receives
//!   plaintext.
//!
//! A raw file write is checked twice: on the merged tree the loader will
//! see, and on the posted file's own tree against that file's current
//! content, so a value in a file the loader skips (an unknown `config.d`
//! file) or in a section another file overrides is refused too.
//!
//! Only what a change *introduces* is refused: a value the stored
//! configuration already holds at the same path (a mask an older release
//! stored, a plaintext key an older release wrote) does not block an
//! unrelated change. `PUT /api/config/secrets` and a full array are the
//! repair paths for those.
//!
//! For a rollback, [`reintroduced_credentials`] lists the secret values the
//! restored configuration holds that the current configuration no longer
//! holds anywhere (a rotated-out key, a removed paired token, URL userinfo
//! that was rotated out).

use crate::config::Config;
use crate::config::secret_paths::{self, PathElem, REDACTION_MASK, SecretHandling};
use std::collections::{BTreeMap, BTreeSet};

/// One string value at a secret path of a configuration tree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SecretLeaf {
    value: String,
    encrypted_slot: bool,
}

/// Every string leaf at or under a secret path, keyed by its display path.
fn secret_leaves<T: secret_paths::SecretTree>(root: &T) -> BTreeMap<String, SecretLeaf> {
    fn walk<T: secret_paths::SecretTree>(
        node: &T,
        path: &mut Vec<PathElem>,
        inside_secret: bool,
        out: &mut BTreeMap<String, SecretLeaf>,
    ) {
        let secret = inside_secret || (!path.is_empty() && secret_paths::is_secret_path(path));
        if let Some(value) = node.as_secret_str() {
            if secret {
                let encrypted_slot = secret_paths::registered(path)
                    .is_some_and(|registered| registered.handling == SecretHandling::EncryptedAtRest);
                out.insert(
                    secret_paths::display_path(path),
                    SecretLeaf {
                        value: value.to_string(),
                        encrypted_slot,
                    },
                );
            }
            return;
        }
        node.for_each_child(&mut |elem, child| {
            path.push(elem);
            walk(child, path, secret, out);
            path.pop();
        });
    }
    let mut out = BTreeMap::new();
    walk(root, &mut Vec::new(), false, &mut out);
    out
}

/// Refuse a stored candidate tree that introduces the mask at a secret path,
/// or (with `encrypt`) plaintext at an encrypted-at-rest slot, compared with
/// the stored tree currently on disk. The message names paths, never values.
pub(crate) fn check_stored_candidate(
    candidate: &toml::Value,
    current: Option<&toml::Value>,
    encrypt: bool,
) -> Result<(), String> {
    let current_masks = current
        .map(|current| secret_paths::scan(current).masked)
        .unwrap_or_default()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let masked = secret_paths::scan(candidate)
        .masked
        .into_iter()
        .filter(|path| !current_masks.contains(path))
        .collect::<Vec<_>>();
    let current = current.map(secret_leaves).unwrap_or_default();
    let unchanged = |path: &str, leaf: &SecretLeaf| current.get(path).is_some_and(|stored| stored.value == leaf.value);
    let mut plaintext = Vec::new();
    for (path, leaf) in secret_leaves(candidate) {
        if unchanged(&path, &leaf) || leaf.value == REDACTION_MASK {
            continue;
        }
        if encrypt
            && leaf.encrypted_slot
            && !leaf.value.is_empty()
            && !crate::security::SecretStore::is_encrypted(&leaf.value)
        {
            plaintext.push(path);
        }
    }
    if !masked.is_empty() {
        return Err(format!(
            "The change holds a redaction mask (`{REDACTION_MASK}`, or `[redacted]` in a URL) at: {}. A masked value is never \
             stored; secrets are write-only: set a credential through PUT /api/config/secrets, or send the real \
             value.",
            masked.join(", ")
        ));
    }
    if !plaintext.is_empty() {
        return Err(format!(
            "Refusing to write plaintext credentials at: {}. With secrets.encrypt = true these slots hold enc2: \
             ciphertext; keep the stored value or set the credential through PUT /api/config/secrets.",
            plaintext.join(", ")
        ));
    }
    Ok(())
}

/// A raw replacement must retain populated secret slots unless the caller
/// explicitly permits clearing them. Inspect the file itself, including
/// secrets hidden by another fragment or held in an unmanaged file.
pub(crate) fn check_raw_secret_removal(
    candidate: &toml::Value,
    current: Option<&toml::Value>,
    allow_secret_clear: bool,
) -> Result<(), String> {
    // Clearing permission never makes a redaction mask a stored credential
    // or a "keep" instruction. Encryption checks still run after staging.
    check_stored_candidate(candidate, current, false)?;
    if allow_secret_clear {
        return Ok(());
    }
    let Some(current) = current else {
        return Ok(());
    };
    let current = serde_json::to_value(current).map_err(|_| "Cannot inspect stored secret paths".to_string())?;
    let candidate = serde_json::to_value(candidate).map_err(|_| "Cannot inspect posted secret paths".to_string())?;
    let mut dropped = super::plan::omitted_secret_paths(&current, &candidate)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let after_urls = userinfo_urls(&candidate);
    for (path, url) in userinfo_urls(&current) {
        if !after_urls.contains_key(&path) && !after_urls.values().any(|value| value == &url) {
            dropped.insert(path);
        }
    }
    if dropped.is_empty() {
        return Ok(());
    }
    Err(format!(
        "Raw file replacement would drop stored secrets at: {}. Keep the stored values or use PUT /api/config/secrets \
         to change or clear credentials; for an intentional raw-file removal, use the CLI --allow-credential-clear flag. \
         POST /api/config/plans and POST /api/config/apply do not accept a clear flag. Nothing was written.",
        dropped.into_iter().collect::<Vec<_>>().join(", ")
    ))
}

/// Secret paths of `candidate` whose value the current configuration holds
/// at no secret path at all. `None` for `current` (nothing loadable) lists
/// every secret of the candidate.
pub(crate) fn reintroduced_credentials(current: Option<&Config>, candidate: &Config) -> Vec<String> {
    let Ok(candidate) = serde_json::to_value(candidate) else {
        tracing::warn!("Failed to render the restored configuration; treating every credential as re-introduced");
        return vec!["*".to_string()];
    };
    let current_values = current
        .and_then(|current| match serde_json::to_value(current) {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(error = %error, "Failed to render the current configuration for a rollback check");
                None
            }
        })
        .map(|current| {
            secret_leaves(&current)
                .into_values()
                .map(|leaf| leaf.value)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let current_urls = current
        .and_then(|current| serde_json::to_value(current).ok())
        .map(|current| userinfo_urls(&current).into_values().collect::<BTreeSet<_>>())
        .unwrap_or_default();
    let mut reintroduced = secret_leaves(&candidate)
        .into_iter()
        .filter(|(_, leaf)| !leaf.value.is_empty() && leaf.value != REDACTION_MASK)
        .filter(|(_, leaf)| !current_values.contains(&leaf.value))
        .map(|(path, _)| path)
        .collect::<Vec<_>>();
    // URL userinfo (`user:password@` in a provider or endpoint URL, or in a
    // `cost.prices` key) is a credential too (PMM-D-8): a URL carrying
    // userinfo that no URL of the current configuration carries is one the
    // current configuration no longer holds.
    reintroduced.extend(
        userinfo_urls(&candidate)
            .into_iter()
            .filter(|(_, url)| !current_urls.contains(url))
            .map(|(path, _)| path),
    );
    reintroduced
}

/// Every URL that carries userinfo, at a URL-registry path or as a key of a
/// userinfo key table, keyed by its display path (userinfo redacted).
fn userinfo_urls(root: &serde_json::Value) -> BTreeMap<String, String> {
    fn carries_userinfo(url: &str) -> bool {
        matches!(
            crate::providers::redact_endpoint_userinfo(url),
            std::borrow::Cow::Owned(_)
        )
    }
    fn walk(
        node: &serde_json::Value,
        raw: &mut Vec<PathElem>,
        display: &mut Vec<PathElem>,
        out: &mut BTreeMap<String, String>,
    ) {
        if secret_paths::is_url_userinfo_path(raw) {
            if let Some(url) = node.as_str().filter(|url| carries_userinfo(url)) {
                out.insert(secret_paths::display_path(display), url.to_string());
            }
            return;
        }
        let key_table = secret_paths::is_url_userinfo_key_table(raw);
        match node {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    let shown = crate::security::redaction::redact_url_value(key).into_owned();
                    if key_table && carries_userinfo(key) {
                        let mut path = display.clone();
                        path.push(PathElem::Key(shown.clone()));
                        out.insert(secret_paths::display_path(&path), key.clone());
                    }
                    raw.push(PathElem::Key(key.clone()));
                    display.push(PathElem::Key(shown));
                    walk(child, raw, display, out);
                    raw.pop();
                    display.pop();
                }
            }
            serde_json::Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    raw.push(PathElem::Index(index));
                    display.push(PathElem::Index(index));
                    walk(child, raw, display, out);
                    raw.pop();
                    display.pop();
                }
            }
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut Vec::new(), &mut Vec::new(), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(text: &str) -> toml::Value {
        toml::Value::Table(toml::from_str(text).expect("test: toml"))
    }

    #[test]
    fn raw_replacement_requires_permission_to_drop_secret_slots() {
        let current = tree(
            "[web_search]\nbrave_api_key = 'prx-brave-fixture'\n\
             [gateway]\npaired_tokens = ['prx-token-a', 'prx-token-b']\n\
             [future_section]\nclient_secret = 'prx-future-fixture'\n\
             [proxy]\nhttp_proxy = 'http://prx_user:prx-password@example.com'\n",
        );
        let posted = tree("[web_search]\nbrave_api_key = ''\n[gateway]\npaired_tokens = ['prx-token-a']\n");
        let error = check_raw_secret_removal(&posted, Some(&current), false).unwrap_err();
        for path in [
            "web_search.brave_api_key",
            "gateway.paired_tokens[1]",
            "future_section.client_secret",
            "proxy.http_proxy",
        ] {
            assert!(error.contains(path), "{error}");
        }
        assert!(error.contains("PUT /api/config/secrets"), "{error}");
        assert!(error.contains("--allow-credential-clear"), "{error}");
        assert!(
            error.contains("POST /api/config/plans and POST /api/config/apply do not accept a clear flag"),
            "{error}"
        );
        for (_, value) in secret_paths::scan(&current).values_with_paths() {
            assert!(!error.contains(value), "{error}");
        }
        assert!(check_raw_secret_removal(&posted, Some(&current), true).is_ok());
        assert!(check_raw_secret_removal(&current, Some(&current), false).is_ok());
        assert!(check_raw_secret_removal(&tree(""), None, false).is_ok());
    }

    #[test]
    fn raw_secret_array_rotation_preserves_populated_slots() {
        let current = tree("[gateway]\npaired_tokens = ['prx-token-a', 'prx-token-b']\n");
        for tokens in [
            "['prx-token-a', 'prx-token-c']",
            "['prx-token-b', 'prx-token-a']",
            "['prx-token-c', 'prx-token-c']",
            "['prx-token-c', 'prx-token-d', 'prx-token-e']",
        ] {
            let posted = tree(&format!("[gateway]\npaired_tokens = {tokens}\n"));
            assert!(check_raw_secret_removal(&posted, Some(&current), false).is_ok());
        }
        for tokens in ["['prx-token-c']", "['', 'prx-token-c']", "[]"] {
            let posted = tree(&format!("[gateway]\npaired_tokens = {tokens}\n"));
            let error = check_raw_secret_removal(&posted, Some(&current), false).unwrap_err();
            assert!(error.contains("gateway.paired_tokens[1]"), "{error}");
            assert!(check_raw_secret_removal(&posted, Some(&current), true).is_ok());
        }
        let duplicate = tree("[gateway]\npaired_tokens = ['prx-token-a', 'prx-token-a']\n");
        let shrunk = tree("[gateway]\npaired_tokens = ['prx-token-a']\n");
        assert!(check_raw_secret_removal(&shrunk, Some(&duplicate), false).is_err());
    }

    #[test]
    fn raw_secret_removal_guidance_is_not_scrubbed_by_substring() {
        let current = tree("api_key = 'config'\n");
        let error = check_raw_secret_removal(&tree(""), Some(&current), false).unwrap_err();
        assert!(error.contains("api_key"), "{error}");
        assert!(error.contains("PUT /api/config/secrets"), "{error}");
        assert!(error.contains("--allow-credential-clear"), "{error}");
    }

    #[test]
    fn raw_secret_clear_permission_never_accepts_a_mask() {
        let current = tree("[web_search]\nbrave_api_key = 'prx-brave-fixture'\n");
        let posted = tree("[web_search]\nbrave_api_key = '***'\n");
        for allow in [false, true] {
            let error = check_raw_secret_removal(&posted, Some(&current), allow).unwrap_err();
            assert!(error.contains("redaction mask"), "{error}");
            assert!(error.contains("web_search.brave_api_key"), "{error}");
        }
    }

    #[test]
    fn raw_secret_rotation_is_not_an_omission() {
        let current = tree("[web_search]\nbrave_api_key = 'prx-old-fixture'\n");
        let posted = tree("[web_search]\nbrave_api_key = 'prx-new-fixture'\n");
        assert!(check_raw_secret_removal(&posted, Some(&current), false).is_ok());
    }

    #[test]
    fn raw_route_reordering_keeps_secrets_but_deleting_a_keyed_route_does_not() {
        let current = tree(
            "[[model_routes]]\nhint = 'unkeyed'\n[[model_routes]]\nhint = 'keyed'\napi_key = 'prx-route-fixture'\n",
        );
        let shifted = tree("[[model_routes]]\nhint = 'keyed'\napi_key = 'prx-route-fixture'\n");
        assert!(check_raw_secret_removal(&shifted, Some(&current), false).is_ok());
        let removed = tree("[[model_routes]]\nhint = 'unkeyed'\napi_key = 'prx-route-fixture'\n");
        let error = check_raw_secret_removal(&removed, Some(&current), false).unwrap_err();
        assert!(error.contains("model_routes[1].api_key"), "{error}");
    }

    #[test]
    fn an_introduced_mask_is_refused_by_path_only() {
        let current = tree("api_key = \"enc2:aa\"\n");
        let error = check_stored_candidate(&tree("api_key = \"***\"\n"), Some(&current), true).expect_err("mask");
        assert!(error.contains("api_key"), "{error}");
    }

    #[test]
    fn a_mask_already_stored_does_not_block_an_unrelated_change() {
        let current = tree("[gateway]\npaired_tokens = [\"***\"]\n");
        let candidate = tree("default_temperature = 0.5\n[gateway]\npaired_tokens = [\"***\"]\n");
        assert!(check_stored_candidate(&candidate, Some(&current), true).is_ok());
    }

    #[test]
    fn introduced_plaintext_at_an_encrypted_slot_is_refused_only_with_encryption_on() {
        let candidate =
            tree("[[model_routes]]\nhint = \"h\"\nprovider = \"p\"\nmodel = \"m\"\napi_key = \"plain-sentinel\"\n");
        let error = check_stored_candidate(&candidate, None, true).expect_err("plaintext");
        assert!(error.contains("model_routes[0].api_key"), "{error}");
        assert!(!error.contains("plain-sentinel"), "{error}");
        assert!(check_stored_candidate(&candidate, None, false).is_ok());
    }

    /// A credential-named key that is not registered (a section a later
    /// release adds) is masked on read by the name rule, but it is not an
    /// encrypted slot, so plaintext there is not refused.
    #[test]
    fn plaintext_at_an_unregistered_credential_name_is_not_an_encryption_violation() {
        let candidate = tree("[future_section]\nclient_secret = \"plain-token\"\n");
        assert!(check_stored_candidate(&candidate, None, true).is_ok());
    }

    #[test]
    fn a_credential_the_current_config_no_longer_holds_is_reintroduced() {
        let mut current = Config::default();
        current.api_key = Some("key-now".into());
        let mut restored = Config::default();
        restored.api_key = Some("key-rotated-out".into());
        assert_eq!(reintroduced_credentials(Some(&current), &restored), vec!["api_key"]);
        restored.api_key = Some("key-now".into());
        assert!(
            reintroduced_credentials(Some(&current), &restored).is_empty(),
            "expected an empty value"
        );
    }

    /// RED before SECRETS-OTHER: a redacted proxy URL written back was stored
    /// as the proxy, credential lost.
    #[test]
    fn a_redacted_endpoint_url_is_refused_as_a_mask() {
        let current = tree("[proxy]\nhttp_proxy = \"http://u:pw@127.0.0.1:3128\"\n");
        let candidate = tree("[proxy]\nhttp_proxy = \"http://[redacted]@127.0.0.1:3129\"\n");
        let error = check_stored_candidate(&candidate, Some(&current), true).expect_err("mask");
        assert!(error.contains("proxy.http_proxy"), "{error}");
    }

    /// RED before SECRETS-OTHER: with encryption on, a raw write could store
    /// a channel token, a paired token or an MCP environment value as
    /// plaintext. After: refused by path, like a provider key.
    #[test]
    fn introduced_plaintext_channel_and_gateway_secrets_are_refused_with_encryption_on() {
        let candidate = tree(
            "[channels_config.telegram]\nbot_token = \"plain-telegram\"\nallowed_users = []\n\
             [gateway]\npaired_tokens = [\"plain-paired\"]\n\
             [mcp.servers.local]\ncommand = \"c\"\nenv = { GITHUB_PAT = \"plain-mcp\" }\n",
        );
        let error = check_stored_candidate(&candidate, None, true).expect_err("plaintext");
        for path in [
            "channels_config.telegram.bot_token",
            "gateway.paired_tokens[0]",
            "mcp.servers.local.env.GITHUB_PAT",
        ] {
            assert!(error.contains(path), "{path}: {error}");
        }
        assert!(!error.contains("plain-"), "{error}");
        assert!(check_stored_candidate(&candidate, None, false).is_ok());
    }

    /// PMM-D-8: URL userinfo the current configuration no longer carries is
    /// re-introduced; the display path never carries it.
    #[test]
    fn rotated_url_userinfo_is_reintroduced_and_never_shown() {
        let mut current = Config::default();
        current.default_provider = Some("custom:http://alice:pmmd8-new@127.0.0.1:9/v1".into());
        let mut restored = Config::default();
        restored.default_provider = Some("custom:http://alice:pmmd8-old@127.0.0.1:9/v1".into());
        restored.cost.prices.insert(
            "custom:http://bob:pmmd8-price@127.0.0.1:9/v1/model-a".into(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );

        let reintroduced = reintroduced_credentials(Some(&current), &restored);

        assert!(
            reintroduced.contains(&"default_provider".to_string()),
            "{reintroduced:?}"
        );
        assert!(
            reintroduced
                .iter()
                .any(|path| path.starts_with("cost.prices.custom:http://[redacted]@")),
            "{reintroduced:?}"
        );
        assert!(!format!("{reintroduced:?}").contains("pmmd8-"), "{reintroduced:?}");
        assert!(
            reintroduced_credentials(Some(&restored), &restored).is_empty(),
            "expected an empty value"
        );
    }
}
