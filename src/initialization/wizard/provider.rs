use super::{
    CUSTOM_MODEL_SENTINEL, Config, Confirm, Input, LIVE_MODEL_MAX_OPTIONS, MODEL_CACHE_TTL_SECS, Path, Result, Select,
    WizardModelFetchStatus, allows_unauthenticated_model_fetch, apply_wizard_model_fetch, build_model_options,
    canonical_provider_name, catalog_models_for_provider, default_model_for_provider,
    fetch_live_models_for_provider_off_runtime, humanize_age, is_glm_alias, is_glm_cn_alias, is_minimax_alias,
    is_moonshot_alias, is_qianfan_alias, is_qwen_alias, is_zai_alias, is_zai_cn_alias, load_cached_models_for_provider,
    print_bullet, prioritize_default_model, provider_credential_env_var, style, supports_live_model_fetch,
};

// ── Step 2: Provider & API Key ───────────────────────────────────

#[allow(clippy::too_many_lines)]
pub(super) async fn setup_provider(
    workspace_dir: &Path,
    state_dir: &Path,
) -> Result<(String, String, String, Option<String>)> {
    // ── Tier selection ──
    let tiers = vec![
        "⭐ Recommended (OpenRouter, Venice, Anthropic, OpenAI, Gemini)",
        "⚡ Fast inference (Groq, Fireworks, Together AI, NVIDIA NIM)",
        "🌐 Gateway / proxy (Vercel AI, Cloudflare AI, Amazon Bedrock)",
        "🔬 Specialized (Moonshot/Kimi, GLM/Zhipu, MiniMax, Qwen/DashScope, Qianfan, Z.AI, Synthetic, OpenCode Zen, Cohere)",
        "🏠 Local / private (Ollama, llama.cpp server — no API key needed)",
        "🔧 Custom — bring your own OpenAI-compatible API",
    ];

    let tier_idx = Select::new()
        .with_prompt("  Select provider category")
        .items(&tiers)
        .default(0)
        .interact()?;

    let providers: Vec<(&str, &str)> = match tier_idx {
        0 => vec![
            ("openrouter", "OpenRouter — 200+ models, 1 API key (recommended)"),
            ("venice", "Venice AI — privacy-first (Llama, Opus)"),
            ("anthropic", "Anthropic — Claude Sonnet & Opus (direct)"),
            ("openai", "OpenAI — GPT-4o, o1, GPT-5 (direct)"),
            (
                "openai-codex",
                "OpenAI Codex — Sign in with ChatGPT (loopback PKCE, no API key)",
            ),
            ("deepseek", "DeepSeek — V3 & R1 (affordable)"),
            (
                "mistral",
                "Mistral — API key (Le Chat Pro includes monthly API credits)",
            ),
            ("xai", "xAI — Grok 3 & 4"),
            ("perplexity", "Perplexity — search-augmented AI"),
            ("gemini", "Google Gemini — Gemini API key"),
        ],
        1 => vec![
            ("groq", "Groq — ultra-fast LPU inference"),
            ("fireworks", "Fireworks AI — fast open-source inference"),
            ("together-ai", "Together AI — open-source model hosting"),
            ("nvidia", "NVIDIA NIM — DeepSeek, Llama, & more"),
        ],
        2 => vec![
            ("vercel", "Vercel AI Gateway"),
            ("cloudflare", "Cloudflare AI Gateway"),
            (
                "astrai",
                "Astrai — compliant AI routing (PII stripping, cost optimization)",
            ),
            ("bedrock", "Amazon Bedrock — AWS managed models"),
        ],
        3 => vec![
            ("kimi-code", "Kimi Code — coding-plan API key"),
            (
                "qwen-coding",
                "Bailian Coding Plan — API key (interactive use; cron/webhook/daemon are outside the terms)",
            ),
            ("moonshot", "Moonshot — Kimi API (China endpoint)"),
            ("moonshot-intl", "Moonshot — Kimi API (international endpoint)"),
            ("glm", "GLM — ChatGLM / Zhipu (international endpoint)"),
            ("glm-cn", "GLM — ChatGLM / Zhipu (China endpoint)"),
            ("minimax", "MiniMax — international endpoint (api.minimax.io)"),
            ("minimax-cn", "MiniMax — China endpoint (api.minimaxi.com)"),
            ("qwen", "Qwen — DashScope China endpoint"),
            ("qwen-intl", "Qwen — DashScope international endpoint"),
            ("qwen-us", "Qwen — DashScope US endpoint"),
            ("qianfan", "Qianfan — Baidu AI models (China endpoint)"),
            ("zai", "Z.AI — global coding endpoint"),
            ("zai-cn", "Z.AI — China coding endpoint (open.bigmodel.cn)"),
            ("synthetic", "Synthetic — Synthetic AI models"),
            ("opencode", "OpenCode Zen — code-focused AI"),
            ("cohere", "Cohere — Command R+ & embeddings"),
        ],
        4 => vec![
            ("ollama", "Ollama — local models (Llama, Mistral, Phi)"),
            ("llamacpp", "llama.cpp server — local OpenAI-compatible endpoint"),
        ],
        _ => vec![], // Custom — handled below
    };

    // ── Custom / BYOP flow ──
    if providers.is_empty() {
        println!();
        println!(
            "  {} {}",
            style("Custom Provider Setup").white().bold(),
            style("— any OpenAI-compatible API").dim()
        );
        print_bullet("OpenPRX works with ANY API that speaks the OpenAI chat completions format.");
        print_bullet("Examples: LiteLLM, LocalAI, vLLM, text-generation-webui, LM Studio, etc.");
        println!();

        let base_url: String = Input::new()
            .with_prompt("  API base URL (e.g. http://localhost:1234 or https://my-api.com)")
            .interact_text()?;

        let base_url = base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            anyhow::bail!("Custom provider requires a base URL.");
        }

        let api_key: String = Input::new()
            .with_prompt("  API key (or Enter to skip if not needed)")
            .allow_empty(true)
            .interact_text()?;

        let model: String = Input::new()
            .with_prompt("  Model name (e.g. llama3, gpt-4o, mistral)")
            .default("default".into())
            .interact_text()?;

        let provider_name = format!("custom:{base_url}");

        println!("{}", selected_provider_line(&provider_name, &model));

        return Ok((provider_name, api_key, model, None));
    }

    let provider_labels: Vec<&str> = providers.iter().map(|(_, label)| *label).collect();

    let provider_idx = Select::new()
        .with_prompt("  Select your AI provider")
        .items(&provider_labels)
        .default(0)
        .interact()?;

    // SAFETY: dialoguer Select guarantees provider_idx < providers.len()
    #[allow(clippy::indexing_slicing)]
    let provider_name = providers[provider_idx].0;

    if is_zai_alias(provider_name) {
        print_bullet(crate::auth::terms::GLM_CODING_PLAN_WARNING);
    }
    if matches!(
        provider_name,
        "qwen-coding" | "qwen-coding-intl" | "dashscope-coding" | "dashscope-coding-intl"
    ) {
        print_bullet(crate::auth::terms::BAILIAN_CODING_PLAN_WARNING);
    }
    if provider_name == "openrouter" {
        print_bullet(
            "Or sign in with `prx auth login --provider openrouter`. Add `--device-code` and paste the code on a headless machine.",
        );
    }
    if provider_name == "minimax" || provider_name == "minimax-cn" {
        print_bullet("MiniMax M Plan uses a console API key. OAuth is experimental.");
        print_bullet(crate::auth::terms::MINIMAX_OAUTH_WARNING);
    }

    // ── API key / endpoint ──
    let mut provider_api_url: Option<String> = None;
    // OAuth sign-in stores the refreshable token in the auth profile. The
    // access token is kept only long enough to list models, and is not written
    // into config.toml.
    let mut discovery_key: Option<String> = None;
    let api_key = if provider_name == "ollama" {
        let use_remote_ollama = Confirm::new()
            .with_prompt("  Use a remote Ollama endpoint (for example Ollama Cloud)?")
            .default(false)
            .interact()?;

        if use_remote_ollama {
            let raw_url: String = Input::new()
                .with_prompt("  Remote Ollama endpoint URL")
                .default("https://ollama.com".into())
                .interact_text()?;

            let normalized_url = raw_url.trim().trim_end_matches('/').to_string();
            if normalized_url.is_empty() {
                anyhow::bail!("Remote Ollama endpoint URL cannot be empty.");
            }

            provider_api_url = Some(normalized_url.clone());

            print_bullet(&format!(
                "Remote endpoint configured: {}",
                style(&normalized_url).cyan()
            ));
            print_bullet(&format!(
                "If you use cloud-only models, append {} to the model ID.",
                style(":cloud").yellow()
            ));

            let key: String = Input::new()
                .with_prompt("  API key for remote Ollama endpoint (or Enter to skip)")
                .allow_empty(true)
                .interact_text()?;

            if key.trim().is_empty() {
                print_bullet(&format!(
                    "No API key provided. If your endpoint requires one, later {}.",
                    style(credential_setup_hint(provider_name)).yellow()
                ));
            }

            key
        } else {
            print_bullet("Using local Ollama at http://localhost:11434 (no API key needed).");
            String::new()
        }
    } else if matches!(provider_name, "llamacpp" | "llama.cpp") {
        let raw_url: String = Input::new()
            .with_prompt("  llama.cpp server endpoint URL")
            .default("http://localhost:8080/v1".into())
            .interact_text()?;

        let normalized_url = raw_url.trim().trim_end_matches('/').to_string();
        if normalized_url.is_empty() {
            anyhow::bail!("llama.cpp endpoint URL cannot be empty.");
        }
        provider_api_url = Some(normalized_url.clone());

        print_bullet(&format!(
            "Using llama.cpp server endpoint: {}",
            style(&normalized_url).cyan()
        ));
        print_bullet("No API key needed unless your llama.cpp server is started with --api-key.");

        let key: String = Input::new()
            .with_prompt("  API key for llama.cpp server (or Enter to skip)")
            .allow_empty(true)
            .interact_text()?;

        if key.trim().is_empty() {
            print_bullet(&format!(
                "No API key provided. Only if your server requires authentication, later {}.",
                style(credential_setup_hint(provider_name)).yellow()
            ));
        }

        key
    } else if canonical_provider_name(provider_name) == "gemini" {
        if std::env::var("GEMINI_API_KEY").is_ok() {
            print_bullet(&format!(
                "{} GEMINI_API_KEY environment variable detected!",
                style("✓").green().bold()
            ));
            String::new()
        } else {
            print_bullet("Get your API key at: https://aistudio.google.com/app/apikey");
            print_bullet(crate::auth::terms::GEMINI_CLI_OAUTH_REMOVED);
            println!();

            Input::new()
                .with_prompt("  Paste your Gemini API key (or press Enter to skip)")
                .allow_empty(true)
                .interact_text()?
        }
    } else if canonical_provider_name(provider_name) == "xai" {
        let (stored, discovered) = prompt_xai_credential(state_dir).await?;
        discovery_key = discovered;
        stored
    } else if canonical_provider_name(provider_name) == "anthropic" {
        // PRX reads no Anthropic credential from the environment, so a
        // variable set in this shell is not "detected" as configured: the
        // key has to be pasted here or stored later (ONBOARD-HINT, D-062).
        for variable in ["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"] {
            if std::env::var_os(variable).is_some() {
                print_bullet(&format!(
                    "{} is set in this shell, but PRX does not read it: paste the key below to use it.",
                    style(variable).yellow()
                ));
            }
        }
        print_bullet(&format!(
            "Get your API key at: {}",
            style("https://console.anthropic.com/settings/keys").cyan().underlined()
        ));
        print_bullet(
            "Max, Ultra, and Team plans include monthly API credits: https://support.claude.com/en/articles/17154008",
        );
        println!();

        let key: String = Input::new()
            .with_prompt("  Paste your Console API key (or press Enter to skip)")
            .allow_empty(true)
            .interact_text()?;

        if key.is_empty() {
            print_bullet(&format!(
                "Skipped. Later, {}.",
                style(credential_setup_hint(provider_name)).yellow()
            ));
        }

        key
    } else {
        let key_url = if is_moonshot_alias(provider_name) || canonical_provider_name(provider_name) == "kimi-code" {
            "https://platform.moonshot.cn/console/api-keys"
        } else if canonical_provider_name(provider_name) == "qwen-code" {
            "https://qwen.readthedocs.io/en/latest/getting_started/installation.html"
        } else if is_glm_cn_alias(provider_name) || is_zai_cn_alias(provider_name) {
            "https://open.bigmodel.cn/usercenter/proj-mgmt/apikeys"
        } else if is_glm_alias(provider_name) || is_zai_alias(provider_name) {
            "https://platform.z.ai/"
        } else if is_minimax_alias(provider_name) {
            "https://www.minimaxi.com/user-center/basic-information"
        } else if matches!(provider_name, "qwen-coding" | "qwen-coding-intl") {
            "https://help.aliyun.com/zh/model-studio/developer-reference/get-api-key"
        } else if is_qwen_alias(provider_name) {
            "https://help.aliyun.com/zh/model-studio/developer-reference/get-api-key"
        } else if is_qianfan_alias(provider_name) {
            "https://cloud.baidu.com/doc/WENXINWORKSHOP/s/7lm0vxo78"
        } else {
            match provider_name {
                "openrouter" => "https://openrouter.ai/keys",
                "openai" => "https://platform.openai.com/api-keys",
                "venice" => "https://venice.ai/settings/api",
                "groq" => "https://console.groq.com/keys",
                "mistral" => "https://console.mistral.ai/api-keys",
                "deepseek" => "https://platform.deepseek.com/api_keys",
                "together-ai" => "https://api.together.xyz/settings/api-keys",
                "fireworks" => "https://fireworks.ai/account/api-keys",
                "perplexity" => "https://www.perplexity.ai/settings/api",
                "xai" => "https://console.x.ai",
                "cohere" => "https://dashboard.cohere.com/api-keys",
                "vercel" => "https://vercel.com/account/tokens",
                "cloudflare" => "https://dash.cloudflare.com/profile/api-tokens",
                "nvidia" | "nvidia-nim" | "build.nvidia.com" => "https://build.nvidia.com/",
                "bedrock" => "https://console.aws.amazon.com/iam",
                "gemini" => "https://aistudio.google.com/app/apikey",
                "astrai" => "https://as-trai.com",
                _ => "",
            }
        };

        println!();
        if matches!(provider_name, "bedrock" | "aws-bedrock") {
            // Bedrock uses AWS AKSK, not a single API key.
            print_bullet("Bedrock uses AWS credentials (not a single API key).");
            print_bullet(&format!(
                "Set {} and {} environment variables.",
                style("AWS_ACCESS_KEY_ID").yellow(),
                style("AWS_SECRET_ACCESS_KEY").yellow(),
            ));
            print_bullet(&format!(
                "Optionally set {} for the region (default: us-east-1).",
                style("AWS_REGION").yellow(),
            ));
            if !key_url.is_empty() {
                print_bullet(&format!(
                    "Manage IAM credentials at: {}",
                    style(key_url).cyan().underlined()
                ));
            }
            println!();
            String::new()
        } else {
            if !key_url.is_empty() {
                print_bullet(&format!("Get your API key at: {}", style(key_url).cyan().underlined()));
            }
            print_bullet(&format!(
                "You can also add it later: {}.",
                credential_setup_hint(provider_name)
            ));
            println!();

            let key: String = Input::new()
                .with_prompt("  Paste your API key (or press Enter to skip)")
                .allow_empty(true)
                .interact_text()?;

            if key.is_empty() {
                print_bullet(&format!(
                    "Skipped. Later, {}.",
                    style(credential_setup_hint(provider_name)).yellow()
                ));
            }

            key
        }
    };

    // ── Model selection ──
    let discovery_key = discovery_key.unwrap_or_else(|| api_key.clone());
    let canonical_provider = canonical_provider_name(provider_name);
    let mut model_options: Vec<(String, String)> = catalog_models_for_provider(canonical_provider);

    let mut live_options: Option<Vec<(String, String)>> = None;

    if supports_live_model_fetch(provider_name) {
        let can_fetch_without_key = allows_unauthenticated_model_fetch(provider_name);
        let has_api_key = !discovery_key.trim().is_empty()
            || provider_credential_env_var(provider_name)
                .and_then(|variable| std::env::var(variable).ok())
                .is_some_and(|value| !value.trim().is_empty())
            || (provider_name == "minimax"
                && std::env::var("MINIMAX_OAUTH_TOKEN").is_ok_and(|value| !value.trim().is_empty()));

        if can_fetch_without_key || has_api_key {
            if let Some(cached) = load_cached_models_for_provider(workspace_dir, provider_name, MODEL_CACHE_TTL_SECS)? {
                let shown_count = cached.models.len().min(LIVE_MODEL_MAX_OPTIONS);
                print_bullet(&format!(
                    "Found cached models ({shown_count}) updated {} ago.",
                    humanize_age(cached.age_secs)
                ));

                live_options = Some(build_model_options(
                    provider_name,
                    cached.models.into_iter().take(LIVE_MODEL_MAX_OPTIONS).collect(),
                    "cached",
                ));
            }

            let should_fetch_now = Confirm::new()
                .with_prompt(if live_options.is_some() {
                    "  Refresh models from provider now?"
                } else {
                    "  Fetch latest models from provider now?"
                })
                .default(live_options.is_none())
                .interact()?;

            if should_fetch_now {
                let fetch = fetch_live_models_for_provider_off_runtime(
                    provider_name.to_string(),
                    discovery_key.clone(),
                    provider_api_url.clone(),
                )
                .await;
                match apply_wizard_model_fetch(workspace_dir, provider_name, &mut live_options, fetch)? {
                    WizardModelFetchStatus::Fetched {
                        fetched_count,
                        shown_count,
                    } => {
                        if shown_count < fetched_count {
                            print_bullet(&format!("Fetched {fetched_count} models. Showing first {shown_count}."));
                        } else {
                            print_bullet(&format!("Fetched {shown_count} live models."));
                        }
                    }
                    WizardModelFetchStatus::Empty => {
                        print_bullet("Provider returned no models; using curated list.");
                    }
                    WizardModelFetchStatus::Failed { error, stale_age_secs } => {
                        print_bullet(&format!(
                            "Live fetch failed ({}); using cached/curated list.",
                            style(error).yellow()
                        ));
                        if let Some(age_secs) = stale_age_secs {
                            print_bullet(&format!("Loaded stale cache from {} ago.", humanize_age(age_secs)));
                        }
                    }
                }
            }
        } else {
            print_bullet("No API key detected, so using the embedded release catalog.");
            print_bullet("Tip: add an API key and rerun onboarding to fetch live models.");
        }
    }

    if let Some(live_model_options) = live_options {
        let source_options = vec![
            format!("Provider model list ({})", live_model_options.len()),
            format!("Curated starter list ({})", model_options.len()),
        ];

        let source_idx = Select::new()
            .with_prompt("  Model source")
            .items(&source_options)
            .default(0)
            .interact()?;

        if source_idx == 0 {
            model_options = live_model_options;
        }
    }

    if model_options.is_empty() {
        model_options.push((
            default_model_for_provider(provider_name),
            "Provider default model".to_string(),
        ));
    }
    prioritize_default_model(provider_name, &mut model_options);

    model_options.push((
        CUSTOM_MODEL_SENTINEL.to_string(),
        "Custom model ID (type manually)".to_string(),
    ));

    let model_labels: Vec<String> = model_options
        .iter()
        .map(|(model_id, label)| format!("{label} — {}", style(model_id).dim()))
        .collect();

    let model_idx = Select::new()
        .with_prompt("  Select your default model")
        .items(&model_labels)
        .default(0)
        .interact()?;

    // SAFETY: dialoguer Select guarantees model_idx < model_options.len()
    #[allow(clippy::indexing_slicing)]
    let selected_model = model_options[model_idx].0.clone();
    let model = if selected_model == CUSTOM_MODEL_SENTINEL {
        Input::new()
            .with_prompt("  Enter custom model ID")
            .default(default_model_for_provider(provider_name))
            .interact_text()?
    } else {
        selected_model
    };

    println!("{}", selected_provider_line(provider_name, &model));

    Ok((provider_name.to_string(), api_key, model, provider_api_url))
}

