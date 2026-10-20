//! Provider name aliases, regional base URLs and endpoint helpers.

pub(super) const MINIMAX_INTL_BASE_URL: &str = "https://api.minimax.io/v1";
pub(super) const MINIMAX_CN_BASE_URL: &str = "https://api.minimaxi.com/v1";
pub(super) const GLM_GLOBAL_BASE_URL: &str = "https://api.z.ai/api/paas/v4";
pub(super) const GLM_CN_BASE_URL: &str = "https://open.bigmodel.cn/api/paas/v4";
pub(super) const MOONSHOT_INTL_BASE_URL: &str = "https://api.moonshot.ai/v1";
pub(super) const MOONSHOT_CN_BASE_URL: &str = "https://api.moonshot.cn/v1";
pub(super) const QWEN_CN_BASE_URL: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";
pub(super) const QWEN_INTL_BASE_URL: &str = "https://dashscope-intl.aliyuncs.com/compatible-mode/v1";
pub(super) const QWEN_US_BASE_URL: &str = "https://dashscope-us.aliyuncs.com/compatible-mode/v1";
pub(super) const QWEN_CODING_CN_BASE_URL: &str = "https://coding.dashscope.aliyuncs.com/v1";
pub(super) const QWEN_CODING_INTL_BASE_URL: &str = "https://coding-intl.dashscope.aliyuncs.com/v1";
pub(super) const ZAI_GLOBAL_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";
pub(super) const ZAI_CN_BASE_URL: &str = "https://open.bigmodel.cn/api/coding/paas/v4";

pub(crate) fn is_minimax_intl_alias(name: &str) -> bool {
    matches!(
        name,
        "minimax"
            | "minimax-intl"
            | "minimax-io"
            | "minimax-global"
            | "minimax-oauth"
            | "minimax-portal"
            | "minimax-oauth-global"
            | "minimax-portal-global"
    )
}

pub(crate) fn is_minimax_cn_alias(name: &str) -> bool {
    matches!(
        name,
        "minimax-cn" | "minimaxi" | "minimax-oauth-cn" | "minimax-portal-cn"
    )
}

pub(crate) fn is_minimax_alias(name: &str) -> bool {
    is_minimax_intl_alias(name) || is_minimax_cn_alias(name)
}

pub(crate) fn is_glm_global_alias(name: &str) -> bool {
    matches!(name, "glm" | "zhipu" | "glm-global" | "zhipu-global")
}

pub(crate) fn is_glm_cn_alias(name: &str) -> bool {
    matches!(name, "glm-cn" | "zhipu-cn" | "bigmodel")
}

pub(crate) fn is_glm_alias(name: &str) -> bool {
    is_glm_global_alias(name) || is_glm_cn_alias(name)
}

pub(crate) fn is_moonshot_intl_alias(name: &str) -> bool {
    matches!(name, "moonshot-intl" | "moonshot-global" | "kimi-intl" | "kimi-global")
}

pub(crate) fn is_moonshot_cn_alias(name: &str) -> bool {
    matches!(name, "moonshot" | "kimi" | "moonshot-cn" | "kimi-cn")
}

pub(crate) fn is_moonshot_alias(name: &str) -> bool {
    is_moonshot_intl_alias(name) || is_moonshot_cn_alias(name)
}

pub(crate) fn is_qwen_cn_alias(name: &str) -> bool {
    matches!(name, "qwen" | "dashscope" | "qwen-cn" | "dashscope-cn")
}

pub(crate) fn is_qwen_intl_alias(name: &str) -> bool {
    matches!(
        name,
        "qwen-intl" | "dashscope-intl" | "qwen-international" | "dashscope-international"
    )
}

pub(crate) fn is_qwen_us_alias(name: &str) -> bool {
    matches!(name, "qwen-us" | "dashscope-us")
}

pub(crate) fn is_qwen_oauth_alias(name: &str) -> bool {
    matches!(name, "qwen-code" | "qwen-oauth" | "qwen_oauth")
}

pub(crate) fn is_qwen_coding_alias(name: &str) -> bool {
    matches!(
        name,
        "dashscope-coding" | "qwen-coding" | "dashscope-coding-intl" | "qwen-coding-intl"
    )
}

pub(crate) fn is_qwen_alias(name: &str) -> bool {
    is_qwen_cn_alias(name)
        || is_qwen_intl_alias(name)
        || is_qwen_us_alias(name)
        || is_qwen_coding_alias(name)
        || is_qwen_oauth_alias(name)
}

pub(crate) fn is_zai_global_alias(name: &str) -> bool {
    matches!(name, "zai" | "z.ai" | "zai-global" | "z.ai-global")
}

pub(crate) fn is_zai_cn_alias(name: &str) -> bool {
    matches!(name, "zai-cn" | "z.ai-cn")
}

pub(crate) fn is_zai_alias(name: &str) -> bool {
    is_zai_global_alias(name) || is_zai_cn_alias(name)
}

pub(crate) fn is_qianfan_alias(name: &str) -> bool {
    matches!(name, "qianfan" | "baidu")
}

pub(crate) fn is_claude_code_alias(name: &str) -> bool {
    matches!(name, "claude-code" | "claude-cli")
}

pub(crate) fn is_litellm_alias(name: &str) -> bool {
    matches!(name, "litellm" | "lite-llm")
}

