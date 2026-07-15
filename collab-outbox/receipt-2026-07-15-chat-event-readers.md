# Receipt: Step 5.2 first production event readers

Date: 2026-07-15
Branch: `feat/chat-event-readers`
Worktree: `/opt/worker/wt/prx-chat-event-readers`
Baseline: `be87b24008d3cb4f3f77d2ebe2d63c43caf93d1b`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Added a production MessageEvent projection to every saved-chat resume path:
  startup `--session last`, startup `--session <id>`, `/resume last`, and the
  shared `/resume <id>`/saved-session-picker switch path.
- The reader uses the backend's session-key-filtered replay query, which unions
  the stable canonical `chat:terminal:local-user:{id}` key with the legacy
  `chat:{id}` key before applying a bounded 500-event window. Unrelated
  workspace traffic cannot evict the selected session from that window.
- `message.created` user/assistant events project ordered turn role/content.
  `provider.final_outcome` payloads project metered usage and configured costs.
- Each dimension switches to its MessageEvent projection only when it is
  exactly equal to the existing blob snapshot. Missing, truncated, malformed,
  unsupported, or non-equivalent event data leaves that blob dimension intact.
- Blob-only timestamp and tool-call-summary metadata remains in the
  compatibility snapshot because MessageEvent cannot yet reproduce it. The
  event-projected role/content values are overlaid only after equality is
  proven, so JSON/Markdown export shape remains stable.
- `/export` and `/cost` continue to consume the in-memory `ChatSession`; after
  any saved-session resume that session is now event-projected behind the
  parity gate. Live unsaved turns remain the current-process authority.
- The existing `chat_session:{id}` blob write/read contract was not removed or
  weakened. Blob corruption and storage errors still fail closed; only an
  unavailable event reader degrades to the valid compatibility snapshot.

## Red-first and parity evidence

Before the implementation, the focused parity test failed to compile because
`project_chat_session_from_message_events` and
`load_session_by_id_with_message_events` did not exist.

The completed SQLite-backed test writes canonical user/assistant events,
legacy-key provider outcome events, a valid blob, and unrelated workspace
noise. It proves both turn and usage projections were selected, while the
serialized session used by JSON export, the `/cost` text, and the resumed
session remain byte-for-byte equal to their pre-projection values.

Two negative tests prove mismatched event content and a backend without an
event-log reader preserve the blob unchanged. The existing backend regression
test proves external-session traffic does not evict the selected session's
bounded replay query.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed on the final tree with no
  reported warnings.
- `cargo test -p openprx --lib chat_message_event_projection_tests` - 3 passed,
  0 failed, 5,630 filtered out.
- `cargo test -p openprx --lib load_recent_session_context_is_not_evicted_by_external_events`
  - 1 passed, 0 failed, 5,632 filtered out.
- `cargo test -p openprx --lib session_load_error_semantics_tests` - 11 passed,
  0 failed, 5,622 filtered out.
- `cargo test -p openprx --lib session_runtime_binding_tests` - 2 passed, 0
  failed, 5,631 filtered out.
- `cargo test -p openprx --lib slash_cost_` - 5 passed, 0 failed, 5,628
  filtered out.
- `cargo test -p openprx --lib export_` - 11 passed, 0 failed, 5,622 filtered
  out. This filter also covers four non-Chat export tests; the five Chat export
  tests and the Step 5.2 parity test all ran.
- `git diff --check` - passed.

Per `verification-policy.md`, strict clippy, the full workspace/integration
suite, architecture guards, dependency/security audits, independent review,
live Postgres conformance, and a release build were not run. They remain
GitHub delivery gates, not local Step 5.2 gates.

## Scope and rollback

- Scope: Chat saved-session event projection, all production resume call sites,
  colocated parity/fallback tests, and this receipt.
- Final pre-receipt implementation/test diff: 381 insertions, 34 deletions.
- Final pre-receipt diff SHA-256:
  `e0efc0c76c10740ef28fb7b42e04b5450ba9c1866b837b7b15d52fc2a227a142`.
- Rollback: revert the local Step 5.2 commit before Step 5.3 is based on it.
- No push, merge, deploy, binary install, service operation, process restart,
  network listener, active configuration mutation, external database mutation,
  GitHub action, release, or runtime activation was performed.
