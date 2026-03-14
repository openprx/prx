# PRX Online Path Audit Report (2026-03-14)

Scope:
1. Signal/Wacli receive->agent loop->provider->send full path
2. openai-codex decode long-tail + timeout/retry classification
3. config.toml + config.d merge/priority/hot-reload/watch/logging
4. send path consistency (`is_native`, localhost fallback)
5. storm guard false-positive risk

## Executive Summary
- Critical: 2 (both fixed in this audit)
- High: 2 (operational risk; no-code mitigation available)
- Medium: 1

---

## Findings

### 1) Critical (Fixed): Runtime hot-reload ignored `config.d` changes and merge semantics
- File + function:
  - `src/channels/mod.rs`
  - `load_runtime_defaults_from_config_file`
  - `config_file_stamp`
  - `maybe_apply_runtime_config_update`
- Repro condition:
  - Daemon/channels running, edit only `config.d/*.toml` (e.g. `default_model`/`default_provider`/temperature).
  - Send a new Signal/Wacli message; runtime route does not reflect fragment changes.
- Root cause:
  - Runtime reload path parsed `config.toml` directly (no `read_merged_toml` pipeline).
  - Update trigger stamp only used `config.toml` metadata (`mtime/len`), so fragment-only edits never triggered reload.
- Minimal fix:
  - Use merged config load path (`Config::load_from_path`) in runtime reload.
  - Replace mtime/len stamp with full layered fingerprint (`config.toml + config.d/*.toml`).
- Status:
  - Fixed.

### 2) Critical (Fixed): `openai-codex` stream could timeout after valid completion (no `[DONE]` long-tail)
- File + function:
  - `src/providers/openai_codex.rs`
  - `decode_responses_body`
- Repro condition:
  - SSE stream returns `response.completed`/`response.done`, but server keeps connection open and does not emit `data: [DONE]` promptly.
  - Provider eventually throws `provider_response_timeout kind=stream_idle_timeout`.
- Root cause:
  - Stream loop only terminated on `[DONE]`; terminal response events were not treated as end-of-stream.
- Minimal fix:
  - Detect terminal event types (`response.completed`, `response.done`, `response.failed`, `error`) and end read loop early.
- Status:
  - Fixed.

### 3) High: Signal storm guard can drop legitimate rapid messages
- File + function:
  - `src/channels/signal.rs`
  - `guard_allow_user_event` / storm settings application in `process_envelope`
- Repro condition:
  - Same sender sends 2 valid user messages within `min_reply_interval_secs` (default 2s), or similar content within dedupe TTL (default 60s).
  - 2nd message is silently filtered before entering agent loop.
- Root cause:
  - Guard applies strict per-target interval + dedupe at ingress, not just bot-loop events.
- Minimal fix:
  - Make interval guard optional by default for DMs, or only enforce for non-text/system events.
  - Add explicit log level + metrics for dropped user events.
- Status:
  - Not code-fixed in this patch (see no-code mitigation below).

### 4) High: `dm_policy=pairing` currently equals hard deny for direct messages
- File + function:
  - `src/channels/mod.rs`
  - `evaluate_inbound_policy`
- Repro condition:
  - Channel set to `dm_policy = "pairing"` for Signal/WhatsApp.
  - All direct messages dropped with warning.
- Root cause:
  - Policy enum includes `Pairing`, but implementation is placeholder-deny.
- Minimal fix:
  - Implement pairing state machine or reject startup when pairing policy is configured without feature support.
- Status:
  - Not code-fixed in this patch.

### 5) Medium: Channel doctor path does not reflect native Signal runtime behavior
- File + function:
  - `src/channels/mod.rs`
  - doctor channel construction branch around `SignalChannel::new_with_storm_protection`
- Repro condition:
  - Signal configured `mode = "native"`; doctor creates non-native `SignalChannel` and probes external HTTP semantics.
- Root cause:
  - Doctor path always uses HTTP channel constructor without native daemon lifecycle.
- Minimal fix:
  - Use `SignalNativeChannel` when `is_native_mode()` in doctor checks.
- Status:
  - Not code-fixed in this patch.

---

## Immediate stop-gap (No code changes)
1. For reply-drop incidents, set storm guard to permissive temporarily in config:
   - `dedupe_ttl_secs = 0`
   - `min_reply_interval_secs = 0`
   - keep `abnormal_threshold`/breaker for non-user storms.
2. Avoid `dm_policy = "pairing"` in production until pairing flow is implemented; use `allowlist` or `open` with explicit allowlist governance.
3. For openai-codex long-tail timeout windows, temporarily increase `ZEROCLAW_CODEX_STREAM_IDLE_TIMEOUT_SECS` (e.g. 90) until all nodes deploy patched build.
4. After changing `config.d`, force process restart on old builds (pre-fix), because runtime hot-reload may not detect fragment-only edits.

## Code fix plan (Committed in this audit)
1. Runtime config reload path:
   - merged load via `Config::load_from_path`
   - layered fingerprint trigger using `compute_config_fingerprint`
2. openai-codex stream decoder:
   - terminal SSE event detection to prevent false idle timeout
3. Added regression tests:
   - fragment change updates fingerprint
   - runtime defaults load merged `config.toml + config.d`
   - codex terminal event detection without `[DONE]`

---

## Regression test checklist (must prove Vano can receive reply)

### A. Signal end-to-end (DM)
1. Precondition: Signal sender `Vano` in `allowed_from` (or wildcard), `dm_policy=allowlist|open`.
2. Send `Vano -> bot: "ping-1"`, expect one reply.
3. Send `Vano -> bot: "ping-2"` within 2s and after 2s:
   - with strict storm settings: verify drop behavior is observable in logs.
   - with stop-gap storm settings (interval/dedupe=0): must receive replies for both.
4. Validate logs include ingress, provider invocation, and send success/failure.

### B. Wacli end-to-end
1. `wacli` daemon emits `message.received` from `Vano` JID.
2. Verify `ChannelMessage` reaches dispatcher and `reply_target=chatJid`.
3. Confirm response is sent through same channel (`wacli.send`) and delivered.

### C. openai-codex long-tail decode
1. Simulate SSE payload with `response.completed` but no `[DONE]` and delayed socket close.
2. Ensure provider returns decoded text, not `stream_idle_timeout`.
3. Verify retry classifier still treats real timeout/network read failures as retryable.

### D. Config hot-reload merge
1. Start daemon, baseline default model from `config.toml`.
2. Edit only `config.d/*.toml` overriding `default_model`.
3. Send `Vano` message; route/model must switch without restart.
4. Verify log contains runtime config applied with new model/provider.

---

## Risk priority recommendation
1. P0: deploy fixed hot-reload + codex stream patches (already implemented here).
2. P1: tune/relax Signal storm user-message filters in production profile.
3. P1: block or implement `dm_policy=pairing` to eliminate silent deny misconfig.
4. P2: align doctor path with native Signal mode.