pub(crate) fn is_vllm_alias(name: &str) -> bool {
    matches!(name, "vllm" | "v-llm")
}

/// The base URL provider construction addresses for the catalog provider
/// spelled `name` when its connection carries `api_url`: `Some` for the
/// adapters that honour a configured endpoint (`openai`, `ollama`,
/// llama.cpp, the Qwen OAuth aliases, LiteLLM, vLLM), `None` for the ones
/// that always use their own fixed endpoint. Everything that sends this
/// provider's key somewhere other than a generation — the live connection
/// test, `prx models refresh` — takes its host from here, so it addresses
/// exactly the host the provider itself does (D-095 M-1, D-078 T-7).
pub(crate) fn honoured_base_url<'a>(name: &str, api_url: Option<&'a str>) -> Option<&'a str> {
    let api_url = api_url.map(str::trim).filter(|url| !url.is_empty())?;
    let name = name.trim().to_ascii_lowercase();
    let honours = matches!(name.as_str(), "openai" | "ollama" | "llamacpp" | "llama.cpp")
        || is_qwen_oauth_alias(&name)
        || is_litellm_alias(&name)
        || is_vllm_alias(&name);
    honours.then_some(api_url)
}

/// `base` with `suffix` appended to its path, keeping its query and
/// dropping its fragment (`https://h/v1?sig=x` + `/models` =
/// `https://h/v1/models?sig=x`, D-095 L-4). A path already ending in
/// `suffix` is kept. `None` when `base` is not a URL.
pub(crate) fn endpoint_url(base: &str, suffix: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(base.trim()).ok()?;
    let path = url.path().trim_end_matches('/').to_string();
    if !path.ends_with(suffix) {
        url.set_path(&format!("{path}{suffix}"));
    }
    url.set_fragment(None);
    Some(url.to_string())
}

pub(crate) fn is_huggingface_alias(name: &str) -> bool {
    matches!(name, "huggingface" | "hf" | "hf-inference")
}

pub(crate) fn canonical_china_provider_name(name: &str) -> Option<&'static str> {
    if is_qwen_alias(name) {
        Some("qwen")
    } else if is_glm_alias(name) {
        Some("glm")
    } else if is_moonshot_alias(name) {
        Some("moonshot")
    } else if is_minimax_alias(name) {
        Some("minimax")
    } else if is_zai_alias(name) {
        Some("zai")
    } else if is_qianfan_alias(name) {
        Some("qianfan")
    } else {
        None
    }
}

pub(super) fn minimax_base_url(name: &str) -> Option<&'static str> {
    if is_minimax_cn_alias(name) {
        Some(MINIMAX_CN_BASE_URL)
    } else if is_minimax_intl_alias(name) {
        Some(MINIMAX_INTL_BASE_URL)
    } else {
        None
    }
}

pub(super) fn glm_base_url(name: &str) -> Option<&'static str> {
    if is_glm_cn_alias(name) {
        Some(GLM_CN_BASE_URL)
    } else if is_glm_global_alias(name) {
        Some(GLM_GLOBAL_BASE_URL)
    } else {
        None
    }
}

pub(super) fn moonshot_base_url(name: &str) -> Option<&'static str> {
    if is_moonshot_intl_alias(name) {
        Some(MOONSHOT_INTL_BASE_URL)
    } else if is_moonshot_cn_alias(name) {
        Some(MOONSHOT_CN_BASE_URL)
    } else {
        None
    }
}

pub(super) fn qwen_base_url(name: &str) -> Option<&'static str> {
    if is_qwen_cn_alias(name) || is_qwen_oauth_alias(name) {
        Some(QWEN_CN_BASE_URL)
    } else if is_qwen_intl_alias(name) {
        Some(QWEN_INTL_BASE_URL)
    } else if is_qwen_us_alias(name) {
        Some(QWEN_US_BASE_URL)
    } else {
        None
    }
}

pub(super) fn qwen_coding_base_url(name: &str) -> Option<&'static str> {
    if matches!(name, "dashscope-coding" | "qwen-coding") {
        Some(QWEN_CODING_CN_BASE_URL)
    } else if matches!(name, "dashscope-coding-intl" | "qwen-coding-intl") {
        Some(QWEN_CODING_INTL_BASE_URL)
    } else {
        None
    }
}

pub(super) fn zai_base_url(name: &str) -> Option<&'static str> {
    if is_zai_cn_alias(name) {
        Some(ZAI_CN_BASE_URL)
    } else if is_zai_global_alias(name) {
        Some(ZAI_GLOBAL_BASE_URL)
    } else {
        None
    }
}

/// The regional endpoint a provider name selects, for the vendors whose
/// aliases share one catalog row while addressing different servers.
///
/// A credential issued for one region is not a credential for another, so the
/// identity the credential resolvers compare has to carry the endpoint as well
/// as the vendor. These are the same pure selectors the factory dispatches on,
/// so the identity cannot disagree with the URL the provider is built with.
/// The argument is the trimmed, lowercased name.
pub(super) fn regional_endpoint_for(name: &str) -> Option<&'static str> {
    qwen_base_url(name)
        .or_else(|| qwen_coding_base_url(name))
        .or_else(|| glm_base_url(name))
        .or_else(|| moonshot_base_url(name))
        .or_else(|| minimax_base_url(name))
        .or_else(|| zai_base_url(name))
}
