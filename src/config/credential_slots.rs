//! Write-only addressing of the credential slots.
//!
//! A slot names one encrypted-at-rest path of the secret registry
//! ([`crate::config::secret_paths`]) with its concrete selector filled in.
//! The provider, tool and storage slots keep their selector syntax:
//! `api_key`, `agents.<name>.api_key`, `model_routes.<hint>.api_key`,
//! `embedding_routes.<hint>.api_key`, `reliability.api_keys`, and the tool
//! and storage slots. Every other registered path is addressed by its
//! concrete path: `channels_config.telegram.bot_token`,
//! `mcp.servers.<name>.env.<VAR>`, `nodes.nodes[0].bearer_token`, and a list
//! as a whole (`gateway.paired_tokens`). Setting a slot changes only that
//! value of a [`Config`]; the caller stores it through the structured save,
//! which encrypts it.

use crate::config::Config;
use crate::config::secret_paths::{self, PathElem, REDACTION_MASK, SECRET_PATHS, SecretHandling, Segment};
use std::fmt;

/// Upper bound for one credential value, in bytes.
const MAX_CREDENTIAL_BYTES: usize = 16 * 1024;

/// One addressable credential slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSlot {
    /// `api_key`
    Root,
    /// `agents.<name>.api_key`
    Agent(String),
    /// `model_routes.<hint>.api_key`
    ModelRoute(String),
    /// `embedding_routes.<hint>.api_key`
    EmbeddingRoute(String),
    /// `reliability.api_keys` (the whole list)
    RotationKeys,
    /// `composio.api_key`
    Composio,
    /// `browser.computer_use.api_key`
    BrowserComputerUse,
    /// `web_search.brave_api_key`
    BraveSearch,
    /// `storage.provider.config.db_url`
    StorageDbUrl,
    /// Any other registered path, by its concrete path.
    Registered(RegisteredSlot),
}

/// A registered credential path no fixed slot owns, addressed by its
/// concrete path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredSlot {
    /// The registry pattern the path is an instance of.
    pattern: &'static str,
    /// The concrete path of the value; for a list slot, of the list.
    path: Vec<PathElem>,
    /// Whether the slot holds the whole list of a list pattern.
    list: bool,
}

/// The new content of a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialValue {
    /// A single credential.
    One(String),
    /// The full list of a list slot.
    List(Vec<String>),
    /// Remove the credential.
    Clear,
}

/// Why a slot update was refused. The message never contains a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotError {
    /// The slot does not name a credential slot.
    UnknownSlot,
    /// The slot names an agent, route, channel, MCP server or node that does
    /// not exist.
    MissingTarget,
    /// More than one route carries the hint.
    AmbiguousTarget,
    /// A list was given for a single slot, or the reverse.
    WrongShape,
    /// The value is empty, the mask, ciphertext, too long, or holds control
    /// characters.
    InvalidValue(&'static str),
    /// The credential is required by its section and cannot be cleared.
    Required,
    /// The configuration could not be edited at the slot.
    Unrepresentable,
}

impl fmt::Display for SlotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSlot => write!(
                f,
                "unknown credential slot; expected one of: {}, or the path of another registered credential \
                 (for example channels_config.telegram.bot_token, mcp.servers.<name>.env.<VAR>, \
                 nodes.nodes[0].bearer_token, gateway.paired_tokens)",
                CredentialSlot::PATTERNS.join(", ")
            ),
            Self::MissingTarget => write!(
                f,
                "the slot names an agent, route, channel, MCP server or node that is not configured"
            ),
            Self::AmbiguousTarget => write!(f, "more than one route carries this hint"),
            Self::WrongShape => write!(
                f,
                "a list slot (reliability.api_keys, gateway.paired_tokens) takes a list of strings; every other \
                 slot takes one string (or null to clear)"
            ),
            Self::InvalidValue(reason) => write!(f, "invalid credential value: {reason}"),
            Self::Required => write!(
                f,
                "this credential is required by its section; set a new value, or remove the section"
            ),
            Self::Unrepresentable => write!(f, "the configuration could not be edited at this slot"),
        }
    }
}

impl std::error::Error for SlotError {}

impl CredentialSlot {
    /// The registry patterns the slots address, in slot syntax.
    pub const PATTERNS: [&'static str; 9] = [
        "api_key",
        "agents.<name>.api_key",
        "model_routes.<hint>.api_key",
        "embedding_routes.<hint>.api_key",
        "reliability.api_keys",
        "composio.api_key",
        "browser.computer_use.api_key",
        "web_search.brave_api_key",
        "storage.provider.config.db_url",
    ];

