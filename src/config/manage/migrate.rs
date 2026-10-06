//! Migration of legacy provider configuration (SP-PMM §2.7, PMM-H).
//!
//! [`report`] reads the configuration and says, redacted, what is legacy
//! and what `prx providers migrate --apply` would change. It writes nothing:
//!
//! - **Inline keys** of model routes, embedding routes and agents
//!   (`api_key`) become credential references: the value moves into the
//!   encrypted auth-profile store as `cred-<kind>-<name>-<hash>`, owned by
//!   the selection's provider, and the selection names it in
//!   `credential_ref`.
//! - **`[[providers.tool_dialects]]`** rows (the C2-K key, OQ-28) move into
//!   `[[providers.models]]` (`tool_schema` becomes `tool_schema_dialect`,
//!   `tool_call` becomes `tool_call_dialect`). The old key keeps loading for
//!   one release, with a deprecation warning.
//! - **Report only:** the root connection (`api_key`, `api_url`,
//!   `reliability.api_keys`) stays; it is the default provider's implicit
//!   profile, read in memory on every load. Selections the evidence does not
//!   verify (or no longer resolves) are listed. Auth profiles stored under a
//!   catalog alias spelling (`google` for `gemini`), which no provider reads,
//!   are listed with the command that re-stores them; only catalog aliases
//!   of one provider identity are listed, never a regional endpoint or a
//!   bring-your-own URL, and none is moved.
//!
//! [`migrated`] builds the candidate configuration; the management service
//! applies it through the configuration transaction (history, rollback,
//! validation), so an applied migration is undone with `prx config
//! rollback`. Running it again when nothing is legacy changes nothing.

use super::service::{ManageError, redact_provider_text};
use crate::config::schema::{ProviderModelConfig, ProvidersConfig};
use crate::config::{Config, DelegateAgentConfig, EmbeddingRouteConfig, ModelRouteConfig};
use crate::providers::ProviderId;
use crate::providers::catalog::UnknownPolicy;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Which selection an inline key belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineKeyHolder {
    /// `model_routes[..]` with this hint.
    ModelRoute(String),
    /// `embedding_routes[..]` with this hint.
    EmbeddingRoute(String),
    /// `agents.<name>`.
    Agent(String),
}

impl InlineKeyHolder {
    const fn kind(&self) -> &'static str {
        match self {
            Self::ModelRoute(_) => "route",
            Self::EmbeddingRoute(_) => "embedding",
            Self::Agent(_) => "agent",
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::ModelRoute(name) | Self::EmbeddingRoute(name) | Self::Agent(name) => name,
        }
    }

    /// How the validator names the selection.
    fn describe(&self) -> String {
        match self {
            Self::ModelRoute(hint) => format!("model route `hint:{hint}`"),
            Self::EmbeddingRoute(hint) => format!("embedding route `hint:{hint}`"),
            Self::Agent(name) => format!("agent `{name}`"),
        }
    }

    /// The credential name the key at `path` is stored under:
    /// `<kind>-<name>-<hash>`, the name folded to the reference alphabet and
    /// shortened, the hash of the exact kind, name and path keeping any two
    /// holders apart (two embedding routes may share a hint, D-101 F-8).
    fn reference_name(&self, path: &str) -> String {
        let folded = self
            .name()
            .chars()
            .map(|character| {
                let lower = character.to_ascii_lowercase();
                if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
                    lower
                } else {
                    '-'
                }
            })
            .take(24)
            .collect::<String>();
        let folded = folded.trim_matches('-');
        let digest = hex::encode(Sha256::digest(
            format!("{}:{}:{path}", self.kind(), self.name()).as_bytes(),
        ));
        let hash = digest.get(..8).unwrap_or(&digest);
        if folded.is_empty() {
            format!("{}-{hash}", self.kind())
        } else {
            format!("{}-{folded}-{hash}", self.kind())
        }
    }
}

/// One inline key [`migrated`] moves into a credential reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineKey {
    pub holder: InlineKeyHolder,
    /// Its position in its table (routes; `0` for an agent, named by key).
    pub index: usize,
    /// The path of the key (`model_routes[0].api_key`).
    pub path: String,
    /// The selection's provider as configured (compared by identity).
    pub provider: String,
    /// The credential name it moves to (the reference is `cred-<name>`).
    pub reference_name: String,
}

