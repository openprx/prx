# Receipt: UX Round 2 Batch A

Date: 2026-07-06
Scope: Batch A (O1 ollama num_ctx + O2 empty assistant turn)
Push: not pushed

## Commits

- `d09a7124 fix(provider): send num_ctx for ollama requests`
- `c0c1ef67 fix(chat): surface empty assistant responses`

## Changes

### O1 ollama num_ctx

- Added `[providers.ollama] num_ctx` config support.
- Routed explicit Ollama `num_ctx` through provider runtime options.
- Added router-model fallback from `RouterModelConfig.max_context` for matching Ollama models.
- Defaulted Ollama request `options.num_ctx` to `8192`.
- Added debug log when applying the resolved `num_ctx`.
- Replaced repeated provider-runtime option literals with `provider_runtime_options_from_config` so Ollama runtime config reaches every provider-construction path.

### O2 empty assistant turn

- Added shared empty assistant response guard: `trim().is_empty()` and no tool calls.
- Legacy tool loop now suppresses empty assistant history turns, including reasoning-only responses.
- Redux streaming driver now emits a visible `SystemMessageAdded` with `model returned empty response`, skips `RecordAssistantTurn`, and completes the stream with empty visible text/reasoning to clear draft state.
- `chat::run` legacy and Redux-driver completion paths now avoid mirroring/persisting empty assistant turns while preserving the user turn and provider usage/accounting where applicable.
- Tool-call turns with empty content remain valid because the empty-turn guard is only applied when no tool calls are present.

## Verification

Ran after both commits, before writing this receipt, with tracked diff clean:

```text
git diff --quiet && git diff --cached --quiet
cargo test --bin prx request_num_ctx_ -- --nocapture
cargo test --bin prx ollama_provider_num_ctx_deserializes -- --nocapture
cargo test --bin prx empty_assistant_response_writes_no_assistant_history -- --nocapture
cargo test --bin prx real_mode_empty_stream_surfaces_system_message_without_assistant_record -- --nocapture
cargo check --workspace --no-default-features
```

Results:

- `request_num_ctx_`: 3 passed
- `ollama_provider_num_ctx_deserializes`: 1 passed
- `empty_assistant_response_writes_no_assistant_history`: 1 passed
- `real_mode_empty_stream_surfaces_system_message_without_assistant_record`: 1 passed
- `cargo check --workspace --no-default-features`: passed

## Status

Batch A complete. Ready to continue Batch B unless a fix-round arrives.