    /// The registry patterns of the fixed slots. Their paths are addressed
    /// only through the fixed syntax, never as a [`RegisteredSlot`].
    const FIXED_REGISTRY_PATTERNS: [&'static str; 9] = [
        "api_key",
        "agents.<name>.api_key",
        "model_routes[].api_key",
        "embedding_routes[].api_key",
        "reliability.api_keys[]",
        "composio.api_key",
        "browser.computer_use.api_key",
        "web_search.brave_api_key",
        "storage.provider.config.db_url",
    ];

    /// Parse a slot name. `None` when it names no credential slot.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let fixed = match raw {
            "api_key" => Some(Self::Root),
            "reliability.api_keys" => Some(Self::RotationKeys),
            "composio.api_key" => Some(Self::Composio),
            "browser.computer_use.api_key" => Some(Self::BrowserComputerUse),
            "web_search.brave_api_key" => Some(Self::BraveSearch),
            "storage.provider.config.db_url" => Some(Self::StorageDbUrl),
            _ => None,
        };
        if fixed.is_some() {
            return fixed;
        }
        let selector = |prefix: &str| {
            raw.strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(".api_key"))
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        };
        selector("agents.")
            .map(Self::Agent)
            .or_else(|| selector("model_routes.").map(Self::ModelRoute))
            .or_else(|| selector("embedding_routes.").map(Self::EmbeddingRoute))
            .or_else(|| RegisteredSlot::parse(raw).map(Self::Registered))
    }

    /// The registry pattern of this slot, as `secret_paths` spells it.
    #[must_use]
    pub const fn registry_pattern(&self) -> &'static str {
        match self {
            Self::Root => "api_key",
            Self::Agent(_) => "agents.<name>.api_key",
            Self::ModelRoute(_) => "model_routes[].api_key",
            Self::EmbeddingRoute(_) => "embedding_routes[].api_key",
            Self::RotationKeys => "reliability.api_keys[]",
            Self::Composio => "composio.api_key",
            Self::BrowserComputerUse => "browser.computer_use.api_key",
            Self::BraveSearch => "web_search.brave_api_key",
            Self::StorageDbUrl => "storage.provider.config.db_url",
            Self::Registered(slot) => slot.pattern,
        }
    }

    /// Whether the slot holds a list.
    #[must_use]
    pub const fn is_list(&self) -> bool {
        match self {
            Self::RotationKeys => true,
            Self::Registered(slot) => slot.list,
            _ => false,
        }
    }

    /// Set, replace or clear this slot in `config`.
    pub fn apply(&self, config: &mut Config, value: CredentialValue) -> Result<(), SlotError> {
        match (self, value) {
            (Self::Registered(slot), value) => slot.apply(config, value),
            (Self::RotationKeys, CredentialValue::List(keys)) => {
                config.reliability.api_keys = keys;
                Ok(())
            }
            (Self::RotationKeys, CredentialValue::Clear) => {
                config.reliability.api_keys = Vec::new();
                Ok(())
            }
            (Self::RotationKeys, CredentialValue::One(_)) | (_, CredentialValue::List(_)) => Err(SlotError::WrongShape),
            (slot, CredentialValue::One(key)) => {
                *slot.single_mut(config)? = Some(key);
                Ok(())
            }
            (slot, CredentialValue::Clear) => {
                *slot.single_mut(config)? = None;
                Ok(())
            }
        }
    }

    fn single_mut<'a>(&self, config: &'a mut Config) -> Result<&'a mut Option<String>, SlotError> {
        match self {
            Self::Root => Ok(&mut config.api_key),
            Self::Agent(name) => config
                .agents
                .get_mut(name)
                .map(|agent| &mut agent.api_key)
                .ok_or(SlotError::MissingTarget),
            Self::ModelRoute(hint) => single_match(config.model_routes.iter_mut().filter(|route| route.hint == *hint))
                .map(|route| &mut route.api_key),
            Self::EmbeddingRoute(hint) => {
                single_match(config.embedding_routes.iter_mut().filter(|route| route.hint == *hint))
                    .map(|route| &mut route.api_key)
            }
            Self::Composio => Ok(&mut config.composio.api_key),
            Self::BrowserComputerUse => Ok(&mut config.browser.computer_use.api_key),
            Self::BraveSearch => Ok(&mut config.web_search.brave_api_key),
            Self::StorageDbUrl => Ok(&mut config.storage.provider.config.db_url),
            Self::RotationKeys | Self::Registered(_) => Err(SlotError::WrongShape),
        }
    }
}