impl InlineKey {
    /// The reference id the key moves to.
    #[must_use]
    pub fn reference_id(&self) -> String {
        format!("{}{}", super::credentials::REFERENCE_PREFIX, self.reference_name)
    }
}

/// An inline key that cannot move: its provider is not one a reference can
/// be issued for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckKey {
    pub path: String,
    pub reason: &'static str,
}

/// An auth profile stored under a catalog alias spelling, which no provider
/// reads (report only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasProfile {
    pub id: String,
    /// The spelling it is stored under.
    pub stored_as: String,
    /// The canonical name the provider reads its own profile under.
    pub read_as: String,
}

/// What is legacy in a configuration, and what applying the migration
/// changes. No credential value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Inline keys that move into credential references.
    pub inline_keys: Vec<InlineKey>,
    /// Inline keys that stay (report only).
    pub stuck_keys: Vec<StuckKey>,
    /// Whether the root connection holds a key (it stays).
    pub root_connection: bool,
    /// How many `[[providers.tool_dialects]]` rows move.
    pub dialect_rows: usize,
    /// Selections accepted as unverified (the resolver's sentences).
    pub unverified: Vec<String>,
    /// Selections that no longer resolve (the resolver's sentences).
    pub refused: Option<String>,
    /// Auth profiles stored under a catalog alias (report only).
    pub alias_profiles: Vec<AliasProfile>,
}

impl MigrationReport {
    /// Whether applying the migration changes nothing.
    #[must_use]
    pub const fn nothing_to_apply(&self) -> bool {
        self.inline_keys.is_empty() && self.dialect_rows == 0
    }

    /// The report as text. Names paths, selections, reference ids and
    /// redacted providers only.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = String::from("Provider configuration migration report\n");
        if self.inline_keys.is_empty() {
            text.push_str("Inline keys: none to move\n");
        } else {
            text.push_str("Inline keys that --apply moves into credential references:\n");
            for key in &self.inline_keys {
                text.push_str(&format!(
                    "  - {} ({}, provider '{}') -> credential_ref = \"{}\"\n",
                    key.path,
                    key.holder.describe(),
                    redact_provider_text(&key.provider),
                    key.reference_id()
                ));
            }
        }
        for key in &self.stuck_keys {
            text.push_str(&format!("  - {} stays inline: {}\n", key.path, key.reason));
        }
        if self.root_connection {
            text.push_str(
                "Root connection (api_key, api_url, reliability.api_keys): stays; it is the default provider's \
                 implicit profile\n",
            );
        }
        if self.dialect_rows == 0 {
            text.push_str("[[providers.tool_dialects]]: nothing to move\n");
        } else {
            text.push_str(&format!(
                "[[providers.tool_dialects]]: --apply moves {} row(s) into [[providers.models]] \
                 (tool_schema -> tool_schema_dialect, tool_call -> tool_call_dialect)\n",
                self.dialect_rows
            ));
        }
        if self.unverified.is_empty() && self.refused.is_none() {
            text.push_str("Persisted selections: all verified\n");
        }
        for warning in &self.unverified {
            text.push_str(&format!("  - unverified: {warning}\n"));
        }
        if let Some(refused) = &self.refused {
            text.push_str(&format!("  - does not resolve: {refused}\n"));
        }
        for profile in &self.alias_profiles {
            text.push_str(&format!(
                "Auth profile {} is stored under the alias '{}', which no provider reads; re-store it with \
                 `prx auth paste-token --provider {} --from-stdin` (report only, nothing is moved)\n",
                profile.id, profile.stored_as, profile.read_as
            ));
        }
        text
    }
}

/// Why a provider cannot own a credential reference, or `None`.
fn reference_owner_refusal(provider: &str) -> Option<&'static str> {
    let provider = provider.trim();
    if matches!(
        crate::providers::redact_endpoint_userinfo(provider),
        std::borrow::Cow::Owned(_)
    ) {
        return Some("its provider URL carries user:password@, which a reference owner must not");
    }
    if !ProviderId::of(provider).is_resolvable() {
        return Some("its provider is not one this build can construct");
    }
    None
}

