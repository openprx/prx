# Providers

OpenPRX supports 14 LLM providers with automatic fallback chains, model routing, token refresh, and rate limiting.

| Provider | Models | Notes |
|----------|--------|-------|
| Anthropic | Claude Opus, Sonnet, Haiku | OAuth auto-refresh from Claude CLI |
| OpenAI | GPT-4o, GPT-5, o1/o3 | Codex models via dedicated provider |
| Google | Gemini 2.x | |
| DashScope | Qwen, Kimi | Coding Plan support |
| Ollama | Any local model | |
| OpenRouter | 100+ models | |
| AWS Bedrock | Claude, Titan, etc. | |
| GitHub Copilot | GPT-4o | Token auto-refresh |
| GLM (Zhipu) | GLM-4, GLM-5 | Chinese AI models |
| xAI | Grok | |
| LiteLLM | Unified proxy | Route to 100+ providers |
| vLLM | Self-hosted | High-throughput inference |
| HuggingFace | Open models | Inference API |
| Compatible | Any OpenAI-compatible | Custom base URL |

## Features

- **Fallback chains**: Configure per-model fallbacks (e.g. `claude-opus-4-6 → claude-sonnet-4-6`)
- **Provider fallback**: If primary provider fails, try alternatives (e.g. `anthropic → xai`)
- **Model routing**: Route specific models to specific providers
- **Token refresh**: Automatic OAuth token refresh for Anthropic (Claude CLI) and GitHub Copilot
- **Rate limiting**: Per-provider rate limit handling with backoff
- **HTTP/1.1 mode**: Configurable per-provider for compatibility (e.g. DashScope)
- **Custom User-Agent**: Per-provider UA header configuration

## OpenAI Codex Notes

- OpenAI Codex response parsing now fails fast with structured errors for malformed/unexpected payloads (`provider_response_parse_error`), instead of waiting on long tail body decode.
- Stream idle timeout is controlled by `ZEROCLAW_CODEX_STREAM_IDLE_TIMEOUT_SECS` (default: `45` seconds, minimum: `5` seconds).

## Runtime truth boundaries

- Capability checks are model- and request-mode-specific. A streaming request is
  not allowed to inherit a capability advertised only by a non-streaming path.
- Router capabilities describe the provider selected for the resolved route.
  Reliable failover advertises only the capability intersection of every
  compatible candidate, so a later fallback cannot invalidate the request.
- Gemini currently reports native tool calling as unavailable because its
  generate-content requests do not yet send tool schemas. Parsing a function
  call response alone is not treated as end-to-end support.
- Provider construction and availability checks use the same credential
  resolver. A route receives only its own resolved credential; a primary
  provider credential is never inherited by a different routed provider.
- Non-streaming and streaming execution both retain the ordered provider/model
  attempt trace. The successful attempt is the source of final provider and
  model attribution.

The canonical routing, usage, and cost settlement boundary is described in
`provider-routing-cost-lifecycle.md`.
