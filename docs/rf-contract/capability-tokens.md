# rf contract: capability tokens

PRX advertises what it supports to rf as capability tokens in
`GET /api/capabilities` and in the `capabilities` field of
`GET /api/agents/catalog`. rf gates each feature on its token and ignores
tokens it does not know.

## Rules

1. A token never changes meaning. A breaking change gets a new token with the
   next suffix (`_v2`); rf needs no period in which both are served.
2. A token is advertised only when its whole earliest usable form is on PRX
   main. `capabilities` lists the tokens effective in this running daemon;
   `available` lists the tokens present in this build but switched off or
   pending a restart.
3. Every route that belongs to an advertised token returns the coded error
   shape `{"error":{"code":"<code>","message":"<text>"}}`.
4. PRX never adds a static path segment under `/api/agents/` that starts with
   `a_`, and never mints a session key that starts with `rf-`.

## General errors

These codes can answer any `/api` route, in the coded shape:

- `429 rate_limited` `{"error":{"code","message","retry_after_ms"}}`: the
  shared API rate limit. The response also carries `Retry-After` in seconds.
  Both give the limiter window (60 s) as an upper bound: retry after at most
  this long. Exception: `GET /api/skills/discover` reports an upstream limit
  in its own shape `{"code":"rate_limited","error":"<text>"}`.

## Routes: capabilities and catalog

Both routes use the same bearer check as the other `/api` routes and the
shared API rate limit (`429 rate_limited`, see General errors). Both are reads: they
change no configuration, write no store and are not gated by autonomy. No body
carries a credential, a key id or a URL with userinfo.

Errors on both routes use the coded shape:

- `401 auth_failed`: a missing or invalid bearer, or a credential in the query
  string (refused even beside a valid bearer).
- `405 method_not_allowed`: any method other than `GET` or `HEAD`.
- `400 field_invalid`: any query parameter (neither route takes one).
- `500 storage_unavailable`: the catalog could not be assembled.

`GET /api/capabilities` returns
`200 {"prx_version":"<semver>","commit":"<40 hex>","api":1,"capabilities":[...],"available":[...]}`.
`commit` is the full git SHA the binary was built from, or `unknown` when it
was built outside a git checkout without `PRX_BUILD_COMMIT`. Both lists are
sorted ascending, with no duplicates.

`GET /api/agents/catalog` returns `200` with exactly these top-level fields:

- `prx_version`: the PRX package version.
- `revision`: the **catalog revision**, the lowercase hex SHA-256 of the
  canonical JSON of the body without `revision`. It is not the config
  revision of the agent routes below, and sent as `If-Match` it never matches
  (`409 revision_conflict`). It changes whenever any other field changes, including a
  credential becoming usable; a change that shows in no field (for example a
  per-model parameter setting) leaves it unchanged.
- `capabilities`: the same effective list as `GET /api/capabilities`.
- `models`: `[{"provider","model","credentials_ready"}]`, the chat-capable
  rows of the model catalog, sorted by provider and then by model.
  `credentials_ready` is `true` when a chat turn on that provider and model
  would find a usable credential (through the provider's own selection, a
  `[[model_routes]]` entry or a named agent bound to it) or needs none, and
  `false` when it would fail for a missing credential. The key is omitted when
  the lookup itself fails.
- `tools`: `[{"name","read_only"}]`, Main's published tool surface without
  `cron` and `xin`, sorted by name.
- `reserved_names`: agent names no named agent may take (`catalog`, `main`).
- `defaults`: `{"autonomy"}`, Main's effective autonomy level (`readonly`,
  `supervised` or `full`).
- `reserved_session_key_prefixes`: prefixes PRX mints itself (`console-`).
  Other PRX session keys contain `:`, so they cannot collide with a caller key.
- `caller_reserved_session_key_prefixes`: prefixes reserved for callers, which
  PRX never mints (`rf-`).

## Routes: agents (`agent_crud_v1`, `agent_main_readback_v1`)

