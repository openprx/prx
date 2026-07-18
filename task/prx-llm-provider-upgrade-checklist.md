# PRX LLM Provider and Model Upgrade Checklist

Audit date: 2026-07-18. Implementation branch:
`feat/llm-provider-upgrade`; merged to `main` by `5e80192a`, with the 0.8.13
release commit at `b549382c`. The distinction between source verification,
adapter tests, and credential-backed live acceptance is intentional.

## Runtime baseline

- [x] Active provider before upgrade: `kimi-code`
- [x] Active model before upgrade: `kimi-k2.7-code`
- [x] Active endpoint: `https://api.kimi.com/coding/v1`
- [x] Deployed binary: `prx 0.8.12`; SHA-256
  `4ad270f2bc9203b67719870d74b372318b38694891f0b4fddf28670bfe2090f4`
- [x] Source baseline: `8e0c0fed6cab3be10a0e89916dd107c78b4beaed`
- [x] Live Kimi catalog: the active encrypted auth profile resolves correctly
  and returns `k3`, `kimi-for-coding`, and `kimi-for-coding-highspeed`.

## Upgrade implementation

- [x] Kimi Code: official IDs are `k3`, `kimi-for-coding`, and
  `kimi-for-coding-highspeed`; stale K2.7/K2.6/K2.5 coding-endpoint strings are
  rejected locally. `GET /models` discovery uses active auth profiles.
- [x] OpenAI/Codex: current GPT-5.6 and Codex IDs, defaults, source links, and
  published token prices recorded.
- [x] Anthropic: current Claude IDs, Bedrock mappings, native tool/thinking
  contract, defaults, and published token prices recorded.
- [x] Gemini: current IDs/defaults recorded; native function declarations,
  function-call parsing, function-result round trip, image `inlineData`, and
  streaming tool schemas implemented and tested.
- [x] Qwen: current Qwen 3.7/3.6 IDs and China/international/US endpoints
  recorded.
- [x] OpenRouter: current curated aliases and public model discovery refreshed;
  public catalog probe returned 344 models.
- [x] xAI: Grok 4.20 reasoning/non-reasoning aliases and endpoint recorded.
- [x] GLM/Zhipu: global GLM-5 and China GLM-5.2 endpoints/contracts recorded.
- [x] AWS Bedrock: current Claude IDs and Converse/ConverseStream contract
  recorded.
- [x] GitHub Copilot: current catalog examples and OAuth refresh boundary
  recorded.
- [x] Ollama: local discovery plus non-stream, stream, tool call, and tool-result
  round trip live probes succeeded.
- [x] llama.cpp/vLLM: `/v1/models`, custom base URL discovery, and
  model-dependent OpenAI-compatible capability boundary recorded.
- [x] LiteLLM/Hugging Face/Compatible: LiteLLM completion and `/v1/models`
  routes matched, Hugging Face migrated to `router.huggingface.co/v1`, and
  custom model discovery enabled.

## Live acceptance matrix

| Provider | Catalog | Text | Stream | Tool round trip | Vision/reasoning | Result |
|---|---:|---:|---:|---:|---:|---|
| Kimi Code | 200 / 3 | 200 (`K3_OK`) | 200 (`K3_STREAM_OK`, tmux) | 200 (`K3_TOOL_OK`) | multimodal accepted; reasoning preserved by adapter | K3 live gate passed |
| Anthropic | 401 | blocked | blocked | blocked | blocked | configured key invalid |
| Moonshot API | 401 | blocked | blocked | blocked | blocked | configured key invalid |
| OpenRouter | 200 / 344 | not configured | not configured | not configured | not configured | public catalog verified |
| Ollama | 200 / 7 | 200 | 200, terminal chunk | 200 | vision 200 | local runtime verified |
| llama.cpp | unreachable | blocked | blocked | blocked | blocked | no server on `:8080` |
| vLLM | unreachable | blocked | blocked | blocked | blocked | no server on `:8000` |
| LiteLLM | unreachable | blocked | blocked | blocked | blocked | no proxy on `:4000` |
| Other hosted providers | no credential | blocked | blocked | blocked | blocked | add provider credential to accept live |

Probe artifacts are under `/tmp/prx-provider-probe.xCk2pp` for this host run;
they contain response bodies but no emitted credential values. Its raw Kimi
401 used the encrypted-at-rest profile field and is superseded by the PRX
credential resolver probe (200 / three models) and successful K3 requests.

## Kimi K3 gate

- [x] Stable K3 ID is now published in official Kimi Code documentation.
- [x] K3 added to validation, catalog discovery, and onboarding default after
  live acceptance.
- [x] Catalog, text, streaming, tool-result, and multimodal-acceptance probes
  completed; streaming proof is retained in tmux `demo:provider-upgrade`.
- [x] K3 tool call and result round trip passed before enabling the onboarding
  default for agentic sessions.

## Release gate

- [x] Focused tests pass: Gemini 42/42, onboarding 52/52 (2 ignored), plus
  credential-resolution, Kimi validation, and subscription-cost tests.
- [x] `cargo fmt --all -- --check`
- [x] `cargo check -p openprx --all-features`
- [x] `cargo clippy --workspace --all-features --all-targets -- -D warnings`
- [ ] Full workspace tests: provider branch and untouched source baseline both
  produce the same 7 pre-existing `tests/chat_pty_e2e.rs` session/resume
  harness failures (24 pass, 7 fail, 1 ignored); all 5,776 library tests pass
  serially (7 ignored). This is not introduced by the provider upgrade.
- [x] Deployment. Release binary `prx 0.8.13` was built from `b549382c` and
  installed at `/home/ck/.cargo/bin/prx`; SHA-256
  `65cf35729da8c8af58cbf2cfff4feac4e20092cc3a4cf6e6f66ddade34ed4ab1`.
  `prx.service` restarted healthy with K3 as its active model.
- [x] Deployed runtime regression: doctor returned 20 ok / 2 warnings / 0
  errors; runtime doctor returned 16 ok / 3 warnings / 0 errors and verified
  the authoritative SQLite ledger at 15 migrations; live Kimi catalog returned
  the three official IDs; deployed text and tool probes returned
  `DEPLOY_K3_OK` and `DEPLOY_TOOL_OK`.
- [x] Deployed interactive acceptance: `/home/ck/.cargo/bin/prx chat --plain -p
  kimi-code -m k3` returned `DEPLOY_TMUX_STREAM_OK`; proof remains capturable in
  tmux `demo:provider-deploy`.
- [x] Rollback artifacts retained at
  `/home/ck/.cargo/bin/prx.rollback-0.8.12-20260718-provider-upgrade` and
  `/home/ck/.openprx/config.toml.rollback-0.8.12-20260718-provider-upgrade`.
- [x] Migration contract checked. `prx migrate baseline` is intentionally
  disabled because the legacy synthetic ledger is read-only evidence; backend
  startup owns migration, and `doctor runtime` verified the authoritative
  ledger after restart.
