//! One explicitly verified local Splash tokenizer profile, never an alias-only
//! assertion or a general tokenizer framework. No inference/server mutation.
use super::context::LessonCompatibility;
use super::record::{digest, refusal, valid_digest};
use crate::skills::lifecycle::{LifecycleError, ModelBinding, SkillLifecycleStore};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const TOKENIZER_SHA: &str = "8e852142d8ab38bb7678f650bd504ba28c29d9c6357e6ed762bebdbd474689eb";
const CONFIG_SHA: &str = "c0435ea80b819a4fb16914a820960ff7c38f9d48cb94e7e8a28780fe01e8dea0";
const TEMPLATE_SHA: &str = "12827f24b742ea4e80cdc12dbcf9622227056b9f797252a3149263d4f9aaadce";
const MODEL: &str = "unsloth/Qwen3.8-27B-GGUF:UD-Q6_K_L";

#[derive(Clone)]
pub(super) struct SupportedProfile {
    pub sha256: String,
    pub boundary_tokens: u64,
    runtime_signature: String,
    files: Vec<(PathBuf, u64, std::time::SystemTime)>,
}

fn read_asset(path: &Path, cap: u64) -> Result<Vec<u8>, LifecycleError> {
    let metadata = fs::metadata(path).map_err(|error| LifecycleError::io(path, error))?;
    if !metadata.is_file() || metadata.len() > cap {
        return Err(refusal("unsupported tokenizer asset size/type"));
    }
    let bytes = fs::read(path).map_err(|error| LifecycleError::io(path, error))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > cap {
        return Err(refusal("tokenizer asset changed beyond its bound"));
    }
    Ok(bytes)
}
fn command_text(program: &str, arguments: &[&str]) -> Result<String, LifecycleError> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .map_err(|_| refusal("actual Splash mapping inspection unavailable"))?;
    if !output.status.success() || output.stdout.len() > 32 * 1024 {
        return Err(refusal("actual Splash mapping inspection failed or exceeded its bound"));
    }
    String::from_utf8(output.stdout).map_err(|_| refusal("Splash mapping is not UTF-8"))
}
fn flag_value<'a>(command: &'a str, flag: &str) -> Option<&'a str> {
    let (_, value) = command.split_once(&format!("{flag} "))?;
    Some(value.split(" --").next()?.trim().trim_matches('"').trim_matches('\''))
}
fn actual_mapping() -> Result<(PathBuf, String), LifecycleError> {
    if !cfg!(target_os = "macos") {
        return Err(refusal(
            "this lesson profile supports only the verified local Splash deployment",
        ));
    }
    let pid = command_text("/usr/sbin/lsof", &["-nP", "-iTCP:18086", "-sTCP:LISTEN", "-t"])?;
    let pids: Vec<_> = pid.split_whitespace().collect();
    let [pid] = pids.as_slice() else {
        return Err(refusal("Splash endpoint does not have one actual process owner"));
    };
    if !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(refusal("invalid Splash process owner"));
    }
    let command = command_text("/bin/ps", &["-p", pid, "-o", "lstart=", "-o", "command="])?;
    if !command.contains("/splash/")
        || !command.contains("-m server.server")
        || flag_value(&command, "--model") != Some(MODEL)
    {
        return Err(refusal("Splash actual model does not match the supported artifact"));
    }
    let tokenizer = PathBuf::from(
        flag_value(&command, "--tokenizer").ok_or_else(|| refusal("Splash has no explicit actual tokenizer path"))?,
    );
    if tokenizer.file_name().and_then(|name| name.to_str()) != Some("tokenizer") {
        return Err(refusal("unsupported Splash tokenizer mapping"));
    }
    let root = tokenizer
        .parent()
        .ok_or_else(|| refusal("Splash tokenizer has no resolved manifest"))?;
    if root.parent().and_then(Path::file_name).and_then(|name| name.to_str()) != Some(".resolved") {
        return Err(refusal("Splash tokenizer is not a resolved model artifact"));
    }
    Ok((root.to_path_buf(), digest(command.as_bytes())))
}

fn alphabet() -> BTreeSet<char> {
    let initial: Vec<u32> = (33..=126).chain(161..=172).chain(174..=255).collect();
    let mut out: BTreeSet<_> = initial.iter().filter_map(|value| char::from_u32(*value)).collect();
    let mut next = 256;
    for byte in 0..=255 {
        if !initial.contains(&byte) {
            if let Some(value) = char::from_u32(next) {
                out.insert(value);
            }
            next += 1;
        }
    }
    out
}