`agent_crud_v1` covers `GET /api/agents`, `GET`/`PUT`/`DELETE
/api/agents/{name}`, `GET`/`PUT /api/gateway/targeted-agents` and
`POST /api/config/reload`; `agent_main_readback_v1` covers
`GET /api/agents/main`. These routes are authorised by the owner bearer
alone: no autonomy level, Main's included, is consulted. They share the
shared API rate limit. No body carries a credential: an agent's `api_key` and
`credential_ref` are kept across a `PUT` and never returned, and provider
text is redacted as in the catalog.

**Revision.** `revision` on these routes is the **config revision**: the
lowercase hex SHA-256 over the PRX configuration files. Any configuration
change (another agent, a provider, the console, a hand edit) changes it. Read
it from `GET /api/agents` (present when the list is empty), from a readback or
from the last write. `PUT` and `DELETE` require `If-Match: <config revision>`;
a missing `If-Match` or `*` is `400 field_invalid`, any other value is
`409 revision_conflict` with `current_revision`.

**Readback** (`GET /api/agents/{name}`, the answer of `PUT`):
`{"name","config":{"provider","model","prompt"?,"autonomy","tools","enabled"},"revision","incarnation","status"}`.
`autonomy` is `readonly`, `supervised` or `full`; `tools` is `["*"]` (Main's
tools), `[]` (none) or tool names; `config.enabled` is exactly what was put;
`incarnation` is a UUID that never changes across `PUT`s; `status` is
`active` (enabled), `disabled` (`config.enabled` false) or `retired` (deleted,
purge pending). `prompt` is absent when PRX composes the prompt, and is
returned byte for byte as put otherwise (`""` included). A hand-written agent
without a level reads back Main's level, the one it runs at.

**Main** (`GET /api/agents/main`):
`{"name":"main","config":{"provider","model","autonomy","tools":["*"]},"revision","incarnation"}`
(no `enabled`, `prompt` or `status`).

| Route | Request | 2xx |
|---|---|---|
| `GET /api/agents` | | `200 {"revision","agents":[readback…]}`: every configured named agent, retired ones included, sorted by name; Main and names outside the grammar are not listed |
| `GET /api/agents/{name}` | | `200` readback; `main` gives Main's |
| `PUT /api/agents/{name}` | `If-Match`; body `{"provider","model","autonomy","tools","enabled","prompt"?}`, a full replacement; `?confirm_high_risk=true` only after a `428` | `201` readback when created, `200` when replaced. The incarnation is created at the first `PUT`, whatever the switch or `enabled` say |
| `DELETE /api/agents/{name}` | `If-Match` | `200 {"name","status":"retired","revision"}`. The agent stays configured, disabled, and takes no turn; a repeated `DELETE` gives the same body with the current revision. The purge is `agent_memory_purge_v1` |
| `GET /api/gateway/targeted-agents` | | `200 {"revision","configured","effective","restart_required"}` |
| `PUT /api/gateway/targeted-agents` | `If-Match`; `{"enabled":bool}` | `200`, same shape; the switch takes effect at the next daemon restart, so `restart_required` is `true` until then |
| `POST /api/config/reload` | | `200 {"success":true,"message":"<report>"}` |

**Field bounds** (enforced on `PUT`, `400` otherwise):

- name: `^[a-z][a-z0-9_-]{0,63}$` (1 to 64 bytes), not `main` or `catalog`,
  and nothing credential-shaped (`agent_invalid`);
- `provider`: a provider PRX can construct; `model`: non-empty
  (`field_invalid`);
- `prompt`: a string of at most 32,768 bytes (the config schema has no bound
  of its own); `null` is refused (`field_invalid`);
- `autonomy`: exactly `readonly`, `supervised` or `full` (`read_only` is
  refused, `field_invalid`);
- `tools`: `["*"]`, `[]`, or distinct names from the catalog's `tools`, so at
  most that many; a duplicate, `*` beside a name or an unknown name is
  `agent_tools_invalid`;
