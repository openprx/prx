# rf contract: capability tokens

PRX advertises what it supports to rf as capability tokens in
`GET /api/capabilities` and in the `capabilities` field of
`GET /api/agents/catalog`. rf gates each feature on its token. The token names
below are frozen: a name is never renamed or reused for another meaning.

Rule: a token is advertised only when its whole earliest usable form is on
PRX main. A partial implementation is not advertised.

## Routes

Both routes use the same bearer authentication as the other `/api` routes and
keep the shared `401 {"error":"Unauthorized"}` and `429` bodies. Both are
reads: they change no configuration, write no store and are not gated by
autonomy. No body carries a credential, a key id or a URL with userinfo.

- `GET /api/capabilities` returns `200 {"prx_version":"<semver>","capabilities":["<token>", ...]}`.
  The tokens are sorted ascending, with no duplicates.
- `GET /api/agents/catalog` returns `200` with exactly these top-level fields:
  - `prx_version`: the PRX package version.
  - `revision`: the lowercase hex SHA-256 of the canonical JSON of the body
    without `revision`. It changes whenever any other field changes, including
    a credential becoming usable.
  - `capabilities`: the same list as `GET /api/capabilities`.
  - `models`: `[{"provider","model","credentials_ready"}]`, the chat-capable
    rows of the model catalog, sorted by provider and then by model.
    `credentials_ready` is `true` when a usable credential resolves for the
    provider, or when the provider needs none. It is `false` when a credential
    is required and none is stored for that provider. The key is omitted when
    the lookup itself fails.
  - `tools`: `[{"name","read_only"}]`, Main's published tool surface without
    `cron` and `xin`, sorted by name.
  - `reserved_names`: agent names no named agent may take (`catalog`, `main`).
  - `defaults`: `{"autonomy"}`, Main's effective autonomy level (`readonly`,
    `supervised` or `full`).
  - `reserved_session_key_prefixes`: prefixes PRX mints itself (`console-`).
    Other PRX session keys contain `:`, so they cannot collide with an rf key,
    and the `rf-` prefix belongs to rf.

## Tokens

| Token | Meaning | Status |
|---|---|---|
| `capabilities_v1` | `GET /api/capabilities` returns the version and the sorted token list | advertised since 85ab41f7a |
| `agent_catalog_v1` | `GET /api/agents/catalog` returns the fields listed above | advertised since 85ab41f7a |
| `targeted_agents_v1` | A message turn can target a named agent, with a per-agent partitioned history that reads back with an agent label and filter | not yet |
| `agent_crud_v1` | `GET`, `PUT` and `DELETE /api/agents/{name}` with If-Match, a revision, an incarnation and `enabled` | not yet |
| `agent_main_readback_v1` | `GET /api/agents/main` reads back Main's configuration | not yet |
| `coded_errors_v1` | Every error on the contract routes uses the `{"error":{"code"}}` body | not yet |
| `owner_id_stable_v1` | The owner id stays the same for one install across bearer-token rotation | not yet |
| `session_key_v1` | `POST /api/sessions` accepts a caller-chosen `session_key` | not yet |
| `message_target_v1` | The message route accepts a target together with caller context | not yet |
| `message_context_delivery_v1` | Caller context is delivered to the turn within published bounds, with `deliver` | not yet |
| `message_idempotency_v1` | The message route deduplicates by an idempotency key | not yet |
| `message_delete_v1` | A message can be deleted | not yet |
| `sync_snapshot_v1` | `GET /api/sync/snapshot` returns the owner's sessions and turns | not yet |
| `sync_ws_v1` | `GET /api/sync/ws` carries the durable journal kinds with hello, welcome, ack and cursor resume | not yet |
| `sync_root_links_v1` | Child runs are linked to their root session in the sync journal | not yet |
| `stream_frames_v1` | Turns stream `stream` frames with `delta` and `tool_status`, a per-run index and `stream_gap` | not yet |
| `run_approvals_v1` | Pending tool approvals can be approved and continued, including in named and child runs | not yet |
| `history_erase_v1` | An agent's history can be erased through the erase route, which returns a receipt | not yet |
| `agent_memory_purge_v1` | A deleted named agent's memory is purged | not yet |
| `host_bootstrap_v1` | A host can install and start PRX non-interactively (`token issue --out`, `runtime.json`) | not yet |
| `release_manifest_v1` | Releases carry a signed manifest that rf can verify | not yet |

An "advertised since" commit is the one that added the token to
`CAPABILITY_TOKENS` in `src/gateway/api/capabilities.rs` (branch commit; if a
rebase onto main changes it, `git log -S agent_catalog_v1 -- src/gateway/api/capabilities.rs`
finds the landed one). When a later card makes another token advertisable,
that card adds it to `CAPABILITY_TOKENS` and updates its status here in the
same change.