/// The onboarding line naming the provider a quick setup configured.
///
/// Each provider line below prints the label through the one redaction helper:
/// a `custom:` URL may carry `user:password@`.
#[cfg(test)]
pub(super) fn setup_provider_line(provider_name: &str) -> String {
    format!(
        "  {} Provider:   {}",
        style("✓").green().bold(),
        style(crate::providers::redact_endpoint_userinfo(provider_name)).green()
    )
}

/// The onboarding line confirming the provider and model just selected.
pub(super) fn selected_provider_line(provider_name: &str, model: &str) -> String {
    format!(
        "  {} Provider: {} | Model: {}",
        style("✓").green().bold(),
        style(crate::providers::redact_endpoint_userinfo(provider_name)).green(),
        style(model).green()
    )
}

/// The provider line of the closing quick summary.
pub(super) fn quick_summary_provider_line(config: &Config) -> String {
    format!(
        "    {} Provider:      {}",
        style("🤖").cyan(),
        crate::providers::redact_endpoint_userinfo(crate::providers::selection::default_provider(config))
    )
}

async fn prompt_xai_credential(state_dir: &Path) -> Result<(String, Option<String>)> {
    let choices = [
        "Grok subscription — browser sign-in (SuperGrok / X Premium)",
        "Grok subscription — device code (SSH / headless)",
        "xAI API key",
    ];
    println!();
    print_bullet("Grok subscribers can sign in without an API key.");
    print_bullet("An API key from https://console.x.ai still works.");
    let index = Select::new()
        .with_prompt("  How should xAI authenticate?")
        .items(&choices)
        .default(0)
        .interact()?;
    match index {
        0 => Ok(finish_xai_sign_in(state_dir, false).await),
        1 => Ok(finish_xai_sign_in(state_dir, true).await),
        _ => {
            print_bullet("Get your API key at: https://console.x.ai");
            print_bullet(&format!("You can also add it later: {}.", credential_setup_hint("xai")));
            let key: String = Input::new()
                .with_prompt("  Paste your API key (or press Enter to skip)")
                .allow_empty(true)
                .interact_text()?;
            if key.is_empty() {
                print_bullet(&format!(
                    "Skipped. Later, {}.",
                    style(credential_setup_hint("xai")).yellow()
                ));
            }
            Ok((key, None))
        }
    }
}