- any other field is `field_invalid`; `limits` is not supported yet
  (`agent_limits_v1`).

**Errors** (coded shape):

- `400 agent_invalid`: a name outside the grammar or reserved; `PUT` or
  `DELETE` on `main` or `catalog`. Checked first, before `If-Match` and the
  body; nothing is planned.
- `400 agent_tools_invalid`, `400 field_invalid`: as above; also a query
  parameter other than `confirm_high_risk=true|false` on `PUT`/`DELETE`.
- `401 auth_failed`: a missing or invalid bearer, or a credential in the
  query string.
- `404 agent_unknown`: no such agent (also `DELETE` of a name that never
  existed).
- `405 method_not_allowed`, with `Allow`.
- `409 revision_conflict` `{"error":{"code","message","current_revision"}}`;
  `409 agent_disabled`: a `PUT` on a retired name (use a new name);
  `409 agent_backend_unsupported`: the memory backend keeps no incarnation
  registry.
- `422 not_applied`: the change was refused at activation and nothing stays
  written.
- `428 confirmation_required` `{"error":{"code","message","revision"}}`: a
  high-risk change (for example a provider change on an agent that holds an
  inline key); resend with `?confirm_high_risk=true` and `If-Match` equal to
  `revision`. Nothing rf sends in normal use triggers it.
- `500 storage_unavailable`: the configuration, the registry or a reload
  failed.

## Tokens

| Token | Meaning | Status |
|---|---|---|
| `capabilities_v1` | `GET /api/capabilities` returns the version and the sorted token list | advertised since 85ab41f7a |
| `agent_catalog_v1` | `GET /api/agents/catalog` returns the fields listed above | advertised since 85ab41f7a |
| `targeted_agents_v1` | A message turn can target a named agent, with a per-agent partitioned history that reads back with an agent label and filter | not yet |
| `agent_crud_v1` | The agent routes above: `GET /api/agents`, `GET`, `PUT` and `DELETE /api/agents/{name}` with If-Match, the config revision, an incarnation and `status`; `GET`/`PUT /api/gateway/targeted-agents`; the owner-bearer-only `POST /api/config/reload`. `DELETE` retires the agent (the purge is `agent_memory_purge_v1`) | advertised since 39509eb97 |
| `agent_main_readback_v1` | `GET /api/agents/main` reads back Main's configuration | advertised since 39509eb97 |
| `agent_limits_v1` | `limits {max_concurrent_turns, max_cost_per_day_cents}` on an agent `PUT` (unset by default; card AC-LIM) | not yet |
| `owner_id_stable_v1` | The owner id stays the same for one install across bearer-token rotation | not yet |
| `session_key_v1` | `POST /api/sessions` accepts a caller-chosen `session_key` | not yet |
| `message_target_v1` | The message route accepts a target together with caller context | not yet |
| `message_context_delivery_v1` | Caller context is delivered to the turn within published bounds, with `deliver` | not yet |
| `message_idempotency_v1` | The message route deduplicates by an idempotency key, with the replay response and 7-day key retention; job cancel answers a coded `404 job_unknown` | not yet |
| `message_delete_v1` | A message can be deleted | not yet |
| `sync_snapshot_v1` | `GET /api/sync/snapshot` returns the owner's sessions and turns; an expired cursor gets `410 snapshot_expired` | not yet |
| `sync_ws_v1` | `GET /api/sync/ws` carries the durable journal kinds with hello, welcome, ack and cursor resume, the root and parent link fields, `turn.failed{prx_restart,retryable:true}` journaled at start, the config-changed startup check and every C§4 close | not yet |
| `stream_frames_v1` | Turns stream `stream` frames with `delta` and `tool_status`, a per-run index and `stream_gap` | not yet |
| `run_approvals_v1` | Pending tool approvals can be approved and continued, including child-run approvals keyed by the root session, with `cancelled` and `cause` | not yet |
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
