# Step 1.2 Chat authoritative persistence sanitization receipt

Date: 2026-07-12  
Worktree: `/opt/worker/wt/prx-chat-persistence-sanitization`  
Baseline HEAD: `aecd265d`

## Scope

- One shared session content policy sanitizes title, turn content, tool
  `args_preview`, and background-session title/summary.
- Authoritative boundaries: reducer `Effect::SaveSession`, defensive
  `save_session`, user/assistant `MessageEvent`, and `/export` JSON/Markdown.
- Raw in-memory Chat mirrors remain raw and do not claim persistence safety.
- No layout, Redux migration, resume rewrite, commit, push, or deploy.

## Red baseline

The Chat modules are compiled into the `prx` binary, so valid focused evidence
uses `--bin prx`. An isolated target directory avoided same-package worktree
artifact collisions.

```text
CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-sanitize-target TMPDIR=/opt/worker/tmp \
  cargo test --bin prx save_session_effect_redacts_all_authoritative_content_fields -- --nocapture
FAILED (exit 101): authoritative SaveSession blob leaked AWS key

CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-sanitize-target TMPDIR=/opt/worker/tmp \
  cargo test --bin prx save_and_message_event_boundaries_redact_aws_keys -- --nocapture
FAILED (exit 101): stored Memory content leaked AWS key

CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-sanitize-target TMPDIR=/opt/worker/tmp \
  cargo test --bin prx export_redacts_session_content_without_losing_unicode_or_tool_shape -- --nocapture
FAILED (exit 101): export JSON contained the AWS key
```

## Implementation

- `src/chat/sanitize.rs`: `sanitize_session_content` clones the schema and
  sanitizes every policy-governed content field while retaining Unicode and
  structured metadata.
- `src/chat/state.rs`: `build_session_snapshot` returns the safe projection.
- `src/chat/mod.rs`: `save_session` re-applies the policy defensively;
  MessageEvent helpers sanitize content; raw mirror pre-sanitization was
  removed.
- `src/chat/dispatcher.rs`: the real `Effect::SaveSession` store sink applies
  the same policy even when handed a manually constructed raw effect.
- `src/chat/commands.rs`: export operates on the safe projection.

## Green evidence

```text
cargo test --bin prx save_session_effect_redacts_all_authoritative_content_fields -- --nocapture
1 passed

cargo test --bin prx save_and_message_event_boundaries_redact_aws_keys -- --nocapture
1 passed; stored Memory/recall and both MessageEvent roles were safe

cargo test --bin prx export_redacts_session_content_without_losing_unicode_or_tool_shape -- --nocapture
1 passed; JSON/Markdown safe, Unicode and ToolCallSummary structure retained

cargo test --bin prx real_mode_save_session_triggers_memory_store -- --nocapture
1 passed; raw Effect sink stored no secret and recall exposed none

cargo test --bin prx chat_entrypoint_records_user_and_assistant_message_events -- --nocapture
1 passed; ordinary event content behavior retained

cargo check --bin prx
passed

cargo fmt --all -- --check
passed

git diff --check
passed
```

All commands used the isolated
`CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-sanitize-target` and
`TMPDIR=/opt/worker/tmp`.

## Review hardening

- Truncation marker bytes are included inside the 10 KiB field limit. The
  marker reports the redacted pre-truncation byte count, and repeated policy
  application is byte-for-byte idempotent.
- JSON tool args are parsed, recursively sanitized, and reserialized as valid
  JSON; numeric/object structure remains intact.
- Chat auto-promoted semantic memory applies the same text policy before the
  real Memory/FTS sink.
- Chat-only wrappers recursively sanitize route-decision and provider-outcome
  structures before calling the shared runtime event recorders. Stored
  `raw_payload_json` remains valid JSON.
- Plain text coverage includes `Authorization: Bearer`, quoted JSON passwords,
  `sk-proj-*`, and AWS access-key IDs.

```text
cargo test --bin prx sanitization_is_bounded_and_idempotent -- --nocapture
1 passed

cargo test --bin prx redacts_plain_and_recursive_json_secret_forms -- --nocapture
1 passed

cargo test --bin prx real_mode_save_session_triggers_memory_store -- --nocapture
1 passed; reducer-safe >10 KiB snapshot was sanitized again by dispatcher with one bounded marker, and raw Effect was safe

cargo test --bin prx chat_route_payloads_remain_valid_json_and_redact_nested_secrets -- --nocapture
1 passed; route detail and provider Authorization/AWS errors were absent from valid stored JSON

cargo test --bin prx export_redacts_session_content_without_losing_unicode_or_tool_shape -- --nocapture
1 passed; JSON args remained parseable with sensitive string replaced and numeric shape retained

cargo test --bin prx save_and_message_event_boundaries_redact_aws_keys -- --nocapture
1 passed; defensive save, MessageEvents, and real semantic-memory/recall path were safe
```

## Formal gate closure

All formal commands used
`CARGO_TARGET_DIR=/opt/worker/tmp/prx-chat-sanitize-target` and
`TMPDIR=/opt/worker/tmp` unless noted otherwise.

```text
cargo check --workspace --all-features
passed

cargo check --workspace --no-default-features
passed

cargo clippy --workspace --all-targets --all-features -- -D warnings
passed

cargo test --bin prx --all-features
5532 passed; 0 failed; 7 ignored

cargo test --locked --test architecture_boundaries
4 passed; 0 failed

cargo fmt --all -- --check
passed

git diff --check
passed
```

Two independent second-round reviews reported no High or Medium blockers.
No push, deploy, service restart, or live runtime mutation was performed.