impl fmt::Display for CredentialSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(name) => write!(f, "agents.{name}.api_key"),
            Self::ModelRoute(hint) => write!(f, "model_routes.{hint}.api_key"),
            Self::EmbeddingRoute(hint) => write!(f, "embedding_routes.{hint}.api_key"),
            Self::Root => f.write_str("api_key"),
            Self::RotationKeys => f.write_str("reliability.api_keys"),
            Self::Composio => f.write_str("composio.api_key"),
            Self::BrowserComputerUse => f.write_str("browser.computer_use.api_key"),
            Self::BraveSearch => f.write_str("web_search.brave_api_key"),
            Self::StorageDbUrl => f.write_str("storage.provider.config.db_url"),
            Self::Registered(slot) => f.write_str(&secret_paths::display_path(&slot.path)),
        }
    }
}

impl RegisteredSlot {
    /// Parse a concrete path (`a.b[0].c`) that is an instance of a registered
    /// encrypted-at-rest pattern no fixed slot owns. A list pattern is
    /// addressed as the whole list, never element by element; an alias
    /// spelling is not a slot (the save writes the canonical name).
    fn parse(raw: &str) -> Option<Self> {
        let path = parse_concrete_path(raw)?;
        SECRET_PATHS
            .iter()
            .filter(|secret| {
                secret.handling == SecretHandling::EncryptedAtRest
                    && secret.alias_of.is_none()
                    && !CredentialSlot::FIXED_REGISTRY_PATTERNS.contains(&secret.pattern)
            })
            .find_map(|secret| match secret.segments.split_last() {
                Some((Segment::AnyIndex, list)) => {
                    secret_paths::pattern_matches(list, &path).then_some((secret.pattern, true))
                }
                _ => secret_paths::pattern_matches(secret.segments, &path).then_some((secret.pattern, false)),
            })
            .map(|(pattern, list)| Self { pattern, path, list })
    }

    /// Set, replace or clear the value at the path. The section (or map)
    /// holding it must already be configured; a map key such as an MCP
    /// environment variable is added when it is new.
    fn apply(&self, config: &mut Config, value: CredentialValue) -> Result<(), SlotError> {
        let content = match (self.list, value) {
            (true, CredentialValue::List(items)) => Some(serde_json::Value::Array(
                items.into_iter().map(serde_json::Value::String).collect(),
            )),
            (false, CredentialValue::One(item)) => Some(serde_json::Value::String(item)),
            (_, CredentialValue::Clear) => None,
            (true, CredentialValue::One(_)) | (false, CredentialValue::List(_)) => return Err(SlotError::WrongShape),
        };
        let Some((PathElem::Key(field), parent)) = self.path.split_last() else {
            return Err(SlotError::UnknownSlot);
        };
        let mut tree = serde_json::to_value(&*config).map_err(|_| SlotError::Unrepresentable)?;
        let holder = parent
            .iter()
            .try_fold(&mut tree, |node, elem| match elem {
                PathElem::Key(key) => node.get_mut(key.as_str()),
                PathElem::Index(index) => node.get_mut(*index),
            })
            .and_then(serde_json::Value::as_object_mut)
            .ok_or(SlotError::MissingTarget)?;
        let clearing = content.is_none();
        match content {
            Some(content) => {
                holder.insert(field.clone(), content);
            }
            None => {
                holder.remove(field);
            }
        }
        let mut updated: Config = serde_json::from_value(tree).map_err(|_| {
            if clearing {
                SlotError::Required
            } else {
                SlotError::WrongShape
            }
        })?;
        // Fields the serialized form does not carry.
        updated.config_path.clone_from(&config.config_path);
        updated.workspace_dir.clone_from(&config.workspace_dir);
        updated.agent.compaction.max_context_tokens_explicit = config.agent.compaction.max_context_tokens_explicit;
        updated.memory.embedding_provider_explicit = config.memory.embedding_provider_explicit;
        *config = updated;
        Ok(())
    }
}

