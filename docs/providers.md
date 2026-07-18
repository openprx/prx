# Providers

OpenPRX supports native and OpenAI-compatible LLM providers with model routing,
ordered fallback, token refresh, streaming, tools, and usage attribution. Model
catalogs move faster than releases, so `prx doctor models` (read-only) or
`prx models refresh --provider <id>` (cache-updating) is the runtime source of
truth whenever the provider exposes a model-list API.

## Verified catalog baseline (2026-07-18)

| PRX provider | Curated default / current IDs | Endpoint and authoritative source | Live discovery |
|---|---|---|---|
| `kimi-code` | `k3`; `kimi-for-coding`; `kimi-for-coding-highspeed` | `https://api.kimi.com/coding/v1`; [Kimi Code models](https://www.kimi.com/code/docs/en/kimi-code/models.html) | `GET /models`, bearer token |
| `openai` | `gpt-5.6`; `gpt-5.6-terra`; `gpt-5.6-luna` | `https://api.openai.com/v1`; [OpenAI models](https://developers.openai.com/api/docs/models) | `GET /models`, bearer token |
| `openai-codex` | `gpt-5.2-codex`; `gpt-5-codex` | Responses API; [GPT-5.2 Codex](https://developers.openai.com/api/docs/models/gpt-5.2-codex) | OAuth catalog is service-managed |
| `anthropic` | `claude-sonnet-5`; `claude-opus-4-8`; `claude-haiku-4-5-20251001` | `https://api.anthropic.com`; [Claude models](https://platform.claude.com/docs/en/about-claude/models/overview) | Anthropic models API, `x-api-key` |
| `gemini` | `gemini-3.5-flash`; `gemini-3.1-pro-preview`; `gemini-3.1-flash-lite` | Google Generative Language API; [Gemini models](https://ai.google.dev/gemini-api/docs/models) | `v1beta/models?key=...` |
| `qwen` | `qwen3.7-max`; `qwen3.7-plus`; `qwen3.6-flash` | China `dashscope.aliyuncs.com`, international `dashscope-intl.aliyuncs.com`, US `dashscope-us.aliyuncs.com`; [Model Studio models](https://help.aliyun.com/en/model-studio/models) | regional `GET /compatible-mode/v1/models` |
| `openrouter` | dynamic `vendor/model` IDs | `https://openrouter.ai/api/v1`; [model catalog](https://openrouter.ai/docs/guides/overview/models) | public `GET /models`; response `model` identifies router-selected model |
| `xai` | `grok-4.20`; `grok-4.20-non-reasoning-latest` | `https://api.x.ai/v1`; [Grok 4.20](https://docs.x.ai/developers/models/grok-4.20-experimental-beta-0304) | `GET /models`, bearer token |
| `glm` / `zai` | global `glm-5`; China `glm-5.2` | global `api.z.ai`, China `open.bigmodel.cn`; [GLM-5](https://docs.z.ai/guides/llm/glm-5), [GLM-5.2](https://docs.bigmodel.cn/cn/guide/models/text/glm-5.2) | regional `GET /models` |
| `bedrock` | `anthropic.claude-sonnet-5`; `anthropic.claude-opus-4-8`; Haiku 4.5 regional ID | AWS Converse; [model/API compatibility](https://docs.aws.amazon.com/bedrock/latest/userguide/models-api-compatibility.html) | AWS regional catalog; invocation uses Converse/ConverseStream |
| `copilot` | `gpt-5.4`; `gpt-5.3-codex`; `claude-sonnet-4.6` | GitHub Copilot service; [supported models](https://docs.github.com/en/copilot/reference/ai-models/supported-models) | service-managed; OAuth token auto-refresh |
| `ollama` | installed model tags | default `http://localhost:11434`; [list models](https://docs.ollama.com/api/tags) | unauthenticated `GET /api/tags` locally |
| `llamacpp` | server-loaded model ID | default `http://localhost:8080/v1`; [llama.cpp server](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md) | `GET /v1/models`; capabilities depend on template/server flags |
| `vllm` | server-loaded model ID | default `http://localhost:8000/v1`; [vLLM OpenAI server](https://docs.vllm.ai/en/stable/serving/openai_compatible_server/) | `GET /v1/models`; tools depend on served model/parser |
| `litellm` | proxy alias or upstream model ID | default `http://localhost:4000`; [LiteLLM proxy](https://docs.litellm.ai/) | `GET /v1/models`; upstream capabilities are route-dependent |
| `huggingface` | router model ID | `https://router.huggingface.co/v1`; [chat completion](https://huggingface.co/docs/inference-providers/en/tasks/chat-completion) | `GET /v1/models`, bearer token |
| `compatible` | endpoint-defined | configured OpenAI-compatible `/v1` base URL | `GET <base>/models`; capabilities are endpoint/model-dependent |

Curated entries are onboarding defaults, not entitlement claims. Live discovery
also reads the active auth profile when `api_key` is absent, and custom
llama.cpp/vLLM/LiteLLM/Hugging Face/compatible base URLs are honored for model
discovery. Kimi Code rejects model strings outside its three documented IDs
because that endpoint can silently accept or remap a mistyped model.

## Capability contracts

- OpenAI-compatible providers support non-streaming and streaming text and
  preserve provider reasoning fields where the upstream format exposes them.
  Tool and vision support remains model/endpoint dependent.
- Anthropic uses native tool blocks and preserves thinking blocks across tool
  loops. See [extended thinking](https://platform.claude.com/docs/en/build-with-claude/extended-thinking).
- Gemini sends native `functionDeclarations`, parses `functionCall`, sends
  `functionResponse` on the next turn, and converts validated image markers to
  Gemini `inlineData`. Gemini 3.x `thoughtSignature` values are retained across
  tool loops. Both streaming and non-streaming requests carry tools.
- Bedrock uses Converse/ConverseStream native content and tool blocks.
- Ollama uses its native chat/tool schema. llama.cpp, vLLM, LiteLLM, Hugging
  Face, and custom endpoints use the OpenAI-compatible adapter.
- Capability checks are request-mode-specific. Reliable failover exposes only
  the intersection supported by every candidate, so a fallback cannot silently
  invalidate a tool or vision request.

## Routing, fallback, and cost truth

Provider construction and availability checks use the same credential
resolver. A route receives only its own credential. Both streaming and
non-streaming execution retain ordered provider/model attempt traces, and the
successful attempt supplies final PRX provider/model attribution. If an
aggregator such as OpenRouter performs an additional internal fallback, its
response `model` is the authoritative upstream-model signal.

Static prices are recorded only when the provider publishes token prices for a
stable model ID. Current Anthropic, OpenAI GPT-5.6, and documented Gemini 3.1
prices are included. Kimi Code is subscription/quota billed, so PRX deliberately
does not invent a per-token cost for `k3` or the `kimi-for-coding*` aliases.
OpenRouter and proxy pricing remains dynamic.

The canonical routing, usage, and cost settlement boundary is described in
`provider-routing-cost-lifecycle.md`.

## OpenAI Codex notes

- Malformed/unexpected response payloads fail fast with
  `provider_response_parse_error`.
- Stream idle timeout is controlled by
  `ZEROCLAW_CODEX_STREAM_IDLE_TIMEOUT_SECS` (default 45 seconds, minimum 5).