async fn finish_xai_sign_in(state_dir: &Path, device_code: bool) -> (String, Option<String>) {
    match sign_in_xai(state_dir, device_code).await {
        Ok(access_token) => {
            println!("  {} Grok subscription saved", style("✓").green().bold());
            (String::new(), Some(access_token))
        }
        Err(error) => {
            print_bullet(&format!("Sign-in did not finish ({error})."));
            print_bullet("Finish later with `prx auth login --provider xai`, or add `--device-code` on a remote host.");
            (String::new(), None)
        }
    }
}

async fn sign_in_xai(state_dir: &Path, device_code: bool) -> Result<String> {
    let encrypt = crate::config::Config::load_from_path(&state_dir.join("config.toml"), state_dir.join("workspace"))
        .map(|config| config.secrets.encrypt)
        .unwrap_or(true);
    let auth = crate::auth::AuthService::new(state_dir, encrypt);
    let client = reqwest::Client::new();
    if device_code {
        let device = crate::auth::xai_oauth::start_device_code_flow(&client).await?;
        println!("  Visit: {}", device.verification_uri);
        println!("  Code:  {}", device.user_code);
        if let Some(link) = &device.verification_uri_complete {
            println!("  Fast link: {link}");
        }
        let tokens = crate::auth::xai_oauth::poll_device_code_tokens(&client, &device).await?;
        let access = tokens.access_token.clone();
        auth.store_xai_tokens("default", tokens, true)?;
        return Ok(access);
    }

    let pkce = crate::auth::xai_oauth::generate_pkce();
    let pending = crate::auth::xai_oauth::PendingXaiLogin {
        profile: "default".into(),
        code_verifier: pkce.code_verifier.clone(),
        state: pkce.state.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    crate::auth::xai_oauth::save_pending_login(state_dir, encrypt, &pending)?;
    println!("  Open this URL and approve access:");
    println!("  {}", crate::auth::xai_oauth::build_authorize_url(&pkce));
    let code = crate::auth::xai_oauth::receive_loopback_code(&pkce.state, std::time::Duration::from_secs(180)).await?;
    let tokens = crate::auth::xai_oauth::exchange_code_for_tokens(&client, &code, &pkce.code_verifier).await?;
    let access = tokens.access_token.clone();
    auth.store_xai_tokens("default", tokens, true)?;
    crate::auth::xai_oauth::clear_pending_login(state_dir);
    Ok(access)
}

/// The environment variable the daemon itself reads for `name`'s credential,
/// if there is one (ONBOARD-HINT, D-062).
///
/// Provider construction reads `GEMINI_API_KEY` (or `GOOGLE_API_KEY`) for
/// Gemini. Bedrock reads the AWS variables, which its own step explains.
/// Exporting `OPENROUTER_API_KEY` or the like changes nothing for the daemon,
/// the gateway or a channel, so onboarding never suggests one; it points at
/// the configuration or the auth store instead. Model discovery during
/// onboarding reads more ([`provider_credential_env_var`]).
pub(super) fn daemon_credential_env_var(name: &str) -> Option<&'static str> {
    match canonical_provider_name(name) {
        "gemini" => Some("GEMINI_API_KEY"),
        _ => None,
    }
}