/// Parse `a.b[0].c` into path elements. Keys are non-empty and free of
/// whitespace and control characters; an index is a decimal number.
fn parse_concrete_path(raw: &str) -> Option<Vec<PathElem>> {
    let mut path = Vec::new();
    for piece in raw.split('.') {
        let (key, mut indices) = piece.split_once('[').map_or((piece, ""), |(key, rest)| (key, rest));
        if key.is_empty() || key.chars().any(|c| c.is_whitespace() || c.is_control() || c == ']') {
            return None;
        }
        path.push(PathElem::Key(key.to_string()));
        if !indices.is_empty() {
            loop {
                let (index, rest) = indices.split_once(']')?;
                if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                path.push(PathElem::Index(index.parse().ok()?));
                if rest.is_empty() {
                    break;
                }
                indices = rest.strip_prefix('[')?;
            }
        }
    }
    Some(path)
}

fn single_match<T>(mut matches: impl Iterator<Item = T>) -> Result<T, SlotError> {
    let first = matches.next().ok_or(SlotError::MissingTarget)?;
    if matches.next().is_some() {
        return Err(SlotError::AmbiguousTarget);
    }
    Ok(first)
}

/// Validate one plaintext credential. Returns it with surrounding whitespace
/// removed.
pub fn validate_credential(raw: &str) -> Result<String, SlotError> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(SlotError::InvalidValue("empty; send null to clear the slot"));
    }
    if value == REDACTION_MASK {
        return Err(SlotError::InvalidValue("the redaction mask is never stored"));
    }
    if crate::security::SecretStore::is_encrypted(value) {
        return Err(SlotError::InvalidValue("send the plaintext credential, not ciphertext"));
    }
    if value.len() > MAX_CREDENTIAL_BYTES {
        return Err(SlotError::InvalidValue("longer than 16 KiB"));
    }
    if value.chars().any(char::is_control) {
        return Err(SlotError::InvalidValue("contains control characters"));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn every_slot() -> Vec<CredentialSlot> {
        vec![
            CredentialSlot::Root,
            CredentialSlot::Agent("worker".into()),
            CredentialSlot::ModelRoute("fast".into()),
            CredentialSlot::EmbeddingRoute("semantic".into()),
            CredentialSlot::RotationKeys,
            CredentialSlot::Composio,
            CredentialSlot::BrowserComputerUse,
            CredentialSlot::BraveSearch,
            CredentialSlot::StorageDbUrl,
        ]
    }

    /// The fixed slots write exactly the provider, tool and storage paths
    /// they claim, all encrypted at rest. Every other encrypted path is a
    /// registered slot (`every_other_registered_credential_is_a_slot_...`),
    /// so no slot writes a path that would be stored as plaintext and every
    /// encrypted path can be set write-only.
    #[test]
    fn fixed_slots_cover_exactly_their_encrypted_registry_paths() {
        let slots: BTreeSet<&str> = every_slot().iter().map(CredentialSlot::registry_pattern).collect();
        assert_eq!(
            slots,
            CredentialSlot::FIXED_REGISTRY_PATTERNS
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
        assert!(slots.is_subset(&encrypted_patterns()));
        assert_eq!(every_slot().len(), CredentialSlot::PATTERNS.len());
    }

    fn encrypted_patterns() -> BTreeSet<&'static str> {
        SECRET_PATHS
            .iter()
            .filter(|secret| secret.handling == SecretHandling::EncryptedAtRest && secret.alias_of.is_none())
            .map(|secret| secret.pattern)
            .collect()
    }

    #[test]
    fn slot_names_round_trip_through_parse() {
        for slot in every_slot() {
            assert_eq!(CredentialSlot::parse(&slot.to_string()), Some(slot.clone()), "{slot}");
        }
        assert_eq!(
            CredentialSlot::parse("model_routes.a.b.api_key"),
            Some(CredentialSlot::ModelRoute("a.b".into()))
        );
        for unknown in [
            "",
            "gateway.paired_tokens[0]",
            "model_routes..api_key",
            "agents.x.token",
            "api_keys",
        ] {
            assert_eq!(CredentialSlot::parse(unknown), None, "{unknown}");
        }
    }

    #[test]
    fn apply_sets_and_clears_and_refuses_missing_or_ambiguous_targets() {
        let mut config = Config::default();
        let route = |hint: &str| crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: hint.into(),
            provider: "openrouter".into(),
            model: "m".into(),
            api_key: None,
            credential_ref: None,
        };
        config.model_routes = vec![route("fast"), route("dup"), route("dup")];

        let fast = CredentialSlot::ModelRoute("fast".into());
        assert_eq!(fast.apply(&mut config, CredentialValue::One("k".into())), Ok(()));
        assert_eq!(
            config.model_routes.first().and_then(|route| route.api_key.as_deref()),
            Some("k")
        );
        assert_eq!(fast.apply(&mut config, CredentialValue::Clear), Ok(()));
        assert_eq!(
            config.model_routes.first().and_then(|route| route.api_key.as_deref()),
            None
        );

        assert_eq!(
            CredentialSlot::ModelRoute("dup".into()).apply(&mut config, CredentialValue::One("k".into())),
            Err(SlotError::AmbiguousTarget)
        );
        assert_eq!(
            CredentialSlot::Agent("absent".into()).apply(&mut config, CredentialValue::One("k".into())),
            Err(SlotError::MissingTarget)
        );
        assert_eq!(
            CredentialSlot::RotationKeys.apply(&mut config, CredentialValue::One("k".into())),
            Err(SlotError::WrongShape)
        );
        assert_eq!(
            CredentialSlot::Root.apply(&mut config, CredentialValue::List(vec!["k".into()])),
            Err(SlotError::WrongShape)
        );
    }

    #[test]
    fn credential_validation_refuses_masks_ciphertext_and_control_characters() {
        assert_eq!(validate_credential("  sk-live  "), Ok("sk-live".to_string()));
        for bad in ["", "  ", "***", "enc2:00", "enc:00", "a\nb", "a\u{0}b"] {
            assert!(validate_credential(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(validate_credential(&"x".repeat(MAX_CREDENTIAL_BYTES + 1)).is_err());
    }

    /// The slot name of a concrete registered path: the path itself, or for
    /// a list pattern (`gateway.paired_tokens[]`) the list without its index.
    fn slot_name_of(path: &[crate::config::secret_paths::PathElem]) -> String {
        use crate::config::secret_paths::{PathElem, display_path, registered};
        match (
            registered(path).and_then(|secret| secret.segments.last()),
            path.split_last(),
        ) {
            (Some(crate::config::secret_paths::Segment::AnyIndex), Some((PathElem::Index(_), list))) => {
                display_path(list)
            }
            _ => display_path(path),
        }
    }

    /// RED before SECRETS-OTHER: only the provider, tool and storage slots
    /// could be set write-only, so a channel token, a paired token or an MCP
    /// environment value had to go through the raw editor (which refuses
    /// plaintext once these are encrypted). After: every registered path of
    /// the fixture that no fixed slot owns is addressable by its concrete
    /// path, and setting it changes exactly that value.
    #[test]
    fn every_other_registered_credential_is_a_slot_that_sets_its_value() {
        use crate::config::secret_paths::{PathElem, display_path, fixture, registered};
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let fixture_tree: toml::Value = toml::from_str(fixture::TOML).expect("test: fixture parses");
        let sentinels = fixture::string_leaves(&fixture_tree, &|value| value.contains(fixture::SENTINEL_PREFIX));
        let fixed: BTreeSet<&str> = every_slot().iter().map(CredentialSlot::registry_pattern).collect();
        let mut covered = BTreeSet::new();
        for path in &sentinels {
            let secret = registered(path).expect("test: sentinels are registered");
            if fixed.contains(secret.pattern) {
                continue;
            }
            covered.insert(secret.pattern);
            let name = slot_name_of(path);
            let slot = CredentialSlot::parse(&name).unwrap_or_else(|| panic!("{name} must be a credential slot"));
            assert_eq!(slot.to_string(), name, "slot names round trip");
            let list = matches!(path.last(), Some(PathElem::Index(_)));
            assert_eq!(slot.is_list(), list, "{name}");

            let mut config = fixture::config_in(dir.path());
            let value = if list {
                CredentialValue::List(vec!["rotated-a".into(), "rotated-b".into()])
            } else {
                CredentialValue::One("rotated-value".into())
            };
            slot.apply(&mut config, value)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            let tree = toml::Value::try_from(&config).expect("test: render");
            let at = |target: &[PathElem]| {
                target
                    .iter()
                    .try_fold(&tree, |node, elem| match elem {
                        PathElem::Key(key) => node.get(key.as_str()),
                        PathElem::Index(index) => node.get(*index),
                    })
                    .cloned()
            };
            let expected = if list {
                Some(toml::Value::Array(vec!["rotated-a".into(), "rotated-b".into()]))
            } else {
                Some(toml::Value::String("rotated-value".into()))
            };
            let target = if list {
                path.split_last().map_or(path.as_slice(), |(_, list)| list)
            } else {
                path.as_slice()
            };
            assert_eq!(at(target), expected, "{name} was not set");
            // Nothing else changed: every other sentinel is still in place.
            for other in sentinels.iter().filter(|other| !other.starts_with(target)) {
                assert_eq!(
                    at(other).as_ref().and_then(toml::Value::as_str),
                    fixture_tree_value(&fixture_tree, other),
                    "{name} changed {}",
                    display_path(other)
                );
            }
            assert_eq!(config.config_path, dir.path().join("config.toml"));
        }
        let reachable: BTreeSet<&str> = covered.union(&fixed).copied().collect();
        assert_eq!(
            reachable,
            encrypted_patterns(),
            "every encrypted registry path must be settable through a slot"
        );
    }

    fn fixture_tree_value<'a>(
        tree: &'a toml::Value,
        path: &[crate::config::secret_paths::PathElem],
    ) -> Option<&'a str> {
        use crate::config::secret_paths::PathElem;
        path.iter()
            .try_fold(tree, |node, elem| match elem {
                PathElem::Key(key) => node.get(key.as_str()),
                PathElem::Index(index) => node.get(*index),
            })
            .and_then(toml::Value::as_str)
    }

    /// Clearing, adding and refusing through a registered slot.
    #[test]
    fn registered_slots_clear_add_and_refuse_like_the_fixed_slots() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let mut config = crate::config::secret_paths::fixture::config_in(dir.path());
        let slot = |name: &str| CredentialSlot::parse(name).unwrap_or_else(|| panic!("{name} must be a slot"));

        // An optional credential clears to absent.
        assert_eq!(
            slot("webhook.signing_secret").apply(&mut config, CredentialValue::Clear),
            Ok(())
        );
        assert_eq!(config.webhook.signing_secret, None);
        // A new MCP environment variable is added; clearing removes it.
        let var = slot("mcp.servers.local.env.NEW_TOKEN");
        assert_eq!(var.apply(&mut config, CredentialValue::One("v".into())), Ok(()));
        assert_eq!(
            config
                .mcp
                .servers
                .get("local")
                .and_then(|server| server.env.get("NEW_TOKEN"))
                .map(String::as_str),
            Some("v")
        );
        assert_eq!(var.apply(&mut config, CredentialValue::Clear), Ok(()));
        assert!(
            config
                .mcp
                .servers
                .get("local")
                .is_some_and(|server| !server.env.contains_key("NEW_TOKEN"))
        );
        // A list slot clears to empty and refuses a single value.
        assert_eq!(
            slot("gateway.paired_tokens").apply(&mut config, CredentialValue::Clear),
            Ok(())
        );
        assert!(config.gateway.paired_tokens.is_empty(), "expected an empty value");
        assert!(
            slot("gateway.paired_tokens")
                .apply(&mut config, CredentialValue::One("x".into()))
                .is_err()
        );
        // A required credential cannot be cleared; the section stays intact.
        assert!(
            slot("channels_config.telegram.bot_token")
                .apply(&mut config, CredentialValue::Clear)
                .is_err()
        );
        assert_eq!(
            config
                .channels_config
                .telegram
                .as_ref()
                .map(|telegram| telegram.bot_token.as_str()),
            Some("sentinel-telegram")
        );
        // A section or a server that is not configured is a missing target.
        let mut empty = Config::default();
        assert!(
            slot("channels_config.telegram.bot_token")
                .apply(&mut empty, CredentialValue::One("x".into()))
                .is_err()
        );
        let absent = slot("mcp.servers.absent.env.TOKEN").apply(&mut empty, CredentialValue::One("x".into()));
        assert_eq!(absent, Err(SlotError::MissingTarget));
        assert!(
            SlotError::MissingTarget.to_string().contains("MCP server"),
            "R-SECRETS F6: the refusal must not blame an agent or route"
        );
        assert!(
            slot("nodes.nodes[3].bearer_token")
                .apply(&mut config, CredentialValue::One("x".into()))
                .is_err()
        );

        // Not credential slots: a non-secret field, an alias spelling, the
        // generic spelling of a fixed slot, malformed paths.
        for name in [
            "channels_config.telegram.allowed_users",
            "nodes.server.token",
            "model_routes[0].api_key",
            "channels_config..bot_token",
            "nodes.nodes[x].bearer_token",
            "gateway.paired_tokens[0]",
            "mcp.servers.local.env.",
        ] {
            assert_eq!(CredentialSlot::parse(name), None, "{name}");
        }
    }
}