fn held(key: Option<&String>) -> bool {
    key.is_some_and(|key| !key.trim().is_empty())
}

/// Every inline key of `config`'s routes, embedding routes and agents, in a
/// stable order (routes by position, agents by name).
fn inline_keys(config: &Config) -> (Vec<InlineKey>, Vec<StuckKey>) {
    let mut found = Vec::new();
    for (index, route) in config.model_routes.iter().enumerate() {
        if held(route.api_key.as_ref()) {
            found.push((
                InlineKeyHolder::ModelRoute(route.hint.clone()),
                index,
                format!("model_routes[{index}].api_key"),
                route.provider.clone(),
            ));
        }
    }
    for (index, route) in config.embedding_routes.iter().enumerate() {
        if held(route.api_key.as_ref()) {
            found.push((
                InlineKeyHolder::EmbeddingRoute(route.hint.clone()),
                index,
                format!("embedding_routes[{index}].api_key"),
                route.provider.clone(),
            ));
        }
    }
    let mut agents = config.agents.iter().collect::<Vec<_>>();
    agents.sort_by(|left, right| left.0.cmp(right.0));
    for (name, agent) in agents {
        if held(agent.api_key.as_ref()) {
            found.push((
                InlineKeyHolder::Agent(name.clone()),
                0,
                format!("agents.{name}.api_key"),
                agent.provider.clone(),
            ));
        }
    }
    let mut keys = Vec::new();
    let mut stuck = Vec::new();
    for (holder, index, path, provider) in found {
        if let Some(reason) = reference_owner_refusal(&provider) {
            stuck.push(StuckKey { path, reason });
        } else {
            keys.push(InlineKey {
                reference_name: holder.reference_name(&path),
                holder,
                index,
                path,
                provider,
            });
        }
    }
    (keys, stuck)
}

/// Auth profiles (never a `cred-` reference) stored under a catalog alias
/// of the provider identity the canonical name has: nothing reads them.
fn alias_profiles(config: &Config) -> Result<Vec<AliasProfile>, ManageError> {
    let Some(dir) = config.config_path.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        return Ok(Vec::new());
    };
    let store = crate::auth::profiles::AuthProfilesStore::new(dir, config.secrets.encrypt);
    if !store.path().exists() {
        return Ok(Vec::new());
    }
    let data = store.load_read_only().map_err(|error| {
        ManageError::Invalid(format!(
            "the credential store could not be read ({})",
            crate::providers::sanitize_api_error(&format!("{error:#}"))
        ))
    })?;
    let mut found = data
        .profiles
        .values()
        .filter(|profile| !profile.id.starts_with(super::credentials::REFERENCE_PREFIX))
        .filter_map(|profile| {
            let stored = profile.provider.trim();
            let id = ProviderId::of(stored);
            let canonical = id.catalog_name()?;
            // Catalog aliases of this one identity only: a regional endpoint
            // is another identity, and a BYO URL has no catalog name.
            (stored.to_ascii_lowercase() != canonical && ProviderId::of(canonical) == id).then(|| AliasProfile {
                id: profile.id.clone(),
                stored_as: stored.to_string(),
                read_as: canonical.to_string(),
            })
        })
        .collect::<Vec<_>>();
    found.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(found)
}

/// What is legacy in `config` and what applying the migration would change.
/// Reads the auth-profile store without writing it; writes nothing.
///
/// # Errors
///
/// When the credential store cannot be read.
pub fn report(config: &Config) -> Result<MigrationReport, ManageError> {
    let (inline_keys, stuck_keys) = inline_keys(config);
    let root_connection =
        held(config.api_key.as_ref()) || config.reliability.api_keys.iter().any(|key| !key.trim().is_empty());
    let (unverified, refused) =
        match super::validate::check_affected_selections(None, config, UnknownPolicy::OperatorSelection) {
            Ok(warnings) => (warnings, None),
            Err(refusal) => (Vec::new(), Some(refusal)),
        };
    Ok(MigrationReport {
        inline_keys,
        stuck_keys,
        root_connection,
        dialect_rows: config.providers.tool_dialects.len(),
        unverified,
        refused,
        alias_profiles: alias_profiles(config)?,
    })
}