/// How onboarding tells the operator to supply `name`'s credential later:
/// only a variable the daemon reads, and always the supported mechanism
/// (`api_key` in config.toml or `prx auth paste-token`).
pub(super) fn credential_setup_hint(name: &str) -> String {
    if canonical_provider_name(name) == "xai" {
        let provider = canonical_provider_name(name);
        return format!(
            "run `prx auth login --provider {provider}` or `prx auth login --provider {provider} --device-code`, or set `api_key` in config.toml (or run `prx auth paste-token --provider {provider}`)"
        );
    }
    daemon_credential_env_var(name).map_or_else(
        || keyless_provider_hint(name),
        |variable| format!("export {variable} for the daemon, or {}", keyless_provider_hint(name)),
    )
}

/// How onboarding tells the operator to supply a key later for a provider
/// with no variable of its own (INT-2B a36). Nothing reads a generic
/// `API_KEY`, so the key goes into the configuration or the auth store.
///
/// The command must work when pasted (B2f F-8): the label is shell-quoted
/// when it needs it, and a label that carries URL userinfo is never printed
/// (neither the password nor a redacted spelling, which would register the
/// key under a different identity), so the operator is pointed at the label
/// in config.toml instead.
pub(super) fn keyless_provider_hint(name: &str) -> String {
    let provider = match crate::providers::redact_endpoint_userinfo(name) {
        std::borrow::Cow::Borrowed(_) => shell_quoted(name),
        std::borrow::Cow::Owned(_) => "<your default_provider value from config.toml, quoted>".to_string(),
    };
    format!("set `api_key` in config.toml (or run `prx auth paste-token --provider {provider}`)")
}

