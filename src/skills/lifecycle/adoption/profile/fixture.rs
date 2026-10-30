//! Hermetic deterministic-transport fixture, compiled only into unit tests.
//! Reuses the production byte-BPE proof. No arbitrary profile/Prepared factory,
//! external model assets, process mapping or production model registry.
use super::{SupportedProfile, alphabet, certify_byte_bpe, digest, refusal};
use crate::skills::lifecycle::{LifecycleError, ModelBinding, SkillLifecycleStore};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

type Key = (PathBuf, String, String);
struct Registration {
    id: String,
    profile: SupportedProfile,
}
static REGISTRY: LazyLock<Mutex<BTreeMap<Key, Registration>>> = LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Owns one exact fixture store/model registration; drop revokes future sends.
/// Existing records and prepared contexts gain no exemption after revocation.
pub(crate) struct LessonFixtureProfileGuard {
    key: Key,
    id: String,
}
impl Drop for LessonFixtureProfileGuard {
    fn drop(&mut self) {
        if let Ok(mut registry) = REGISTRY.lock() {
            if registry.get(&self.key).is_some_and(|entry| entry.id == self.id) {
                registry.remove(&self.key);
            }
        }
    }
}
fn key(store: &SkillLifecycleStore, model: &ModelBinding) -> Key {
    (
        store.root().to_path_buf(),
        store.owner_scope().as_str().to_owned(),
        model.digest(),
    )
}
fn fixture_binding(model: &ModelBinding) -> Result<(), LifecycleError> {
    let endpoint = model
        .provider_label
        .strip_prefix("custom:")
        .ok_or_else(|| refusal("fixture profile requires the actual custom loopback endpoint"))?;
    let url = url::Url::parse(endpoint).map_err(|_| refusal("invalid fixture profile endpoint"))?;
    if model.model != "experiment-loopback"
        || model.temperature_milli != 100
        || url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none_or(|port| port == 0)
        || url.path() != "/v1"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || endpoint != format!("http://127.0.0.1:{}/v1", url.port().unwrap_or(0))
    {
        return Err(refusal(
            "fixture profile is limited to one exact deterministic loopback binding",
        ));
    }
    Ok(())
}
fn certified_fixture(model: &ModelBinding) -> Result<SupportedProfile, LifecycleError> {
    // Exactly 256 nonempty one-byte symbols. There are no BPE merges,
    // expanding normalizers, prefix insertion or postprocessor token inserts.
    let vocab: serde_json::Map<String, serde_json::Value> = alphabet()
        .into_iter()
        .enumerate()
        .map(|(id, symbol)| (symbol.to_string(), serde_json::json!(id)))
        .collect();
    let tokenizer = serde_json::json!({
        "normalizer":{"type":"NFC"},
        "pre_tokenizer":{"type":"Sequence","pretokenizers":[
            {"type":"Split","pattern":{"Regex":"."},"behavior":"Isolated","invert":false},
            {"type":"ByteLevel","add_prefix_space":false,"trim_offsets":false,"use_regex":false}
        ]},
        "post_processor":{"type":"ByteLevel","add_prefix_space":false,"trim_offsets":false},
        "model":{"type":"BPE","vocab":vocab,"merges":[],"byte_fallback":false},
        "added_tokens":[]
    });
    let config = serde_json::json!({"tokenizer_class":"TokenizersBackend"});
    let max_token_bytes = certify_byte_bpe(&tokenizer, &config)?;
    // Fixture HTTP consumes the exact text-only serialized messages without a
    // hidden tokenizer/template. This fixed concatenation adds zero tokens;
    // retain the stricter production 320 boundary/fixed-token allowance.
    let certificate = serde_json::json!({
        "domain":"prx-hermetic-text-only-byte-bpe-profile-v1", "model":model,
        "tokenizer":tokenizer,"config":config,"text_template":"system+user",
        "max_token_bytes":max_token_bytes,"boundary_tokens":320
    });
    Ok(SupportedProfile {
        sha256: digest(&serde_json::to_vec(&certificate).map_err(|_| refusal("fixture certificate encoding failed"))?),
        boundary_tokens: 320,
        runtime_signature: String::new(),
        files: Vec::new(),
    })
}
impl SkillLifecycleStore {
    /// Test-only exact binding, with genuine byte-BPE verification. Callers
    /// still use real publication/adoption/query/runtime/physical HTTP APIs.
    pub(crate) fn register_lesson_fixture_profile(
        &self,
        model: &ModelBinding,
    ) -> Result<LessonFixtureProfileGuard, LifecycleError> {
        fixture_binding(model)?;
        let profile = certified_fixture(model)?;
        let key = key(self, model);
        let id = uuid::Uuid::new_v4().to_string();
        let mut registry = REGISTRY
            .lock()
            .map_err(|_| refusal("fixture profile registry poisoned"))?;
        if registry.contains_key(&key) {
            return Err(refusal("fixture profile already registered for this owner/model"));
        }
        registry.insert(
            key.clone(),
            Registration {
                id: id.clone(),
                profile,
            },
        );
        Ok(LessonFixtureProfileGuard { key, id })
    }
}
pub(super) fn registered(
    store: &SkillLifecycleStore,
    model: &ModelBinding,
) -> Result<Option<SupportedProfile>, LifecycleError> {
    let registry = REGISTRY
        .lock()
        .map_err(|_| refusal("fixture profile registry poisoned"))?;
    Ok(registry.get(&key(store, model)).map(|entry| entry.profile.clone()))
}