/// The inline key values the report's keys move, by path. The caller
/// stores each as its reference before the migrated configuration is
/// written.
#[must_use]
pub fn inline_values(config: &Config, report: &MigrationReport) -> HashMap<String, String> {
    // By position, as `migrated` rewrites them: two routes that share a
    // hint keep their own keys (D-101 F-8).
    let value = |key: &InlineKey| -> Option<String> {
        match &key.holder {
            InlineKeyHolder::ModelRoute(_) => config
                .model_routes
                .get(key.index)
                .and_then(|route| route.api_key.clone()),
            InlineKeyHolder::EmbeddingRoute(_) => config
                .embedding_routes
                .get(key.index)
                .and_then(|route| route.api_key.clone()),
            InlineKeyHolder::Agent(name) => config.agents.get(name).and_then(|agent| agent.api_key.clone()),
        }
    };
    report
        .inline_keys
        .iter()
        .filter_map(|key| value(key).map(|value| (key.path.clone(), value.trim().to_string())))
        .collect()
}

/// `config` with the report's inline keys replaced by their references and
/// the `[[providers.tool_dialects]]` rows moved (see [`migrate_tool_dialects`]).
#[must_use]
pub fn migrated(config: &Config, report: &MigrationReport) -> Config {
    let mut next = config.clone();
    let reference_for = |path: &str| {
        report
            .inline_keys
            .iter()
            .find(|key| key.path == path)
            .map(InlineKey::reference_id)
    };
    for (index, route) in next.model_routes.iter_mut().enumerate() {
        if let Some(id) = reference_for(&format!("model_routes[{index}].api_key")) {
            *route = ModelRouteConfig {
                api_key: None,
                credential_ref: Some(id),
                ..route.clone()
            };
        }
    }
    for (index, route) in next.embedding_routes.iter_mut().enumerate() {
        if let Some(id) = reference_for(&format!("embedding_routes[{index}].api_key")) {
            *route = EmbeddingRouteConfig {
                api_key: None,
                credential_ref: Some(id),
                ..route.clone()
            };
        }
    }
    for (name, agent) in &mut next.agents {
        if let Some(id) = reference_for(&format!("agents.{name}.api_key")) {
            *agent = DelegateAgentConfig {
                api_key: None,
                credential_ref: Some(id),
                ..agent.clone()
            };
        }
    }
    migrate_tool_dialects(&mut next.providers);
    next
}

