# PRX 0.8.13 Release Notes

Release date: 2026-07-18

PRX 0.8.13 refreshes hosted and local LLM provider contracts, model catalogs,
defaults, validation, pricing references, and discovery behavior.

## Provider and model refresh

- Kimi Code now discovers and validates the official `k3`,
  `kimi-for-coding-highspeed`, and `kimi-for-coding` model IDs. K3 is the
  onboarding default after live catalog, text, streaming, and tool round-trip
  acceptance.
- OpenAI, Anthropic, Gemini, Qwen, OpenRouter, xAI, GLM/Zhipu, Bedrock, GitHub
  Copilot, Ollama, llama.cpp, vLLM, LiteLLM, Hugging Face, and generic
  OpenAI-compatible metadata and defaults are aligned with their current
  provider contracts.
- Gemini native tool declarations, streamed function calls, function results,
  image inputs, and tool schema handling are implemented end to end.
- Custom OpenAI-compatible and local endpoints can discover their model
  catalogs without assuming a hosted-provider credential contract.

## Verification boundary

Focused provider, onboarding, credential-resolution, validation, and pricing
tests pass. The complete library suite passes serially. The full workspace
retains seven pre-existing `chat_pty_e2e` session/resume harness failures that
reproduce unchanged on the 0.8.12 source baseline.

## Upgrade procedure

1. Back up the deployed binary and active configuration.
2. Build `prx` in release mode from the signed-off `main` commit.
3. Install through a same-filesystem temporary path and atomically replace the
   active binary.
4. Restart `prx.service`, run the baseline migration, and verify runtime doctor
   and compliance attestation output.
5. Exercise Kimi K3 catalog, text, streaming, and tool use with the deployed
   binary, including an interactive tmux chat acceptance run.