/// `value` as one shell word: unchanged when it holds only characters no
/// POSIX shell treats specially, otherwise single-quoted.
pub(super) fn shell_quoted(value: &str) -> String {
    let plain = !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/' | b'@' | b'+' | b'=' | b',')
        });
    if plain {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

pub(super) fn provider_supports_keyless_local_usage(provider_name: &str) -> bool {
    matches!(canonical_provider_name(provider_name), "ollama" | "llamacpp")
}

pub(super) fn choose_model_level(
    provider: &str,
    model: &str,
) -> Result<std::collections::BTreeMap<String, crate::config::model_settings::ModelSettings>> {
    let release = crate::providers::catalog::ReleaseCatalog::embedded_ref()?;
    let Some(row) = release.model(&crate::providers::ProviderId::of(provider), model) else {
        return Ok(Default::default());
    };
    if row.levels.is_empty() {
        return Ok(Default::default());
    }
    let levels = ["low", "medium", "high"];
    let default = levels
        .iter()
        .position(|level| row.default_level.as_deref() == Some(*level))
        .unwrap_or(1);
    let choice = Select::new()
        .with_prompt("Model level")
        .items(levels)
        .default(default)
        .interact()?;
    let level = levels
        .get(choice)
        .ok_or_else(|| anyhow::anyhow!("invalid model level selection"))?;
    let settings = crate::config::model_settings::ModelSettings {
        level: Some((*level).to_owned()),
        ..Default::default()
    };
    settings.validate(&row.id)?;
    Ok([(row.id.clone(), settings)].into())
}