/// Move every `[[providers.tool_dialects]]` row into `[[providers.models]]`
/// (the C2-K shape differs from SP-PMM §2.7, so this is the mechanical
/// migration §2.7 names): `provider` and `model` unchanged, `tool_schema`
/// becomes `tool_schema_dialect`, `tool_call` becomes `tool_call_dialect`.
/// A row joins the `[[providers.models]]` entry with the same provider
/// identity and model (ignoring ASCII case), which cannot already declare
/// the same dialect (the loader refuses that); otherwise it becomes a new
/// entry. Rows are moved, not copied: a dialect declared in both tables is
/// refused at load. Returns how many rows moved.
pub fn migrate_tool_dialects(providers: &mut ProvidersConfig) -> usize {
    let rows = std::mem::take(&mut providers.tool_dialects);
    let moved = rows.len();
    for row in rows {
        let identity = ProviderId::of(&row.provider);
        let model = row.model.as_deref().map(|model| model.trim().to_ascii_lowercase());
        let existing = providers.models.iter_mut().find(|entry| {
            ProviderId::of(&entry.provider) == identity
                && entry.model.as_deref().map(|model| model.trim().to_ascii_lowercase()) == model
        });
        match existing {
            Some(entry) => {
                if row.tool_schema.is_some() {
                    entry.tool_schema_dialect = row.tool_schema;
                }
                if row.tool_call.is_some() {
                    entry.tool_call_dialect = row.tool_call;
                }
            }
            None => providers.models.push(ProviderModelConfig {
                provider: row.provider,
                model: row.model,
                operations: None,
                native_tools: None,
                vision_input: None,
                streaming: None,
                structured_output: None,
                tool_schema_dialect: row.tool_schema,
                tool_call_dialect: row.tool_call,
                context_window: None,
            }),
        }
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::manage::{ConfigActor, ManagementService, RollbackRequest};

    /// Credential-shaped sentinels. Not real secrets.
    const ROUTE_KEY: &str = "pmmh-route-sentinel";
    const EMBEDDING_KEY: &str = "pmmh-embedding-sentinel";
    const AGENT_KEY: &str = "pmmh-agent-sentinel";
    const ROOT_KEY: &str = "pmmh-root-sentinel";
    const ALIAS_TOKEN: &str = "pmmh-alias-sentinel";
    const DIALECT_ENDPOINT: &str = "custom:http://127.0.0.1:9/v1";

    /// A configuration with every legacy slot: inline keys on a route, an
    /// embedding route and an agent, a root key, a C2-K dialect row, and a
    /// default model the evidence does not know.
    async fn legacy(dir: &std::path::Path) -> Config {
        let mut config = Config {
            config_path: dir.join("config.toml"),
            workspace_dir: dir.join("workspace"),
            default_provider: Some("openai".into()),
            default_model: Some("pmmh-unlisted-model".into()),
            api_key: Some(ROOT_KEY.into()),
            ..Config::default()
        };
        config.model_routes.push(ModelRouteConfig {
            hint: "fast".into(),
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
            api_key: Some(ROUTE_KEY.into()),
            credential_ref: None,
        });
        config.embedding_routes.push(EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "openai".into(),
            model: "text-embedding-3-small".into(),
            dimensions: None,
            api_key: Some(EMBEDDING_KEY.into()),
            credential_ref: None,
        });
        config.agents.insert(
            "coder".into(),
            serde_json::from_value(serde_json::json!({
                "provider": "anthropic",
                "model": "claude-sonnet-4",
                "api_key": AGENT_KEY,
            }))
            .expect("test: agent"),
        );
        config.providers.tool_dialects.push(
            toml::from_str(&format!(
                "provider = \"{DIALECT_ENDPOINT}\"\ntool_call = \"xml_function_tags\"\n"
            ))
            .expect("test: dialect row"),
        );
        config.save().await.expect("test: save");
        Config::load_from_path(&config.config_path, config.workspace_dir.clone()).expect("test: load")
    }

    /// Every stored configuration file, by name.
    fn stored_files(dir: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut files = std::collections::BTreeMap::new();
        files.insert(
            "config.toml".to_string(),
            std::fs::read(dir.join("config.toml")).expect("test: config.toml"),
        );
        if let Ok(entries) = std::fs::read_dir(dir.join("config.d")) {
            for entry in entries {
                let path = entry.expect("test: entry").path();
                files.insert(
                    format!(
                        "config.d/{}",
                        path.file_name().and_then(|name| name.to_str()).unwrap_or_default()
                    ),
                    std::fs::read(&path).expect("test: fragment"),
                );
            }
        }
        files
    }

    /// SP-PMM H-1: the dry-run report names every legacy slot, the
    /// unverified default and the alias-stored token, holds no credential,
    /// and leaves every file (configuration and credential store) as it was.
    #[tokio::test]
    async fn the_report_lists_every_legacy_slot_redacted_and_writes_nothing() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = legacy(dir.path()).await;
        crate::auth::profiles::AuthProfilesStore::new(dir.path(), true)
            .upsert_profile(
                crate::auth::profiles::AuthProfile::new_token("google", "default", ALIAS_TOKEN.into()),
                true,
            )
            .expect("test: a token stored under an alias");
        let files = stored_files(dir.path());
        let store = std::fs::read(dir.path().join("auth-profiles.json")).expect("test: store");

        let report = report(&config).expect("test: report");
        let text = report.text();

        for path in [
            "model_routes[0].api_key",
            "embedding_routes[0].api_key",
            "agents.coder.api_key",
        ] {
            assert!(text.contains(path), "{path} missing: {text}");
        }
        assert_eq!(report.inline_keys.len(), 3);
        assert!(text.contains("Root connection"), "{text}");
        assert!(text.contains("moves 1 row(s)"), "{text}");
        assert!(
            text.contains("pmmh-unlisted-model"),
            "the unverified default is listed: {text}"
        );
        assert!(
            text.contains("stored under the alias 'google'") && text.contains("--provider gemini"),
            "{text}"
        );
        for secret in [ROUTE_KEY, EMBEDDING_KEY, AGENT_KEY, ROOT_KEY, ALIAS_TOKEN] {
            assert!(!text.contains(secret), "the report carries {secret}: {text}");
            assert!(!format!("{report:?}").contains(secret));
        }
        assert_eq!(stored_files(dir.path()), files, "the report wrote a configuration file");
        assert_eq!(
            std::fs::read(dir.path().join("auth-profiles.json")).expect("test: store"),
            store,
            "the report rewrote the credential store"
        );
    }

    /// SP-PMM H-2: applying moves every inline key into a credential
    /// reference the selections then use, and every dialect row into
    /// `[[providers.models]]`; applying again changes nothing; the rollback
    /// of the applied record restores every file byte for byte.
    #[tokio::test]
    async fn apply_is_idempotent_and_its_rollback_restores_the_files_byte_for_byte() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = legacy(dir.path()).await;
        let files = stored_files(dir.path());
        let service = ManagementService::new(crate::config::new_shared(config));

        let (report, plan) = service
            .plan_migration()
            .await
            .expect("test: plan")
            .expect("test: something to migrate");
        let revision = plan.revision().map(str::to_string).expect("test: a revision");
        assert!(
            service
                .apply_migration(
                    &report,
                    service
                        .plan_migration()
                        .await
                        .expect("test: plan")
                        .expect("test: plan")
                        .1,
                    ConfigActor::Cli,
                    None
                )
                .await
                .is_err(),
            "a high-risk migration needs its confirmation"
        );
        assert!(
            service
                .credentials()
                .expect("test: references")
                .list()
                .expect("test: list")
                .is_empty(),
            "nothing is stored before the confirmation"
        );
        let outcome = service
            .apply_migration(&report, plan, ConfigActor::Cli, Some(&revision))
            .await
            .expect("test: apply");
        let record = outcome.record_id.expect("test: recorded");

        let migrated =
            Config::load_from_path(&dir.path().join("config.toml"), dir.path().join("workspace")).expect("test: load");
        assert!(migrated.providers.tool_dialects.is_empty());
        assert_eq!(migrated.providers.models.len(), 1);
        assert!(
            migrated
                .model_routes
                .first()
                .is_some_and(|route| route.api_key.is_none())
        );
        assert_eq!(
            migrated
                .model_routes_with_references()
                .first()
                .and_then(|route| route.api_key.as_deref()),
            Some(ROUTE_KEY)
        );
        assert_eq!(
            migrated
                .embedding_routes_with_references()
                .first()
                .and_then(|route| route.api_key.as_deref()),
            Some(EMBEDDING_KEY)
        );
        assert_eq!(
            migrated
                .agents_with_references()
                .get("coder")
                .and_then(|agent| agent.api_key.as_deref()),
            Some(AGENT_KEY)
        );
        assert_eq!(migrated.api_key.as_deref(), Some(ROOT_KEY), "the root connection stays");
        for (name, contents) in stored_files(dir.path()) {
            let text = String::from_utf8_lossy(&contents);
            for secret in [ROUTE_KEY, EMBEDDING_KEY, AGENT_KEY] {
                assert!(!text.contains(secret), "{name} holds {secret} in plaintext");
            }
        }

        // Idempotent: nothing left to migrate, nothing written.
        let after_apply = stored_files(dir.path());
        let again = ManagementService::new(crate::config::new_shared(migrated));
        assert!(again.plan_migration().await.expect("test: plan").is_none());
        assert_eq!(stored_files(dir.path()), after_apply, "a second run wrote something");

        // The rollback brings the inline keys back, byte for byte.
        let current =
            crate::config::files::current_config_revision(&dir.path().join("config.toml")).expect("test: revision");
        again
            .transactions()
            .rollback(RollbackRequest {
                record_id: &record,
                expected_revision: &current,
                confirm: true,
                restore_credentials: true,
                disable_encryption: false,
            })
            .await
            .expect("test: rollback");
        assert_eq!(
            stored_files(dir.path()),
            files,
            "the rollback did not restore every file"
        );
    }

    /// SP-PMM H-3 (C2-K shape differs from §2.7): moving the dialect rows
    /// gives the same runtime options, a row joins a declaration with the
    /// same key, and the moved table loads without the deprecated key.
    #[test]
    fn migrated_dialect_rows_give_the_same_runtime_options() {
        let label = DIALECT_ENDPOINT;
        let mut config = Config::default();
        config.providers.tool_dialects = vec![
            toml::from_str(&format!("provider = \"{label}\"\ntool_call = \"xml_function_tags\"\n")).expect("test: row"),
            toml::from_str(&format!(
                "provider = \"{label}\"\nmodel = \"Fixture-Model\"\ntool_schema = \"no_root_combinators\"\n"
            ))
            .expect("test: row"),
        ];
        config.providers.models = vec![
            toml::from_str(&format!(
                "provider = \"{label}/\"\nmodel = \"fixture-model\"\ncontext_window = 8192\n"
            ))
            .expect("test: declaration"),
        ];
        let before = crate::providers::provider_runtime_options_from_config(&config);

        let mut migrated = config;
        assert_eq!(migrate_tool_dialects(&mut migrated.providers), 2);

        assert!(migrated.providers.tool_dialects.is_empty());
        assert_eq!(
            migrated.providers.models.len(),
            2,
            "one row joined the existing declaration"
        );
        let after = crate::providers::provider_runtime_options_from_config(&migrated);
        // Compared as the runtime compares them: provider identity, model
        // ignoring ASCII case, dialect. A row that joined an existing
        // declaration carries that declaration's spelling.
        let schema = |declarations: &[crate::providers::ToolSchemaDialectDeclaration]| {
            declarations
                .iter()
                .map(|declaration| {
                    (
                        ProviderId::of(&declaration.provider),
                        declaration.model.as_deref().map(str::to_ascii_lowercase),
                        declaration.dialect,
                    )
                })
                .collect::<Vec<_>>()
        };
        let call = |declarations: &[crate::providers::ToolCallDialectDeclaration]| {
            declarations
                .iter()
                .map(|declaration| {
                    (
                        ProviderId::of(&declaration.provider),
                        declaration.model.as_deref().map(str::to_ascii_lowercase),
                        declaration.dialect,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            schema(&before.tool_schema_dialects),
            schema(&after.tool_schema_dialects)
        );
        assert_eq!(call(&before.tool_call_dialects), call(&after.tool_call_dialects));
        assert_eq!(after.tool_schema_dialects.len(), 1);
        assert_eq!(after.tool_call_dialects.len(), 1);
        // Its stored form has no old key (the H-2 test loads a migrated
        // configuration through the real loader).
        let stored = toml::to_string(&migrated.providers).expect("test: render");
        assert!(!stored.contains("tool_dialects"), "{stored}");
    }

    /// Reference names are valid, stable, and keep two holders apart.
    #[test]
    fn reference_names_are_valid_and_distinct() {
        let names = [
            InlineKeyHolder::ModelRoute("fast".into()),
            InlineKeyHolder::ModelRoute("Fast!".into()),
            InlineKeyHolder::Agent("fast".into()),
            InlineKeyHolder::EmbeddingRoute("ümlaut".into()),
            InlineKeyHolder::Agent("a-very-long-agent-name-that-goes-on-and-on".into()),
        ]
        .map(|holder| holder.reference_name("model_routes[0].api_key"));
        for name in &names {
            assert!(
                super::super::credentials::CredentialReferences::id_for(name).is_ok(),
                "{name} is not a valid credential name"
            );
        }
        let unique = names.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), names.len(), "{names:?}");
        assert_eq!(
            Some(&InlineKeyHolder::ModelRoute("fast".into()).reference_name("model_routes[0].api_key")),
            names.first(),
            "names are stable"
        );
        // Two holders that share a hint are told apart by their path.
        assert_ne!(
            InlineKeyHolder::EmbeddingRoute("semantic".into()).reference_name("embedding_routes[0].api_key"),
            InlineKeyHolder::EmbeddingRoute("semantic".into()).reference_name("embedding_routes[1].api_key")
        );
    }
}
