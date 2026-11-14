# Advisory decision engine

The decision engine projects validated prediction answers into a bounded advice
brief and records final turn outcomes. It never selects tools, grants approval,
or changes admission. Tool selection belongs to the tool surface owner,
approval to the run's execution policy and run-keyed approvals, and admission
to the ordinary turn and child gates described in
[Turn gates and evidence](#turn-gates-and-evidence). There is no separate
decision plane and no `[causal_tree]` table any more.

## Modes and defaults

The engine is **off by default** (`enabled = false`). Off means no analysis,
brief, telemetry store open, or outcome writes. To disable it again, set
`[decision_engine] enabled = false` and restart the runtime.

When enabled, `mode` accepts:

- `pre` (default): append a bounded brief to the system prompt; no outcome store.
- `post`: record the final outcome; no brief.
- `both`: append the brief and link the outcome to its analysis decision ID.

`off` is the status label for `enabled = false`, not a `mode` configuration value.

## Configuration

```toml
[decision_engine]
enabled = false
mode = "pre"
prompt_budget_tokens = 300
candidates = 5
timeout_ms = 1500
telemetry_cap = 1000
observe_queue = 64
# Turn limits (formerly under [causal_tree]; same defaults and bounds)
latency_budget_ms = 500
token_ratio_limit = 0.35
max_candidates = 12
```

Analysis waits at most `timeout_ms` (100–30000 ms), and cancellation, errors or
timeouts yield no brief with one warning. Tool execution retains its normal
gates. The prompt budget is 50–2000 tokens and candidates is 1–20.

The turn limits apply whether or not the engine is enabled:

- `latency_budget_ms` (50–5000) is the extra latency one turn may spend on
  advisory work, and the ceiling for the backend transport timeout;
- `token_ratio_limit` (a finite number greater than 0) caps extra tokens as a
  ratio of the turn's baseline tokens;
- `max_candidates` (1–32) caps the candidates and mentions a turn considers.

`[decision_engine.backend]` is optional. When it is absent, the backend's own
defaults apply (`kind = "auto"`, `remote_enabled = false`), which is the same
backend a fresh `prx init` writes. Its keys are `kind` (`auto`, `builtin`,
`compatible_http`, `typesafe`, `openrouter`), `remote_enabled`, `endpoint`,
`model`, `pinned_snapshot`, `credential_ref`, `timeout_ms`,
`max_request_bytes`, `max_response_bytes` and `circuit_breaker`
(`failure_threshold`, `cooldown_secs`). Decision state leaves the machine only
when `remote_enabled = true`.

A configuration that still contains a `[causal_tree]` table (or any of its
keys) is refused at load with the ordinary unknown-configuration-path error.
Nothing imports, warns about or strips it: remove the table and, if needed,
set the three turn limits under `[decision_engine]`.

The advisory driver (`runtime/decision_advisory.rs`) is the only caller of the
prediction path. Every turn opens without it; the analysis runs afterwards
within `timeout_ms` and is charged to the turn's own accounting scope. When
the remaining tokens or money do not cover the analysis worst case plus the
first send's minimum admission, the analysis is skipped; the request is never
refused because of the engine. A brief that does not fit the first send's
token budget is dropped. Builtin and cache hits spend nothing.

## Telemetry and diagnostics

Post/both opens `decision-engine.sqlite` in the workspace lazily on the first
outcome. The store is fresh-schema only: an older schema version is dropped and
recreated, because telemetry carries no settlement or authority. One dedicated
writer inserts and evicts oldest records atomically to `telemetry_cap`
(0–100000). Zero disables the store. `observe_queue` (1–1024) bounds
admission; a full or closed queue drops the observation and increments the
existing dropped counter without blocking execution. Store failures warn and
increment the store-error counter. Shutdown closes admission and drains
accepted outcomes until its deadline; late advisory telemetry warns without
failing catalog retirement.

Gateway status and doctor report enabled/mode, configured backend kind, the
effective cap (always 0 in `pre` mode, which records no outcomes),
queue depth, dropped count and store errors, without endpoints or credentials.
Counters cover live engine queues in the current process and workspace; a
separate doctor process reports zero runtime counters, not daemon totals.
Queue depth excludes the write currently in progress.

## Management interfaces

- **CLI.** `prx decision-engine status` prints the status above.
  `prx decision-engine backends` prints the effective backend and the builtin
  (`kind`, `locality`, `model`, `pinned_snapshot`, `credential_configured`,
  `circuit`) and never a secret. `prx decision-engine config set <key> <value>`
  accepts only `enabled`, `mode`, `prompt_budget_tokens`, `candidates`,
  `timeout_ms`, `telemetry_cap`, `observe_queue`, `latency_budget_ms`,
  `token_ratio_limit`, `max_candidates` and the `backend.*` keys above except
  `credential_ref`. Any other key and any credential-bearing key is refused
  with a typed error and a non-zero exit.
- **Gateway.** `GET /api/decision-engine/status`, `GET …/backends`,
  `GET …/analyses?limit=N` and `GET …/outcomes?limit=N` (`N` absent means 20;
  otherwise 1–100), `POST …/config/plan` and `POST …/config/apply` with
  `{"changes": {key: value}}` over the same allowlist, and
  `PUT …/credential`, which is write-only, high risk, bound to `If-Match` and
  answers `204`. A bad `limit`, an unknown or secret key, or a malformed body
  answers `400 {"error":"invalid_request","field":…}`. No response carries
  user input text, backend free text or a secret; briefs are shown only in
  their redacted form.
- **Console.** The **Intent analysis** page shows the status, the effective
  backend and the last 20 analyses (redacted briefs), and can replace the
  backend credential through the write-only route.

### Retired interfaces

`prx causal …` prints exactly one deprecation line pointing at `prx decision-engine` and exits 0. Every former causal route (`/api/causal/status`, `/api/causal/backends`, `/api/causal/test`, `/api/causal/decisions`, `/api/causal/decisions/{id}`, `/api/causal/reports`, `/api/causal/reports/saved`, `/api/causal/config/plans`, `/api/causal/config/apply`, `/api/causal/credential`) answers `410 Gone` with `{"error":"gone","see":"/api/decision-engine"}`.

The approval resume route (`POST /api/sessions/{id}/approvals/{decision_id}/resume`)
and the worker relaunch path are deleted. An approved call is not re-executed
from a saved snapshot; the run that asked continues under its own approval
strategy.

## Turn gates and evidence

### Approvals

Approvals are run-keyed. Run-keyed approvals (the Console inbox
`GET /api/sessions/approvals` and `GET`/`POST`/`DELETE
/api/sessions/{id}/approvals/{decision_id}`) are available when user-message
recording is on and the memory backend has a recovery repository (SQLite, or
Lucid's local SQLite store); otherwise the inbox answers `409`. Approval
records are sealed under the runtime key. `Full` autonomy needs no approval,
`ReadOnly` refuses, and `Supervised` asks the run's resolver.

### Terminal evidence and restart recovery

With a recovery repository and recording on, every turn terminal is recorded
through the repository: the settled receipt, the persisted claim semantic and
the cost outbox row. After a restart, a recorded terminal verifies for every
status; a terminal that fails verification is reported not replayable and is
neither re-executed nor settled again. Without a repository, or with recording
off, a terminal is unrecorded: it is settled directly, cannot be replayed and
is never refused. Interrupted turns are recovered only on repository backends.

Cost is never lost. Every committed metered terminal has one outbox row, which
the inline path or the process-level cost projector projects exactly once. The
projector runs at startup, then every 60 seconds while retryable rows exist,
backing off to 15 minutes, and again after an inline projection failure.

### Child sessions and delegation

A child session equals the main session. Every child is issued through one
ordinary (or operator) path from its authenticated parent. Recursive
delegation is on by default: omitting `allow_delegation` on `sessions_spawn`
gives a child that can spawn its own children, bounded only by the sealed
spawn depth, the funding carve from the parent's allowance and the integrity
checks. Where the bound leaves no room, an omitted flag gives a child that
cannot spawn; an explicit `true` is then refused, and an explicit `false`
always gives a non-delegating child. There are no recursive-delegation
or depth keys to configure.

A task-mode child runs under the parent run's approval strategy: a Supervised
child's `Ask` reaches the same resolver as the root, Full needs no approval and
ReadOnly refuses. Process-mode children keep the worker approval path.

Worker children (process, batch and operator process) are issued and verified
under a key derived from the strict session-worker transport secret: the
`SESSION_WORKER_SECRET` environment override, which must be exactly 32 bytes,
or one persisted 32-byte file (`OPENPRX_SESSION_WORKER_SECRET_PATH`, else
`<config_dir>/keys/session_worker.secret`) that PRX creates once with mode
0600 and never rewrites. A missing, malformed, symlinked or widened secret
refuses the launch; there is no fallback key. A worker manifest from before
this release fails to start. In-process `delegate` and task-mode children
persist nothing and are not durable across a restart.

### Runtime key and legacy files

The runtime key lives at `<workspace>/self/runtime/runtime.key`. It seals
approval records and turn state. Nothing opens `self/causal_tree/decisions.db`,
`self/causal_tree/.decision_key` or `self/child_authority/journal.db`; when any
of them exists, the process logs one warning
(`legacy decision files present under <ws>/self (ignored; start a fresh
workspace to remove them)`) and continues.

**Fresh workspace.** This release carries no decision-plane data over. To
start clean, stop PRX and either point it at a new workspace or delete
`<ws>/self/causal_tree/`, `<ws>/self/child_authority/` and `<ws>/memory/`.
Keeping an old workspace is harmless, with three consequences: interrupted
turns from before the upgrade are not recovered, pending approvals from before
the upgrade cannot be resumed, and workers launched by an older build fail to
start.

### Memory-backend support

Durable event storage and selected evidence reads are separate contracts. A
backend that records `message_events` is not thereby able to serve bounded
Memory, Document or Session reads for a turn.

| Backend | Memory / Document / Session reads | Recovery repository |
| --- | --- | --- |
| SQLite | Implemented | Yes |
| Lucid | Delegates to its local SQLite store | Yes (local SQLite) |
| PostgreSQL | Dedicated read-only ports | No |
| Markdown / None | Unsupported; explicit error | No |

Unsupported reads fail explicitly; they never fall back to semantic recall,
remote Lucid operations or unscoped reads.

## System-prompt cache effect

The brief sits inside the system prompt. With pre/both enabled, a changing
brief can miss or rewrite Anthropic's cached system prefix on each turn.
Use the existing prompt-cache metrics to assess this cost before enabling it
broadly. Post mode leaves the system prompt unchanged.