/// Pure shared proof: ASCII normalization is identity, splitting cannot add
/// symbols, BPE only merges a complete byte alphabet, and no empty token exists.
fn certify_byte_bpe(tokenizer: &Value, config: &Value) -> Result<usize, LifecycleError> {
    let normalizer = tokenizer.get("normalizer");
    if normalizer != Some(&serde_json::json!({"type":"NFC"}))
        || config.get("tokenizer_class").and_then(Value::as_str) != Some("TokenizersBackend")
    {
        return Err(refusal("unsupported tokenizer normalization/class"));
    }
    let pre = tokenizer
        .get("pre_tokenizer")
        .and_then(|value| value.get("pretokenizers"))
        .and_then(Value::as_array)
        .ok_or_else(|| refusal("missing tokenizer pre-processing sequence"))?;
    if pre.len() != 2
        || pre.first().and_then(|value| value.get("type")).and_then(Value::as_str) != Some("Split")
        || pre
            .first()
            .and_then(|value| value.get("behavior"))
            .and_then(Value::as_str)
            != Some("Isolated")
        || pre.get(1).and_then(|value| value.get("type")).and_then(Value::as_str) != Some("ByteLevel")
        || pre
            .get(1)
            .and_then(|value| value.get("add_prefix_space"))
            .and_then(Value::as_bool)
            != Some(false)
        || tokenizer
            .get("post_processor")
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str)
            != Some("ByteLevel")
    {
        return Err(refusal("unsupported ASCII-expanding tokenizer processing"));
    }
    let model = tokenizer
        .get("model")
        .ok_or_else(|| refusal("tokenizer model is missing"))?;
    if model.get("type").and_then(Value::as_str) != Some("BPE") {
        return Err(refusal("unsupported tokenizer model"));
    }
    let vocab = model
        .get("vocab")
        .and_then(Value::as_object)
        .ok_or_else(|| refusal("tokenizer vocabulary is missing"))?;
    let alphabet = alphabet();
    if alphabet.len() != 256
        || alphabet.iter().any(|symbol| !vocab.contains_key(&symbol.to_string()))
        || vocab
            .keys()
            .any(|token| token.is_empty() || token.chars().any(|symbol| !alphabet.contains(&symbol)))
        || tokenizer
            .get("added_tokens")
            .and_then(Value::as_array)
            .is_none_or(|tokens| {
                tokens
                    .iter()
                    .any(|token| token.get("content").and_then(Value::as_str).is_none_or(str::is_empty))
            })
    {
        return Err(refusal("tokenizer does not prove a complete nonempty byte alphabet"));
    }
    let max_token_bytes = vocab.keys().map(|token| token.len()).max().unwrap_or(0);
    if max_token_bytes > 256 {
        return Err(refusal("tokenizer boundary exceeds the certified reserve"));
    }
    Ok(max_token_bytes)
}

fn verify_profile() -> Result<SupportedProfile, LifecycleError> {
    let (resolved, runtime_signature) = actual_mapping()?;
    let manifest_path = resolved.join("model.json");
    let manifest_bytes = read_asset(&manifest_path, 16 * 1024)?;
    let manifest: Value =
        serde_json::from_slice(&manifest_bytes).map_err(|_| refusal("invalid actual Splash manifest"))?;
    if manifest.get("model").and_then(Value::as_str) != Some(MODEL)
        || resolved.file_name().and_then(|name| name.to_str()) != Some(digest(&manifest_bytes).as_str())
    {
        return Err(refusal("Splash manifest does not bind its actual resolved model"));
    }
    let mut files = Vec::new();
    let mut asset = |name: &str, expected: &str, cap: u64| -> Result<Vec<u8>, LifecycleError> {
        let path = resolved
            .join(name)
            .canonicalize()
            .map_err(|error| LifecycleError::io(&resolved, error))?;
        let bytes = read_asset(&path, cap)?;
        let item = manifest
            .get("files")
            .and_then(|value| value.get(name))
            .ok_or_else(|| refusal("tokenizer asset is absent from the actual manifest"))?;
        if digest(&bytes) != expected
            || item.get("digest").and_then(Value::as_str) != Some(expected)
            || item.get("bytes").and_then(Value::as_u64) != Some(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
        {
            return Err(refusal("tokenizer asset differs from its pinned manifest/hash"));
        }
        let metadata = fs::metadata(&path).map_err(|error| LifecycleError::io(&path, error))?;
        files.push((
            path,
            metadata.len(),
            metadata
                .modified()
                .map_err(|_| refusal("tokenizer asset lacks stable metadata"))?,
        ));
        Ok(bytes)
    };
    let bytes = asset("tokenizer/tokenizer.json", TOKENIZER_SHA, 16 * 1024 * 1024)?;
    let tokenizer: Value = serde_json::from_slice(&bytes).map_err(|_| refusal("invalid supported tokenizer JSON"))?;
    let config: Value = serde_json::from_slice(&asset("tokenizer/tokenizer_config.json", CONFIG_SHA, 16 * 1024)?)
        .map_err(|_| refusal("invalid supported tokenizer config"))?;
    let _template = asset("tokenizer/chat_template.jinja", TEMPLATE_SHA, 16 * 1024)?;
    let max_token_bytes = certify_byte_bpe(&tokenizer, &config)?;
    // This exact text-only template/profile is certified, not arbitrary Jinja.
    // Reserve 256 for the final merge boundary and 64 for fixed role/BOS/EOS/
    // thinking prefixes. No new message is inserted; total remains under 1536.
    let boundary_tokens = 320;
    let body = serde_json::json!({"profile":"splash-qwen-bytelevel-ascii-v1","manifest":digest(&manifest_bytes),
        "tokenizer":TOKENIZER_SHA,"config":CONFIG_SHA,"template":TEMPLATE_SHA,"alphabet":256,
        "max_token_bytes":max_token_bytes,"boundary_tokens":boundary_tokens});
    files.push((
        manifest_path.clone(),
        u64::try_from(manifest_bytes.len()).unwrap_or(u64::MAX),
        fs::metadata(&manifest_path)
            .and_then(|metadata| metadata.modified())
            .map_err(|_| refusal("manifest lacks stable metadata"))?,
    ));
    Ok(SupportedProfile {
        sha256: digest(&serde_json::to_vec(&body).map_err(|_| refusal("tokenizer profile encoding failed"))?),
        boundary_tokens,
        runtime_signature,
        files,
    })
}

pub(super) fn supported_profile(
    store: &SkillLifecycleStore,
    model: &ModelBinding,
) -> Result<SupportedProfile, LifecycleError> {
    #[cfg(test)]
    if let Some(profile) = fixture::registered(store, model) {
        return Ok(profile);
    }
    #[cfg(not(test))]
    let _ = store;
    static PROFILE: OnceLock<SupportedProfile> = OnceLock::new();
    if model.provider_label != "custom:http://127.0.0.1:18086/v1" || model.model != "qwen38-27b-splash" {
        return Err(refusal("no verified tokenizer profile for the actual model"));
    }
    if PROFILE.get().is_none() {
        let profile = verify_profile()?;
        let _ = PROFILE.set(profile);
    }
    let profile = PROFILE
        .get()
        .ok_or_else(|| refusal("verified tokenizer profile unavailable"))?;
    if actual_mapping()?.1 != profile.runtime_signature
        || profile.files.iter().any(|(path, size, time)| {
            !fs::metadata(path).is_ok_and(|metadata| metadata.len() == *size && metadata.modified().ok() == Some(*time))
        })
    {
        return Err(refusal(
            "actual Splash model/tokenizer mapping changed after profile verification",
        ));
    }
    Ok(profile.clone())
}
impl SkillLifecycleStore {
    pub(in crate::skills::lifecycle) fn lesson_profile_compatibility(
        &self,
        model: &ModelBinding,
        policy_sha256: &str,
        source_profile_sha256: &str,
    ) -> Result<LessonCompatibility, LifecycleError> {
        if !valid_digest(policy_sha256) || !valid_digest(source_profile_sha256) {
            return Err(refusal("invalid lesson policy/source pin"));
        }
        let profile = supported_profile(self, model)?;
        Ok(LessonCompatibility {
            model_sha256: model.digest(),
            policy_sha256: policy_sha256.into(),
            source_profile_sha256: source_profile_sha256.into(),
            tokenizer_profile_sha256: profile.sha256,
        })
    }
}

#[cfg(test)]
mod fixture;
#[cfg(test)]
pub(crate) use fixture::LessonFixtureProfileGuard;
