# Changelog

All notable changes to OpenPRX will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Decision-plane candidate under local development, **not accepted or released**.
The source default-on cutover is prepared for adoption with its init template;
this does not change an installed service or real configuration. See the
[candidate notes](docs/release-notes-decision-plane-candidate-2026-10-04.md)
for integration, validation and rollback limits.

### Changed

- The former internal `agent::loop_` module and its positional turn helpers are
  removed. Internal entrypoints now admit their source through their owning
  adapter and invoke the typed `agent::turn_engine::run_turn` API with
  `TurnRequest`, `TurnServices`, and `TurnDelivery`. There is no old-path
  compatibility facade. This is a Rust source-layout break only; it requires no
  stored-data or configuration migration. See the
  [source migration and module ownership notes](docs/agent-turn-engine-migration.md).

- `[causal_tree]` has a new schema (the decision plane). A configuration
  written by an earlier release is mapped on load and written in the new form
  on the next save, with `schema_version = 1`. Keys left at their old default
  take the new default. Other values move to their new home, for example
  `policy.max_branches` to `policy.max_candidates` and
  `policy.rehearsal_timeout_ms` to `rehearsal.timeout_ms`. Under D-073/D-074,
  versioned `schema_version = 1` plus `enabled = false` stays disabled, including
  an init-default sidecar that also records false. Only recognized unversioned
  legacy-default signatures, including the old bare false template, treat that
  false as unset; unversioned false with modern non-legacy settings stays
  explicit. Loading alone does not rewrite the file. The load
  refuses a legacy value that has no equivalent or does not fit its new key,
  naming the file and key and never the value. Each refusal, its exact
  error and the fix:
  - `w_confidence` other than `0.5`: `<file>: causal_tree.w_confidence holds a
    non-default value with no equivalent in this release; the single
    confidence weight is retired; set the weights in
    [causal_tree.scoring.weights] and remove the key`. Fix: remove the key and
    set `[causal_tree.scoring.weights]` if you need other weights.
  - `policy.default_side_effect_mode` other than `"ReadOnly"`: `... holds a
    non-default value with no equivalent in this release; rehearsal is
    read-only (CDP 9.3); remove the key`. Fix: remove the key.
  - A legacy key and its replacement both set: `... is set together with
    causal_tree.<new key> (in <file>), the key that replaces it; keep only
    causal_tree.<new key>`. Fix: delete the legacy key.
  - `policy.max_branches` above 255, or a negative or oversized
    `policy.rehearsal_timeout_ms` or `policy.circuit_breaker_*`: `... holds a
    value causal_tree.<new key>, the key that replaces it, cannot hold; set
    causal_tree.<new key> within its documented range and remove the key`.
  - A moved value outside the new range: `policy.max_branches` above 32,
    `policy.rehearsal_timeout_ms` above `policy.extra_latency_budget_ms`
    (500 unless set), `policy.circuit_breaker_threshold` above 100, or
    `policy.circuit_breaker_cooldown_secs` above 3600. These fail validation
    with `causal_tree.<new key> must be <range>`, for example
    `causal_tree.rehearsal.timeout_ms must be at most
    causal_tree.policy.extra_latency_budget_ms`. This happens even with
    `enabled = false`. Fix: set the new key within its documented range and
    the existing approved budget; this candidate does not relax performance
    or admission budgets.
  - A remote `https` decision endpoint now needs
    `causal_tree.backend.remote_enabled = true`. A literal loopback endpoint
    does not.

- Turns use source-bound decision authority, selected retrieval and constrained
  capabilities. Recovery retains authenticated input/decision/terminal evidence
  and owned backend work; interrupted or uncertain execution is not relabeled
  successful or automatically resent. Child replay separates logical lineage
  from the actual durable store namespace.
- The local Router candidate adds bounded KNN with a concrete local hash
  embedder, purpose-bound history reads and dependent model readmission. Learning
  uses actual serving and terminal evidence; newly constructed embedded Agents
  do not inherit an earlier session's statistics. New authenticated Router
  projections require readers that understand them.
- Separately billed embedding, premium AutoMix escalation and auxiliary Channel
  vision summaries remain blocked by `BudgetUnproven`. Vision summaries are
  unavailable in this candidate; ordinary chat authority does not permit that
  additional send. Full behavior, restart, performance and independent acceptance
  remain pending.

## [0.8.162] - 19 September 2026

### Fixed

- The MCP owner id is composed verbatim from issuer and subject, so every
  owner id, identity binding, idempotency scope and rate-budget key is
  byte-identical to earlier releases for every input (the 0.8.159 encoding
  of `:` and `%`, which would have moved the id of any issuer URL or SPIFFE
  subject and orphaned that caller's history, is withdrawn before release).
  The delimiter-collision class is closed at configuration load instead:
  `mcp_server.jwt_issuer` may not equal, or start with, a built-in origin
  name (`spiffe`, `mtls`, `anonymous`) followed by `:`.
- `document_get_chunk`, which is exposed over MCP by default, applies the
  same owner isolation as the other reads: an owner-isolated caller sees a
  chunk only when its owner id matches, an unattributed chunk is hidden from
  such a caller, and non-isolated callers are unchanged.
- The memory-draft lifecycle memory events now record their owner in both
  adapters, so draft rows created at session or private visibility are
  attributed from birth instead of being invisible to their own
  owner-isolated author. Document-ingest events record the owner as well;
  they still carry no session key, so a session- or private-visibility
  ingest event remains unreadable through the session arms (tracked
  separately).

## [0.8.161] - 19 September 2026

### Changed

- The architecture guard decides tool-execution exemptions by binding and call
  count, never by name: a field or accessor receiver (`self.conn`,
  `checkout.client_mut()?`) carries no binding and must be listed as a reviewed
  site; a producer expression is matched by its canonical shape and receiver
  rather than by substring; and every allowlisted direct invocation carries an
  exact occurrence count, so a second direct tool call added inside an
  allowlisted function, a stale entry, or a shadowed handle fails the guard.

## [0.8.160] - 19 September 2026

### Changed

- The channel test that proves a two-hour turn is never cut off by a wall clock
  now keeps that turn observably progressing while it waits, so it no longer
  contradicts the idle (no-progress) detector once turn silence is measured on
  the runtime clock. A silent two-hour turn is still terminated, as documented.

## [0.8.159] - 19 September 2026

### Fixed

- MCP session and private reads are isolated by owner. Two authenticated MCP
  identities that present the same subject under different issuers no longer
  share `session`- or `private`-visibility history through `sessions_list`,
  `sessions_history`, `session_status`, `sessions_send` or `document_search`.
  Externally authenticated envelopes are marked owner-isolated; under that mark
  the `session` and `private` arms of every principal-scoped read in the SQLite
  and Postgres adapters, the legacy-key read merge included, additionally
  require `owner_id` equality, and an isolated principal without an owner id
  reads no such row. `workspace` and `global` visibility keep their shared
  contract and in-process callers are unaffected. `memory_events` gains an
  `owner_id` column through an idempotent migration (SQLite schema v16,
  Postgres v22); existing rows are preserved, stay readable by non-isolated
  principals, and unattributed rows can be assigned to an owner with the
  bounded statements documented in `docs/tools.md`. (This entry originally
  also percent-encoded `:` and `%` in the issuer and subject components of
  the owner id; that change is withdrawn in 0.8.162 before any release.)

## [0.8.158] - 19 September 2026

### Fixed

- The idle (no-progress) hang detector measures a turn's silence on the tokio
  runtime clock (identical to `std::time::Instant` in a release build), so its
  regression tests can drive time deterministically instead of racing a loaded
  CI runner. The tool-execution idempotency ledger's reservation and terminal
  writes are recorded as progress once they return; a write that never returns
  still records nothing, so a wedged blocking pool is still terminated. No
  threshold, default or configuration bound changed.

## [0.8.157] - 19 September 2026

### Fixed

- The architecture guard no longer exempts a direct tool execution because of
  the name its receiver happens to have. Every `execute` call form is recorded,
  and a call is exempt only when the receiver's binding in the enclosing
  function proves a non-tool type or the site is listed with its file, function,
  receiver, exact call count and cited declaration. Shadowing a database handle
  with a `Tool`, an extra call at a listed site, or a stale entry now fails the
  guard.

## [0.8.156] - 19 September 2026

### Fixed

- The fabricated `[Used tools: ...]` marker cleaner and its corrective-retry
  detector now follow CommonMark fenced-code rules through one shared fence
  walker: a closing fence must use the opening fence's character, be at least
  as long and carry only trailing whitespace; a fence may be indented up to
  three spaces; a backtick fence's info string may not contain a backtick; an
  unterminated fence runs to the end of the text. Prose after a mixed fence is
  no longer mistaken for code (the marker is detected and stripped again), and a
  marker inside a longer fence is no longer deleted out of a code example.

## [0.8.155] - 18 September 2026

### Fixed

- A chat session that reports on an assignment the operator has already
  killed is now refused with `assignment_cancelled` (HTTP 409) carrying the
  cancellation summary, instead of being answered with the operator's result
  sequence as if its own report had been recorded. Genuine replays of the
  session's own report still return the original sequence.
- The mailbox pull samples the poll counter and the wall clock inside the
  critical section that mints the ordering revision, so a higher revision can
  never carry a lower poll count.
- The MCP rate limiter keeps each caller's window across configuration
  reloads; changing `mcp_server.rate_limit_rps` adopts the new budget in place
  instead of resetting every caller.
- Tests: the discovery-routing exclusion tests use a registry where discovery
  would expose an excluded tool, so they fail if exclusions stop outranking
  discovery; the gateway mailbox contention test restores the process store it
  swaps in.

## [0.8.154] - 18 September 2026

### Fixed

- The unspaced-script table behind discovery routing is expressed as code
  point ranges and its test fixtures use Thai text, so the repository guard
  that keeps CJK characters and escapes out of `src/` and `tests/` passes
  again. Routing behaviour is unchanged.

## [0.8.153] - 18 September 2026

### Added

- `POST /mcp/v1/tools/call` honours an `Idempotency-Key` header (trimmed,
  256-byte cap shared with `/webhook`), scoped per caller as
  `{prx_owner_id}:{key}` and attached to the durable tool idempotency ledger,
  so a transport retry with the same key executes once and replays the first
  response, a reused key with a different body is rejected, and two callers
  sharing a key string both execute. With a key but no ledger-capable memory
  backend, side-effecting tools fail closed with an explicit error while
  read-only tools proceed; without a key, behaviour is unchanged and retries
  are not deduplicated.
- `mcp_server.rate_limit_rps` is now enforced on `/mcp/v1/tools/call` as a
  per-caller one-second budget (`0` disables); over budget returns 429.

### Changed

- `tools/list` on the MCP endpoint reports the real effect (`readonly` or
  `act`) of each exposed tool instead of a hard-coded `readonly`, and config
  load warns when `mcp_server.exposed_tools` names a side-effecting tool.

## [0.8.152] - 18 September 2026

### Fixed

- An established chat session with a narrow pinned tool set can now discover a
  different capability on a later substantive request that the keyword table
  does not recognise (another language or an unrecognised paraphrase). Such an
  unrouted turn is classified structurally, by concrete objects (URLs, paths,
  flags, code spans) or request weight measured per script class, and earns a
  one-turn discovery exposure of the tools operator policy allows; the exposure
  is never absorbed into the session, so greetings, closings and one-word
  follow-ups still republish the pinned set. Channel exclusions, model
  allowlists and worker manifests are unchanged and still outrank discovery.

## [0.8.151] - 18 September 2026

### Changed

- Skills catalog tests pin both community skill roots (open-skills and
  OpenClaw) inside each fixture's own temporary directory instead of falling
  back to the home-directory defaults, so a machine holding real community
  checkouts no longer fails twenty unrelated tests. Default resolver behaviour
  is unchanged and still covered by its own tests. Test-only change.

## [0.8.150] - 18 September 2026

### Fixed

- `prx init --force` no longer hides a committed configuration reset when a
  later step fails. Post-commit work (defaults sidecar, workspace tree,
  scaffolding, permissions) is grouped after the commit; a failure there is
  recorded on the report, so the reset warning, the exact unique archive path
  and the preserved settings are still printed together with an `ERROR:` line,
  and the command exits non-zero naming both the reset and the failing step.
  A real CLI integration test covers the obstructed-workspace case.

## [0.8.149] - 18 September 2026

### Fixed

- The vector candidate window is now ordered by a monotonic `update_seq`
  column maintained by triggers instead of `rowid`, so a refreshed old memory
  stays eligible for vector recall when more rows than the cap match. The
  column is covered by a partial index (`provider, model, dimensions,
  update_seq` where an embedding exists) and existing databases migrate on
  open with the sequence backfilled from `rowid`, preserving their candidate
  set. The FTS update trigger now fires only when `key` or `content` change,
  which also stops metadata-only updates from rewriting the FTS index. The
  candidate cap policy is unchanged.

## [0.8.148] - 18 September 2026

### Fixed

- Chat assignment mailbox persistence is now ordered, idempotent and atomic.
  Every store write carries the revision it was planned at and stale flushes
  write nothing, so a slow pull can no longer overwrite a newer acknowledgement
  on disk or roll liveness counters back; existing databases gain the revision
  columns on open. Re-attaching the store merges the result feed by sequence
  instead of duplicating it. Completion and deregistration cascades retire the
  assignment and record its result in one SQLite transaction, and a replayed
  report answers with the original sequence. Gateway mailbox handlers run their
  SQLite work on the tracked blocking pool with one contention budget per
  request, so a locked database no longer stalls unrelated HTTP requests;
  degradation is visible through the pool statistics surface.

## [0.8.147] - 18 September 2026

### Fixed

- Fabricated `[Used tools: ...]` markers embedded inside a sentence are now
  handled with the same rules as whole-line markers: an assistant response
  carrying one without a real tool execution spends the corrective retry, and
  the marker is removed before streaming, history, compaction summaries,
  channel replay and TUI session resume. Fenced code blocks, inline code
  spans, other bracketed prose, user turns, native tool-call ids and reasoning
  boundaries are preserved.

## [0.8.146] - 18 September 2026

### Changed

- The architecture guard in `tests/architecture_boundaries.rs` now inventories
  every direct `Tool` execution form (`execute`, the cancellation variants,
  `execute_named*`, UFCS and fully qualified calls, multi-line chains) with a
  token-level scanner that blanks comments and string literals first. Every
  call site outside `ToolExecutionService` must appear in an explicit allowlist
  with its reason, and every gateway call site must document the gate chain
  that replaces the service (`GATEWAY_DIRECT_TOOL_INVOCATION_GATES`), which now
  records both `post_channel_send` and `post_config_reload`. No production
  code was rerouted.

## [0.8.145] - 18 September 2026

### Fixed

- Schema composition (`allOf`, `anyOf`, `oneOf`, `if`/`then`/`else`) is now
  evaluated for scalar and array values, not only objects. A field published as
  `{"allOf": [{"type": "integer", "minimum": 1}]}` previously compiled its
  member and never evaluated it, so invalid scalars and arrays reached the
  executor unchecked. Production execution-service tests prove such arguments
  are rejected before the backend runs; unfollowable references and cycles
  still compile to no constraints and are documented as such.

## [0.8.144] - 18 September 2026

### Changed

- Release publication is now gated on the exact tag SHA: a new `release-gate` job
  reads the newest completed `CI Run` and `Sec Audit` runs for that commit and
  fails closed when any required job is missing, skipped, failed, cancelled, or
  still running after a bounded wait. The decision logic lives in
  `.github/workflows/scripts/release_gate.py` with recorded fixtures and a local
  test runner (`scripts/ci/release_gate_tests.sh`). The gate job requests
  `actions: read`. See `docs/release-gate.md`.

## [0.8.143] - 17 September 2026

### Fixed

- A stored `[Used tools: ...]` marker no longer reaches the model through the
  quoting surfaces. 0.8.137 closed the paths a *newly written* marker takes, but
  the ~150 assistant rows that started the imitation were still quoted back into
  every prompt by `[Current session context]`, `[Recent shared workspace events]`,
  the replayed conversation turns and `[Document evidence]` -- and those builders
  flatten content to a single line, which turned a whole marker line into an
  in-line fragment the line-level detector could never see again. Assistant-role
  quoted content is now stripped before it is flattened, with a second,
  sentence-safe strip for markers that are already in-line
  (`strip_inline_fabricated_tool_markers`: only a `[Used tools: ...]` fragment
  that names at least one tool and closes on the same line is dropped; every
  other bracketed text is left byte-for-byte alone). User turns are still never
  rewritten, and persisted rows are still untouched.
- Recalled OS-paging context pages are stripped the same way. A page is a
  verbatim transcript of earlier turns, so the markers written before 0.8.137 sat
  in the recall text as well.
- A marker written in the same response that carries real tool calls no longer
  enters history. That text became the assistant message requesting the batch,
  which the final-response guard never inspects, so it survived into compaction,
  summaries and the next prompt.
- A tool batch now counts as tool use only once at least one call came back with
  a result that is neither an error nor a denial. The flag was set at dispatch,
  so a batch whose every call was refused by the permission layer or the
  idempotency ledger silently spent the turn's right to a corrective retry.
- The chat mailbox database keeps its `-wal` and `-shm` sidecars `0600`, not just
  the database file. Under `journal_mode = WAL` everything written since the last
  checkpoint lives in the sidecar, and SQLite created those with the mode of the
  main file it found: the file the module promised was owner-only held a 4 KiB
  header while the assignment bodies and plaintext origins sat next to it
  world-readable. The database file is now created `0600` before SQLite opens it
  (so sidecars are born private) and all three are restricted after migration (so
  a database an older build left behind is fixed on open). The
  "the token is nowhere in the file" assertion, which read the empty main file
  and could not have failed, now scans all three.
- `synchronous` is now `FULL` for that database. Under WAL with `NORMAL` a commit
  is not flushed, so a power cut -- the incident this store was built after --
  could take back an acceptance the caller had already been given, while the
  module documented "accepted" and "durable" as the same instant.
- A chat pull no longer performs its SQLite writes while holding the session
  state lock. The writes are planned under the lock and executed after it, and
  the mailbox connection now uses a bounded busy handler (5 s) instead of the
  unbounded one: a second process holding the database lock used to block a chat
  pull indefinitely, which is precisely the outage the mailbox's "a failed write
  costs durability and nothing else" trade-off exists to avoid, and the tested
  error branch was unreachable for the real store.
- A chat re-registers itself at most once a minute. A daemon that answers every
  pull with "unknown session" made the poller register again -- and announce it
  -- every 1.5 s.
- `prx doctor`'s daemon-PATH check no longer demands its own directory when that
  directory is a Cargo build output. `cargo run -- doctor` and
  `./target/release/prx doctor` warned about a healthy daemon and told the
  operator to add a build directory to the unit's `PATH`. The required directory
  now comes from the daemon binary recorded in `daemon_state.json` (the daemon
  writes `binary_path` there), falling back to the doctor's own directory only
  when it is not a build directory.
- `prx doctor` checks that the pid in the daemon state file still belongs to a
  prx process before reading `/proc/<pid>/environ`. A reused pid made the daemon
  PATH verdict come from an unrelated process' environment; it is now an explicit
  skip.
- The six daemon-PATH helpers in `src/doctor/mod.rs` are compiled only where they
  are used. On a non-Linux target they were dead code, so `cargo check` for
  macOS and Windows carried six `never used` warnings against the zero-warning
  rule.

### Changed

- Narrowed three CHANGELOG claims in the unreleased 0.8.137, 0.8.140 and 0.8.142
  entries to what their code actually does: "or replayed", the enforcement scope
  of the `git_operations` action contract, and the mailbox database's `0600`
  promise plus the byte-level token assertion behind it.

## [0.8.142] - 17 September 2026

### Added

- The daemon's chat-session mailbox is now durable. `chat_sessions` kept its
  registry and every queued assignment in process memory, and said so: a daemon
  restart forgot every session and every assignment somebody had already been
  told was accepted, and every `prx chat` had to register again before it could
  be given work. Registrations, queued assignments, deliveries,
  acknowledgements, deregistrations and results are now written through to
  `{workspace}/state/chat-mailbox.db` (SQLite, owner-only, parameterised
  throughout) before the caller is answered, and the gateway replays the file on
  the way up -- the one startup path both `prx gateway` and the daemon's
  supervised gateway pass through, so there is a single attachment point and the
  replay is idempotent against the supervisor restarting the gateway in-process.
  A chat keeps its session id *and* its token across the restart: only the
  token's SHA-256 is stored, so the comparison that authenticates the first pull
  after a restart is the comparison it always was, and
  `the_mailbox_file_never_holds_a_plaintext_token` asserts the secret appears in
  none of the stored rows. Work handed out and never acknowledged returns to the
  head of the queue on replay, exactly as it does on the next pull; work that
  *was* acknowledged is restored as acknowledged, because the chat holding it
  usually outlives the daemon and redelivering would be the runtime deciding for
  it that it had lost work it is still doing. Liveness is restored from the
  stored evidence rather than reset, so a chat that died with the daemon reads
  as `silent` instead of as freshly `polling`.
- Durability never costs availability: a store that cannot be written logs at
  `error` and the in-memory mailbox carries on, so a full disk degrades the
  guarantee instead of refusing live work.
  `a_store_that_fails_every_write_still_delivers_work` holds that trade-off to
  an assignment being accepted, delivered and reported while every write fails.

## [0.8.141] - 17 September 2026

### Fixed

- The session worker's manifest is now bound to its tool filter in one place.
  `run_validated_manifest` read `manifest.allowed_tools` and
  `manifest.channel_excluded_tools` out at the call site and handed them to
  `select_tools_for_worker`, and no test reached that wiring: the covering test
  called the filter with a list it computed itself, so replacing the exclusion
  argument with `&[]` left the suite green even though the worker lost a layer of
  the channel boundary. The call site now goes through
  `select_tools_for_manifest(source, &manifest)`, which cannot be handed a list
  that did not come from the sealed manifest, and
  `a_worker_manifest_binds_its_channel_exclusions_to_the_tool_filter` drives that
  binding, with the architecture guard
  `a_session_worker_selects_its_tools_through_its_manifest` holding the call site
  to it -- a unit test on the binding helper alone does not close this, because
  the call site can stop using the helper. No behaviour change: the worker's own
  tool loop already re-filtered against the inherited list, so this closes a test
  gap in a redundant defence rather than an escalation path.

### Changed

- The gateway's two launch paths register one tool surface from one body. The
  gateway the daemon runs (rebuilt on every hot reload) and a standalone
  `prx gateway` each carried a symmetric copy of the same block — the channel
  routing registry, `message_send` behind Signal, `sessions_spawn`
  unconditionally, `config_reload` — and only the daemon copy had tests, so a
  change to one could silently give the two launch modes different tools. Both
  now call `register_gateway_turn_tools`, and
  `both_gateway_launch_paths_publish_the_same_tool_surface` asserts the two paths
  publish the same tool names. The published surface is unchanged: 37 tools
  without Signal, 38 with it.
- Two architecture guards in `tests/architecture_boundaries.rs` keep both of the
  above single. `a_session_worker_selects_its_tools_through_its_manifest` pins
  that `run_validated_manifest` selects through `select_tools_for_manifest` and
  that no other production site in the module calls the tool filter directly;
  `both_gateway_launch_paths_register_their_tools_from_one_body` pins that
  `build_gateway_turn_runtime` and `run_gateway` both go through
  `register_gateway_turn_tools` and that `sessions_spawn` is constructed at
  exactly one production site in the gateway. Both scan the source text, so
  re-inlining either seam fails a test instead of shipping.
- `docs/channels.md` documents what a gateway without Signal does with a
  sub-agent result. `sessions_spawn` is registered on every gateway, but with no
  IM channel the completion announcement is recorded in a `tracing` line rather
  than pushed anywhere, so the result has to be pulled back with
  `spawn_batch` + `join`, `sessions_list` / `sessions_history`, or
  `GET /api/runtime/tasks`.

## [0.8.140] - 17 September 2026

### Changed

- Every action-dispatched tool is now held to the shared action contract
  (`tools::schema::action_contract::assert_action_tool_contract`), not just the
  five wired in 0.8.133. `mcp_call`, `git_operations`, `gateway`,
  `skills_manage`, `composio`, `nodes`, `subagents`, `proxy_config`,
  `chat_schedule`, `managed_session` and `hooks_manage` each gained a test in
  their own module, bringing the enforced set to sixteen surfaces. One of those
  sixteen is enforced on a narrower discriminator than it dispatches on:
  `git_operations` dispatches on `operation` (ten values) and the shared contract
  reads `action`, so what it holds there is the four-value `'stash'` sub-action
  enum. `docs/tool-schema-and-child-capability-parity.md` records that gap; the
  ten `operation` values are described but not contract-enforced. The contract
  requires that the tool-list description, the `action` parameter's own
  description, and the description of every parameter an action cannot be called
  without each name that action, so a capability can no longer be published as a
  bare enum identifier the model has to guess at.
- Rewrote the tool and parameter descriptions those eleven tools publish so each
  names its actions explicitly: `mcp_call` now spells out `'call'`, `'list'`,
  `'status'` and `'refresh'`; `git_operations` names its ten operations and the
  four `'stash'` sub-actions; `gateway`, `nodes`, `subagents`, `skills_manage`,
  `proxy_config`, `chat_schedule`, `managed_session` and `hooks_manage` name
  every enumerated action and say which action each required parameter belongs
  to; `composio` names `'connect'` and `'execute'` on the parameters those two
  actions require. No executor behaviour, action enum or required-field set
  changed.
- Added the missing `action` parameter description to the `skills_manage`,
  `proxy_config` and `hooks_manage` schemas and the missing `event` description
  to `hooks_manage`. These were published with no description at all, leaving
  the model the bare property name; the enums, defaults and conditional
  requirements are unchanged, so existing calls validate exactly as before.

## [0.8.139] - 17 September 2026

### Added

- `prx doctor` now inspects the live daemon's own `PATH` instead of only the doctor
  process's. It takes the pid from the daemon state file, reads `/proc/<pid>/environ`
  on Linux, and checks that the daemon's `PATH` contains the directory the running
  `prx` binary lives in plus `~/.cargo/bin` when that directory exists. A missing
  directory is reported as a warning naming the directory and the fix — add
  `Environment="PATH=…"` to the unit or re-run `prx service install` — which is the
  case for hand-written units that were never given the installer's absolute `PATH`
  and therefore make the shell tool answer `sh: prx: not found` inside the daemon.
  When the `PATH` is complete the check prints the daemon `PATH` it saw. The check
  never turns the run red on its own: a state file without a pid, an unreadable or
  dead `/proc` entry, and non-Linux hosts are all reported as an explicit skip. Both
  `prx doctor` and `prx doctor runtime` run it.

## [0.8.138] - 17 September 2026

### Changed

- Recalled OS-paging context pages are now framed as history instead of arriving as bare
  quoted text. The injected recall message keeps `[Recalled context from earlier in this
  session]` as its first line and adds a history notice directly underneath: the excerpts
  are a verbatim transcript of earlier turns, every number, version, commit hash, status,
  path, and command output in them describes the state at the time it was recorded, they
  must not be presented as something done, run, or verified in the current turn, and a
  current value has to be re-measured with a fresh tool call. Each excerpt is also
  introduced by its own `[page evicted_at=<timestamp>]` source line (taken from the page
  chunk's creation time, reduced to `[page]` when the backend supplies none) so the model
  can see how old the excerpt is. Recall thresholds, page counts, injection role, and
  injection position are unchanged.

## [0.8.137] - 17 September 2026

### Fixed

- A fabricated `[Used tools: ...]` line can no longer be claimed or delivered, and the
  replay path through cached channel turns is closed.
  A final response that carries such a line while the turn never dispatched a tool now
  spends the existing corrective retry; if the next response still carries one, only the
  marker lines are removed and the real answer is still delivered (a response that is
  nothing but a marker keeps failing closed as before). The surviving text is stripped
  before it reaches the stream, the history, or the recorded assistant message, and
  cached channel turns are stripped again on the way back to the model in both
  `normalize_cached_channel_turns` and `merged_history`. Previously the runtime only
  rejected a response that was *entirely* a marker, so a marker appended to a real answer
  was persisted, replayed, and imitated: a WhatsApp session announced tool runs for a week
  without ever emitting a single tool call. Persisted rows are untouched — only the copy
  handed to the model and the user is cleaned. The other routes a *stored* marker takes
  back into a prompt — the session and shared event preambles, recalled context pages, and
  the assistant message that carries a real tool batch — are closed in 0.8.143.

## [0.8.136] - 16 September 2026

### Fixed

- The gateway webhook turn's spawn boundary is now enforced at the production seam.
  Seeding the turn's spawn execution context and scoping the tool loop inside it moved
  into a single `within_gateway_turn_spawn_context` function, covered by a test that
  reads the context exactly as `sessions_spawn` does, plus an architecture guard that
  the webhook turn still routes through it and scopes no context of its own. Previously
  only the context builder was covered, so emptying the inherited exclusion list at the
  call site would have let a sub-agent spawned from a webhook turn reach tools the
  operator hid from the gateway, with the whole suite green.

### Changed

- The `sec-audit` workflow's Miri comment now quotes the measured start-up cost
  (about 3 minutes over this test binary) instead of an unsourced ten-minute figure.

## [0.8.135] - 16 September 2026

### Fixed

- The scheduled `Sec Audit` Miri job runs an explicit allowlist of pure-computation
  modules instead of the whole library. `cargo miri test --lib` could never pass:
  `ring` resolves its CPU feature flags through the foreign function
  `OPENSSL_cpuid_setup` before any primitive runs, Miri refuses foreign calls, and
  the refusal aborts the entire test binary at the first test that touches crypto —
  which is why the run reported only `test exited abnormally` with no error text.
  `ring` sits on the witness-keyring, session-worker HMAC, GLM signing and rustls
  provider paths, so ignoring the one reported test would only move the abort. The
  library's ~6700 unit tests, many of them async or filesystem-bound, do not fit
  Miri's isolation model or the job's 45-minute budget either. The job now runs
  `agent::stream_buffer`, `providers::utf8_stream` and `memory::chunker` under
  `-Zmiri-strict-provenance` — 51 tests, clean — with the admission rule and the
  remaining `unsafe`-coverage gap recorded in the workflow. No undefined behaviour
  was found and no production code changed.

## [0.8.134] - 16 September 2026

### Fixed

- Gateway: `sessions_spawn` is now registered on every gateway, not only when a
  Signal channel is configured. The channel passed to the tool is only the
  fallback destination for a run's completion announcement, so a deployment
  without Signal silently lost the ability to start sub-agents from a webhook
  turn. Signal remains the announce destination wherever it is configured;
  without it the gateway falls back to its own sink, which records the
  announcement in the log — the result itself stays available through the tool's
  `join` action, `sessions_list` / `sessions_history`, and
  `GET /api/runtime/tasks`. No new outbound transport and no new listener.

### Changed

- Gateway: a webhook turn now seeds a turn-root spawn execution context. A
  sub-agent started from that turn inherits the turn's run id as its parent
  (previously it had none) and the gateway's own hidden tool names. The gateway
  is not an IM channel and has no `channel_exclude` list, so the inherited
  boundary is exactly `tool_tiering.always_exclude`: a spawned sub-agent's tool
  surface is the gateway's own surface minus what the operator hid from it, and
  `always_include` is unaffected.

## [0.8.133] - 16 September 2026

### Changed

- Hold every action-dispatched tool to the *whole* schema contract, not just the
  half of it that fits in a conditional-required clause. `assert_action_schema_contract`
  now also requires the `action` enum's own description to name every action it
  enumerates, and a new `assert_action_tool_contract` requires the tool-list
  description to name them too — the two pieces of text a model reads before it
  can choose a tool and then an action. Both checks used to exist only inside
  `sessions_spawn`'s private test module, which is why the convention had
  silently stopped holding elsewhere: `cron`, `xin` and `wasm_plugins_manage` all
  shipped `"Action to perform."` as the entire action description, `xin`'s
  tool-list summary named none of its 23 actions, and `message_send` advertised
  two of its six. All five schemas are brought into compliance. The 0.8.116 note
  that the schema-contract tests "continue to hold" was true only of
  `sessions_spawn`; it holds across all five from this version.

### Fixed

- Stop `docs/tools.md` contradicting itself about what an IM channel turn may
  do. The entrypoint table said hooks were unavailable on a channel supervisor
  and its footnote listed seven excluded tools, while the routing section below
  listed fifteen — hooks in fact fire normally and only the management and
  status tools are hidden. Both places are now derived from
  `DEFAULT_CHANNEL_EXCLUDED_TOOLS`, and a new section explains the two distinct
  reasons a tool can be absent from a turn: intent routing, which
  `[tool_tiering] always_include` overrides by name, and channel exclusion,
  which outranks routing entirely.

## [0.8.132] - 16 September 2026

### Fixed

- A corrective retry no longer hands the model a second consecutive user turn,
  and labels its own words. The instruction was pushed as its own `user`
  message, producing `[system, user, user]`: no provider in this crate merges a
  same-role run, and a strict chat template rejects the alternation break
  outright. It is now appended to the trailing user turn as a paragraph marked
  `[system note]`, so the request shape is unchanged and anything reading the
  turn back — including a model that quotes it at the user — can tell the
  runtime's instruction from the operator's words. The system prefix is still
  byte-identical across the retry.
- A delegate or spawned sub-loop records its own
  `context.compaction.degraded` event. The child inherits the parent's
  `source_message_event_id`, so both carried the same `turn_key`, and the
  per-turn idempotency key collapsed the two events into one: a turn in which
  parent and child both degraded reported only the parent. The child runtime
  now derives a nested turn key, and the session worker does the same for each
  segment it runs. Repeated degradation inside one runtime is still one event.
- `prx init --force` exits non-zero when a setting is reset because this build
  can no longer represent it. A key whose parent the templates now write as a
  scalar is data loss exactly like one the rewriter could not place, but it
  only printed a warning, so a script read the run as clean. It is now listed
  under its own heading in both the warning and the error.

### Changed

- The `--force` archive carries the `.init-defaults.toml` defaults record
  alongside `config.toml` and the fragments. The record is the baseline the
  next `--force` classifies against, so an archive without it could not restore
  a tree that behaves the way it did before. A tree written by an older build
  has no record and is archived without one.
- The tool-snapshot render cache decides a hit by comparing the snapshot. The
  `Arc::ptr_eq` short-circuit in front of that comparison could never return
  true — each iteration compiles a freshly canonicalized `Vec` — so it was
  removed rather than left reading as a working optimization.

## [0.8.131] - 15 September 2026

### Security

- The channel surface's hidden tool set crosses the process boundary. A
  sub-agent started with `mode: "process"`, and a session worker resumed from a
  manifest, ran with the full operations surface: the exclusion list lived only
  in a task-local the OS child never saw, and the worker registers `gateway` and
  `config_reload` itself. A conversation partner on an IM channel could
  therefore reach `gateway`, `config_reload`, `proxy_config`, `nodes` and
  `git_operations` by asking for a spawn in process mode — the same route
  0.8.130 closed for task mode, with one argument changed. The list now travels
  in `WorkerManifest`, which the parent capability seals, so it cannot be
  stripped in transit; the worker applies it after the agent's own
  `allowed_tools`, so naming a hidden tool there does not re-open it; and
  everything the worker spawns in turn inherits the same boundary. Names an
  operator re-enables through `[tool_tiering] always_include` are unaffected.
- An excluded tool published under an alias is hidden too. The exclusion was
  matched against each backend's root name, so an MCP router or WASM alias that
  answers to an excluded public name stayed exposed.
- Compiling a tool schema is bounded. `$ref` resolution pops its stack, so a
  non-cyclic document that names the same definition from `k` places at each of
  `n` levels expanded to `k^n` nodes; MCP and WASM contracts are caller-supplied
  and are compiled on their first call, so such a document could burn a turn
  inside the validator. Past the budget the remaining subtrees compile to no
  constraints and a warning is logged. The bound is a work count, not a clock:
  the same document always costs the same.
- The compiled-schema fingerprint distinguishes the three JSON number
  representations. Without a discriminator `minimum: -1` and
  `minimum: 18446744073709551615` hashed to the same eight bytes, as did `1` and
  the smallest subnormal `f64`, so two different caller-supplied contracts could
  share a cache slot.

### Fixed

- A whole number written in float form reaches the executor as an integer.
  0.8.128 made `type: "integer"` accept `200.0`, as JSON Schema requires, but
  executors read integers with `as_u64()` / `as_i64()`, which answer `None` for
  `Float(200.0)` and fall back to a default — so an accepted call silently ran
  with the wrong value and the model got no signal, where before it got a
  correctable rejection. The execution service now normalizes integral floats
  against the published schema after validation and before dispatch, including
  inside nested objects, arrays, `patternProperties`, `additionalProperties` and
  the taken branch of an `if` / `then` / `else`. A non-integral value is still a
  violation and is still rejected.
- `enum` and `const` read whole numbers the way `type` does. `serde_json`
  compares numbers by representation, so one document could accept `1.0` for
  `type: "integer"` and reject it for `enum: [1]`. Distinct 64-bit integers are
  still distinct.
- A `context.compaction.degraded` event the backend refused is retried instead
  of claimed. The turn marked itself as having recorded the event before
  attempting the write, so a backend that was briefly unavailable lost the event
  for the rest of the turn — silently, because the failure is only a debug line.
  The claim now follows a write that landed; duplicates remain impossible
  through the turn-derived idempotency key.
- The chat thinking-progress batcher flushes its final window on a failed turn
  as well as a completed one. `StreamFailed` retires the draft, so the reasoning
  that led up to the failure was dropped — the part a reader most needs.

### Changed

- The published-contract sweep derives its example calls from the schema alone.
  The generator repaired a seed against the validator's own complaints until the
  validator stopped complaining, and the test then asserted that the validator
  does not complain: the assertion was the loop's exit condition and could not
  fail. Examples are now read off `required`, `type`, `enum`, `const`,
  `minimum`, `minLength`, `minItems`, `pattern` and local `$ref`, with `if` /
  `then` / `else` decided by a predicate written in the test, and validated
  once with no repair; a contract that cannot be satisfied this way is pinned by
  name with the reason. Paired accept/reject cases cover the five keywords
  0.8.128 changed, none of which appear in any schema PRX itself publishes.

## [0.8.130] - 15 September 2026

### Fixed

- A session whose `switch` rollover keeps degrading records one
  `context.compaction.degraded` event per turn instead of one per tool-loop
  iteration. The event id doubled as the idempotency key, so every call was a
  guaranteed insert and the key could never reject anything; the key is now
  derived from the turn, and the turn also claims the write once in process. The
  pre-provider budget check alone runs on every iteration, so a long degraded
  session used to open one synchronous write transaction per iteration for the
  rest of its life.
- A corrective retry no longer rewrites the system prefix of the request. The
  "previous response was empty" and "previous response was a tool summary"
  instructions were appended as request-local `system` messages, which the
  front-coalescing pass then folded into the leading system message: the retry
  round missed the provider-side prefix cache for the whole conversation, on the
  one turn that had already paid for a wasted provider call. They ride at the
  tail of the request instead, so the system message stays byte-identical and
  every provider still sees exactly one system message at index 0.
- `switch` compaction makes progress when the cold window is nothing but tool
  round-trips. The boundary that keeps a tool result from being orphaned could
  only walk backwards, so a window that sat entirely inside one long round-trip
  chain walked back to the start, reported "nothing to compact", and left the
  session silently running on the lossy trim from then on. The boundary now
  walks forward to the first message that can head the hot window, and the
  window folds into one handoff.
- OS-paging no longer strands a tool result at the head of the hot window. An
  eviction span that ended between an assistant `tool_calls` envelope and the
  result answering it left an orphan every native provider rejects; the stranded
  round-trip is paged out with its request, so the output stays recallable
  instead of being dropped.
- The chat thinking-progress batcher publishes its final window. Reasoning that
  arrived after the last 120 ms boundary stayed in the buffer and never reached
  the live counter, so the counter stopped short of the body the completed turn
  goes on to show.

### Changed

- A sub-agent that `sessions_spawn` or `delegate` runs inside the parent process
  inherits the tool exclusions of the channel surface that spawned it. Sub-agent
  loops run without tiering of their own, so a conversation partner on an IM
  channel could reach the operations surface the channel hides simply by asking
  for a spawn. Names an operator re-enables through `[tool_tiering]
  always_include` are unaffected, and operator entry points (terminal chat,
  gateway, console) seed no exclusions. `mode: "process"` spawns and resumed
  session workers are a separate process and were **not** covered by this
  change; 0.8.131 carries the list across that boundary.
- The canonical tool order is a pure function of the exposed set even when two
  entries share a name. Duplicate names were collapsed by keeping whichever copy
  came first, so an MCP or WASM alias shadowing a built-in, or a middleware pass
  that rewrote one description, made the rendered prompt depend on registry
  order. Ties are broken by content digest instead.
- Compiling a turn's tool context no longer copies the snapshot. Canonicalizing
  it deep-copied every `ToolSpec` — tens of kilobytes of JSON schema — once per
  tool-loop iteration, and the render cache copied it a second time; the
  snapshot is sorted in place and shared.

### Removed

- The unreachable legacy chat stream driver (`#[cfg(test)]`-only, no callers).

## [0.8.129] - 15 September 2026

### Fixed

- `prx init --force` no longer discards a value written under a quoted key or a
  quoted table header. The folder decoded key paths with a bare-key scanner that
  returned nothing for a segment containing a quote, so `[cost.prices."gpt-4.1"]`
  and `"gpt-4.1" = [...]` were neither carried over nor reported: the operator's
  value was replaced by the shipped default in silence. Key paths are now
  decoded and re-emitted through the TOML grammar itself, and anything the
  rewriter still cannot place is named on stderr and exits the command non-zero
  rather than disappearing.
- Two `prx init --force` runs in the same second each keep their own archive.
  The archive directory was stamped to the second and created with
  `create_dir_all`, so the second run copied over the first run's files and the
  earlier configuration was gone. The stamp carries milliseconds, the directory
  is created exclusively, and a taken name takes a numeric suffix.

### Changed

- `prx init --force` refreshes defaults that moved instead of pinning the ones an
  older `prx init` wrote. `prx init` records the values its own templates produce
  in `.init-defaults.toml` next to `config.toml`, which lets `--force` tell a
  value the operator typed from an untouched default. The report now prints the
  three outcomes separately: preserved explicit settings, defaults this version
  adds, and untouched settings reset to a refreshed default. A tree with no such
  record keeps every value currently on disk, as before.
- `prx chat` shows the inherited embedding-default notice where a chat user can
  read it: one stderr line before the TUI takes the screen, and one line on the
  first screen of the session. The warning `dispatch` logs went to `chat.log`,
  which is exactly where the interactive entry point never looks.

### Added

- Tests for `starts_agent_runtime`, which decides which entry points announce the
  inherited embedding default and previously had none.

## [0.8.128] - 15 September 2026

### Security

- Route `Agent::execute_tool_call` through the shared tool execution service.
  The `Agent::run_single` shell called `Tool::execute_named` directly, so a
  model-authored argument document reached a tool with no autonomy policy
  decision, no approval gate, no published-schema check and no audit record —
  the second execution path left over after 0.8.126 closed the gateway one. It
  now builds the same `ToolExecutionService` every other entry point uses, with
  a fail-closed approval strategy because this shell cannot prompt anyone, and
  `AgentBuilder::security_policy` binds the policy it decides under. The runtime
  scope block is authored by the service from the agent's own context instead of
  being scrubbed in place, and the scope is deliberately not the local-operator
  tuple, so the destructive-action guard still refuses.
  `tests/architecture_boundaries.rs` fails if a direct call comes back.
  Two behaviour changes come with it. A failed tool call on this path now
  emits `HookEvent::Error` where it used to emit `HookEvent::ToolCall` with
  `success: false`, so a hook that counts failures by subscribing to `ToolCall`
  stops seeing them. And `_zc_scope_trusted` — which asserts "the runtime, not
  the caller, wrote this scope block" — is `true` on this path, where the old
  code forced it to `false`; the marker that gates destructive `gateway`
  actions is `_prx_scope_trusted`, and that one is still `false` here.

### Fixed

- A key matched by `patternProperties` is no longer rejected by
  `additionalProperties: false`. JSON Schema counts a pattern-matched key as
  declared, so a dynamic MCP or WASM contract that closes its property set and
  names the open part by pattern had every legal call refused. The pattern
  subschemas are enforced for the keys they claim, and a pattern this build
  cannot compile leaves the property set open rather than closed, because no key
  can then be shown to be outside it.
- `type: "integer"` accepts a whole number written as a float. JSON Schema
  defines `integer` by mathematical value, so `1.0` is one and `1.5` is not;
  several providers emit whole numbers in float form and every one of those
  calls was refused.
- `oneOf` no longer rejects a call because this subset cannot tell two branches
  apart. Branches that differ only by a keyword the validator does not model
  (`pattern`, `format`, `not`, `multipleOf`) compile to the same weaker rules
  and both match, which used to fail the "exactly one" count. The rule is now
  explicit: where every branch is expressible in the modelled subset — which
  covers every contract PRX itself emits — "exactly one" is enforced as written;
  where a branch is not, the test relaxes to "at least one", so an undecidable
  `oneOf` can still reject a document no branch accepts but can never reject one
  on multiplicity alone.
- `if` / `then` / `else` written at a schema's own level is enforced, not only
  inside an `allOf` member. Both spellings are the same contract.
- Local `$ref` entries are resolved against the document's `$defs` and
  `definitions` before compilation, so the subtree behind a reference is
  enforced instead of silently unconstrained. Resolution carries a reference
  stack that stops a cycle, and a reference that points outside the document —
  or at a missing definition — compiles to no constraints rather than to a
  rejection.
- The compiled-schema cache keys on the whole contract instead of the tool name,
  so two documents advertised under one name each keep a slot. Alternating
  between them missed on every call and took the global write lock every time,
  which is exactly what an MCP router swapping servers or a WASM alias reload
  does.
- A full compiled-schema cache evicts its least recently used entries instead of
  clearing the table. Past the capacity, clearing meant every new name dropped
  every cached contract, so a runtime with more live aliases than the capacity
  recompiled every schema on every call — worse than having no cache at all.

### Changed

- The compiled-schema fingerprint is SHA-256 over the provider dialect, the
  public tool name and the complete schema document, replacing a 64-bit
  `DefaultHasher`. Dynamic MCP and WASM schemas are caller-controlled content,
  and a collision would validate a new contract against the rules compiled for
  an old one. Every variable-length field is length-prefixed.

## [0.8.127] - 14 September 2026

### Fixed

- `prx --config-dir <DIR> init --force` no longer rewrites the default
  configuration. The init branch read only its own `--dir` argument, so a run
  scoped to another directory archived and regenerated `~/.openprx` instead —
  reordering the operator's live configuration tree and deleting emptied tables
  while leaving the requested directory untouched. `init` now resolves its
  target through one shared precedence: `--dir`, then the global `--config-dir`,
  then `OPENPRX_CONFIG_DIR` / `OPENPRX_WORKSPACE` / the active-workspace marker,
  then `~/.openprx`. Passing `--dir` and `--config-dir` with different
  directories is rejected rather than guessed, a blank value for either is
  rejected, and a target that does not exist yet is created.
- Key material now follows the resolved config directory instead of anchoring on
  `$HOME`. The runtime witness key and the session-worker secret are created
  lazily from call sites that hold no `Config`, so a run scoped with
  `--config-dir` or `OPENPRX_CONFIG_DIR` still wrote `~/.openprx/keys/`. The CLI
  now publishes the directory it resolved and both paths follow it, with the
  historical `$HOME/.openprx` location kept as the fallback for library
  embeddings and the existing path-override environment variables unchanged.

### Added

- `prx init --force` prints a notice when a live `prx daemon` is running out of
  the resolved target directory, stating that the daemon will hot-reload the
  regenerated configuration. Liveness is read from a held `daemon.lock` or a
  freshly written `daemon_state.json`; the notice never blocks the run.
- `docs/configuration.md` documents the configuration-directory precedence.

## [0.8.126] - 14 September 2026

### Security

- Route the gateway MCP endpoint through the shared tool execution service.
  `POST /mcp/v1/tools/call` called `Tool::execute_named` directly, so a request
  body an authenticated external MCP client fully controls reached a tool with
  no autonomy policy decision, no approval gate, no published-schema check and
  no audit record — none of which the 0.8.123 schema enforcement could see,
  because that enforcement lives in `ToolExecutionService::execute`. The
  endpoint now builds the same service every other entry point uses. Approval
  there is fail-closed (`DenyApprovalStrategy`): a remote client cannot answer a
  prompt, so a tool the autonomy level wants confirmed is refused rather than
  auto-approved. The success and tool-failure response shapes are unchanged, and
  a service-level refusal reuses the same envelope with `is_error: true`.
  `exposed_tools` semantics are unchanged.
  `tests/architecture_boundaries.rs` now pins the inventory of production call
  sites that invoke a tool directly, so a new bypass fails a test.

### Fixed

- Validate tool arguments against the schema the provider was actually shown.
  Gemini's API rejects `additionalProperties`, `minLength`, `minimum` and a
  dozen more keywords, so those are stripped before the request goes out — but
  validation ran against the unstripped document, so every tool that closes its
  property set (30+ of them) rejected otherwise-correct Gemini calls for a field
  the model was never told about. Execution now carries the provider's schema
  dialect and replays the same `SchemaCleanr` strategy before validating, so the
  relaxation list is the cleaner's list rather than a second copy of it.
  Everything that survives the rewrite stays enforced, and OpenAI and
  OpenAI-compatible endpoints are validated against the published document
  unchanged.
- Print the `prx init` report instead of logging it. `prx init` returns before
  any tracing subscriber is installed, so the archive directory, the list of
  preserved keys and the reset warning all went nowhere: an operator whose whole
  configuration had just been reset to templates saw no output at all. The
  report is now written to stdout/stderr directly.
- Stop `prx init --force` from resetting a configuration silently. When the
  preserved values fail staged validation the command still falls back to bare
  templates — that is how `--force` repairs a configuration this build cannot
  load — but it now prints an uppercase warning naming the archive directory and
  the validation error, and exits non-zero.
- Order the vector recall candidate window by insertion (`rowid`) instead of
  `updated_at`. `updated_at` is a local-offset RFC 3339 string ordered as text,
  so a DST transition, a machine timezone change, or a store shared between
  processes in different zones reordered whole blocks of rows and truncated the
  wrong ones. The partial index is replaced by
  `idx_memories_embedding_candidates`, which leads with the three
  embedding-identity columns every vector query filters on; the previous index
  led with `updated_at` and carried none of them, so a store whose embedding
  model had changed scanned past large runs of non-matching rows.
  `idx_memories_embedding_recency` is dropped on open.

### Added

- `[memory] vector_candidate_cap` (default 2000) bounds how many embedded
  memories one vector recall scores. In a store holding more embedded memories
  than the cap, an old-but-highly-relevant memory is never scored and therefore
  never recalled; the key makes that trade-off tunable, and vector recall now
  warns once per process when a recall actually hits the cap instead of logging
  it at `debug`. `docs/configuration.md` documents the trade-off in both
  directions.

### Changed

- The vector recall candidate count is the configured cap, raised only to the
  requested result count. It was `max(limit * 50, 2000)` with nothing to tune,
  which meant a caller asking for many results silently widened the scan and an
  operator could not narrow it.
- `prx init` exits non-zero when it resets a configuration to templates. A
  script that treated `prx init --force` as always-clean will now see the
  failure it was hiding; the tree on disk is still valid and usable.

## [0.8.125] - 14 September 2026

### Security

- Upgrade rustls to 0.23.45 for RUSTSEC-2026-0285 (TLS 1.3 handshake
  messages accepted across encryption level boundaries).

## [0.8.124] - 14 September 2026

### Fixed

- Stop `prx init --force` from resetting explicit configuration. Regenerating
  the managed templates now refreshes structure, comments and defaults only:
  every key written in `config.toml` or in a managed `config.d/*.toml` fragment
  keeps its value, keys the template does not mention are carried into the table
  they came from, and a value written in `config.toml` still out-ranks the
  fragment template's default. The previous tree is copied to
  `config.d.pre-<version>-<timestamp>/` before anything is written, preserved
  keys are listed in the command output, and a preserved value this build can no
  longer represent is reported as reset instead of disappearing. Previously the
  templates were written verbatim, so an operator who had turned local vector
  recall off lost that decision on the next `--force`.
- Make the local vector recall default flip visible on upgrade. A configuration
  written before 0.8.10x has no `memory.embedding_provider` key, so upgrading the
  binary alone switched vector recall on. `prx doctor memory` now reports the
  inherited default and `prx chat`, `prx daemon`, `prx gateway` and
  `prx channel start` log it once at startup, naming the key that turns it off.
  The notice disappears as soon as the key is written either way.

### Changed

- Generated memory templates state `embedding_provider`, `embedding_model` and
  `embedding_dimensions` explicitly, with the line that turns vector recall off,
  instead of only mentioning them in comments. A generated configuration no
  longer depends on which side of the default flip the binary is on.
- Move local embedding computation off the async runtime worker. The built-in
  feature-hash provider is synchronous CPU work with no I/O; batches above an
  inline budget are handed to the blocking pool, and the comment claiming the
  call awaited I/O is corrected.
- Bound the unindexed cosine scan in SQLite vector recall by recency instead of
  scoring every embedded memory on every turn, backed by a partial index on
  embedded rows. The bound is a candidate cap: in a store holding more embedded
  memories than the cap, the ones outside the window are not scored and
  therefore cannot be recalled by vector search at all, however relevant they
  are. The cap was fixed at `max(limit * 50, 2000)` here and became the
  configurable `[memory] vector_candidate_cap` (default 2000) in 0.8.126.

## [0.8.123] - 14 September 2026

### Fixed

- Enforce the published tool schema on the production execution path. The
  canonical validator existed since 0.8.110 but every call site was inside
  `#[cfg(test)]`: the shipped pipeline checked only the root `required` list, so
  an out-of-range action, a missing action-specific field, a wrong scalar type
  or an undeclared property reached the executor. `ToolExecutionService::execute`
  now validates each call against its own descriptor schema — the same document
  the provider was shown — for native, skill, MCP alias and WASM plugin tools
  alike. Rejection happens before the approval prompt and before any backend
  runs, produces an `InvalidArguments` outcome naming every offending path so the
  model can correct itself, and is audited like any other terminal outcome.
  Validation is fail-open on JSON Schema keywords PRX does not model and
  fail-closed on the ones it does, so richer dynamic MCP and WASM contracts stay
  callable. Contracts are compiled once per tool name and schema fingerprint, so
  per-call validation does not re-derive rules and a re-advertised alias with a
  changed contract is recompiled rather than checked against stale ones.

  This is a behaviour change, not only an internal one: the constraints every
  tool already published take effect in production for the first time here. A
  tool that declares `additionalProperties: false` (30+ of them do) now rejects
  a call carrying any field it does not declare, an `enum` rejects a value
  outside it, a declared type rejects a differently typed value, and
  `minLength` / `maxLength` / `minimum` / `maximum` / `minItems` / `maxItems`
  are enforced. Three spellings that executors used to accept are now refused
  before they run, because the published schema never allowed them:
  `git_operations` with `action: "save"` (the stash enum is
  `push` / `pop` / `list` / `drop`), `proxy_config` with a mixed-case `action`
  such as `"GET"`, and `http_request` with a lowercase `method` such as `"get"`
  or a non-string value in `headers`.
- Stop the published schema from contradicting the executor for tools with a
  default action. `sessions_spawn`, `mcp_call`, `message_send` (both
  implementations), `subagents` and `managed_session` no longer declare the
  discriminator as a root `required` field — the executor has always accepted
  its absence — and instead publish the default and bind that action's required
  fields when it is omitted. Without this, turning on runtime enforcement would
  have rejected calls that were valid before, and omitting the action would have
  escaped its field requirements entirely.

## [0.8.122] - 14 September 2026

### Changed

- Make `src/` and `tests/` English-only. Every behavioural keyword table that
  used to carry Chinese entries is now English with a language-neutral
  fallback, so no natural language is privileged over any other: capability
  routing, router intent classification, automix confidence, the channel
  pre-gate's interrogative and social-noise tables, memory topic/greeting
  classification, the memory autosave blocklist and the sensitive-content
  patterns. Where a table no longer reads a request, the existing default
  applies - the pre-gate falls through to its classifier and fails open, the
  router returns its default category, automix keeps its base confidence, and
  capability routing stays unrouted, which publishes the caller's fallback
  surface rather than trimming on a guess. The pre-gate answer parser gained
  explicit negated phrases ("should not reply", "no need to reply") so a
  verbose negative no longer parses as the affirmative token it contains.
  User-visible copy, test fixtures, assertion messages and comments are
  English too; fixtures that need multi-byte input use language-neutral
  substitutes. A new `architecture_boundaries` guard fails the build on any CJK
  character in `src/` or `tests/`, literal or `\u{...}`-escaped.

## [0.8.121] - 13 September 2026

### Fixed

- Make the Redux request-context wiring guard tolerate multi-line builder
  chains instead of asserting one exact source line.

## [0.8.120] - 13 September 2026

### Fixed

- Stop one unreadable turn from pinning a chat session at the whole tool
  catalog. Capability routing falls back to the full registry when its
  English-only keyword table activates no category, which is right for an entry
  point that keeps no memory between turns but wrong for chat: the session's
  cumulative exposure absorbed that fallback, so a single greeting, a closing
  "thanks, that is all", or any non-English turn widened the session to every
  tool and held it there for good. A captured session went 14 tools, 16 tools,
  then 43 tools (49 KB of schemas) on a closing pleasantry, and stayed at 43.
  `select_tools_for_intent` now returns `RoutingOutcome::Unrouted` instead of a
  set and the entry point picks the fallback through `UnroutedToolPolicy`:
  channels, gateway, console, delegated and worker turns still publish
  everything operator policy allows, while a chat session republishes the set it
  already had. A session whose *first* turn is unrouted still gets the whole
  registry — there is no earlier decision to keep — so a non-English opener
  loses nothing. `always_exclude` continues to outrank every fallback.

## [0.8.119] - 13 September 2026

### Fixed

- Tell the user when context is dropped without a recoverable handoff. A
  `switch` rollover that cannot resolve exact transcript provenance answers the
  turn on a token-aware trim instead of ending it, but the redux chat driver
  only logged the downgrade: the session kept replying while its oldest messages
  were deleted with no summary and no event to look them up with. The driver now
  dispatches `HistoryCompactionDegraded`, which the reducer turns into one line
  (`Context trimmed (lossy): N older messages dropped …`) in the transcript and,
  when no renderer is attached (`--plain`, piped), on stdout. Repeats inside one
  turn are collapsed, so a turn that degrades on every tool iteration still
  prints a single line.
- Record the durable `context.compaction.degraded` event on the redux driver
  rollover as well. The 0.8.115 entry claimed all three degradation paths wrote
  it; only the two inside the shared tool loop did. The event is written once per
  degradation and only when a compaction audit runtime is attached, so sessions
  with no durable transcript scope add no writes.
- State in the `switch` handoff note how much tool traffic it folds away. The
  note is a list of event ids, and tool round-trips never became events of their
  own, so an agentic window handed over a pointer list that silently omitted its
  tool output. The note now carries `folded_tool_roundtrips` (count plus the
  tools called) and a plain sentence saying that output is not recoverable in
  this window. Tool results are still not persisted as their own events — doing
  so would add a synchronous write per tool call — so the note discloses the gap
  rather than closing it.

### Changed

- Hold every action-dispatched tool schema to one enforceable contract.
  `sessions_spawn`, `cron`, `xin`, `message_send` and `wasm_plugins_manage` are
  now checked against a shared assertion derived from each schema's own
  conditional-required clauses: a parameter an action cannot be called without
  has to name that action, and no parameter may ship without a description.
  `cron`, `xin`, `message_send` and `wasm_plugins_manage` are brought into
  compliance — `cron` had stopped naming `history`, `events` and `cancel` in
  `job_id` at all, and eleven `wasm_plugins_manage` parameters carried no
  description.

## [0.8.118] - 13 September 2026

### Fixed

- Keep the skill section of the system prompt stable across the turns of one
  session. The section is rebuilt from a relevance ranking of the current user
  message and sits ahead of the identity and runtime sections, so every
  rephrasing invalidated the provider's cacheable prefix for the whole
  conversation — the tool-catalog work in 0.8.113 stabilized a later section
  than the one that actually moved. A catalog of at most `[skill_rag] top_k`
  skills is now published whole and rendered in a canonical, de-duplicated
  order, which makes the section a pure function of the installed skills; the
  default `top_k` is raised from 5 to 32 because the section carries metadata
  only. Oversized catalogs still fall back to retrieval.
- Route terminal chat on the raw user text. The chat driver passed no routing
  input to the tool loop, so routing fell back to the last history message —
  which chat has already enriched with memory recall and the
  `[Recent shared workspace events]` block. Any participant could therefore
  widen the published tool surface by posting a URL into the shared workspace.
  Every other entry point already pinned the raw text.

### Changed

- Publish one cumulative tool set per chat session. Intent routing is a per-turn
  decision, but the `tools` array is part of the provider's cacheable request
  prefix: re-deciding it on every turn re-prefilled the whole conversation each
  time the user rephrased, which cost more than the tool schemas it saved. A
  chat session now exposes the union of every set routed so far, through the
  existing `[tool_tiering] always_include` knob, so the prefix moves only the
  first time a new capability is named. `/new` and `/clear` reset the union;
  channel, gateway, console and worker turns are one-session-per-turn and are
  unaffected.
- Intent keywords are English-only, with a language-neutral fallback. The
  classifier table no longer carries non-English entries, and the English side
  gained the words the removed ones covered (`store`, `save`, `screenshot`,
  `voice`, `audio`, `webpage`, `repository`, `picture`, `recurring`, `send`).
  A message that activates no category is now unrouted: the whole registry is
  published instead of the core floor, because a keyword table that cannot read
  the request is not evidence for removing capabilities from it.
  `always_exclude`, `channel_exclude` and `model_allowlists` are unchanged and
  still outrank the fallback.

## [0.8.117] - 13 September 2026

### Fixed

- Satisfy strict clippy gates in the prompt cache, thinking progress, and
  tool surface budget tests.

## [0.8.116] - 13 September 2026

### Changed

- Route capabilities on the terminal chat driver. The redux chat turn owner
  passed no `[tool_tiering]` policy to the shared tool loop, which does not mean
  "use the defaults" — it disables intent routing outright, so `prx chat`
  re-serialized the entire registry into every request of every iteration while
  channels, gateway, console, session workers, `sessions_spawn` and `delegate`
  all routed normally. Terminal chat is back on the same boundary; a tool that
  routing drops is restored by name through `[tool_tiering] always_include`.
- Hide the operations/development tool surface from IM channel sessions by
  default, through a new `[tool_tiering] channel_exclude` list:
  `wasm_plugins_manage`, `wasm_plugin_reload`, `wasm_plugins_status`,
  `hooks_manage`, `hooks_status`, `proxy_config`, `config_reload`, `gateway`,
  `mcp_status`, `nodes`, `git_operations`, `skills_manage`, `memory_reindex`,
  `document_sync`, `document_ingest`. The list is folded into that turn's
  `always_exclude`, so those tools stay hidden even when the message names their
  capability. Terminal chat, the gateway/webhook surface and the web console are
  unaffected. `always_include` still wins per tool, and `channel_exclude = []`
  turns the default off.
- Compress the tool schemas the model pays for on every turn. `sessions_spawn`,
  `cron`, `message_send`, `xin` and `wasm_plugins_manage` carried per-action
  prose that restated the same action/parameter mapping in four places; the
  descriptions are condensed while field names, types, enums, defaults and the
  conditional required-field clauses stay byte-for-byte the same. The
  schema-contract tests that require every action to be named where the model
  reads it only covered `sessions_spawn` at this point, and `cron` left the
  compression no longer naming its actions; 0.8.119 generalizes the contract and
  brings the other four schemas back into compliance.

## [0.8.115] - 13 September 2026

### Fixed

- Unblock `switch` context compaction for sessions that contain tool calls.
  Transcript provenance refused to resolve whenever the cold window held a
  `role="tool"` result, the assistant envelope carrying native `tool_calls`, or
  the prompt-mode `[Tool results]` block, because none of those are persisted as
  their own `message.created` event. They are now folded into the assistant turn
  that produced them, so a real agentic session can roll over instead of
  answering every message with the same non-retryable
  `context switch could not reduce the provider window` error.
- Never end a turn because a hard switch cannot stay lossless. The pre-provider
  budget check, the provider-overflow retry, and the redux driver rollover now
  fall back to the token-aware trim instead of failing the draft. The two paths
  inside the shared tool loop also record a durable
  `context.compaction.degraded` event; the redux driver rollover only logged the
  downgrade until 0.8.119.
- Stop compaction and lossy trims from leaving a tool result without the
  assistant message that requested it. The compaction boundary walks back past a
  tool round-trip, and every trim drops tool results stranded at the cut, so the
  retained window stays a transcript providers accept.

## [0.8.114] - 13 September 2026

### Added

- Show live thinking progress in the chat TUI while a reasoning model streams.
  Reasoning deltas now reach the reducer as `StreamReasoningReceived`, and the
  pre-first-token activity line reports `Thinking (elapsed - N chars (~M tok) -
  Esc to interrupt)` with a one-line preview of the newest reasoning text
  instead of an opaque spinner for the whole 4-40s thinking burst. Reasoning
  deltas share the text-delta version counter, so stale and reordered deltas are
  dropped by the same strict-monotonic guard.
- Batch reasoning deltas behind a 120ms window so the fine-grained reasoning
  stream cannot redraw faster than the visible text stream.

## [0.8.113] - 13 September 2026

### Fixed

- Keep the system prompt prefix stable across turns of one session. The
  `## Runtime Capabilities` inventory and the prompt-guided tool catalog are now
  rendered from a canonical snapshot (name-sorted, one entry per name), so two
  turns that expose the same tools produce byte-identical system prompts and the
  provider-side prefix cache keeps hitting instead of missing whenever intent
  routing returned the same tools in a different order.

### Changed

- Memoize the compiled tool snapshot's rendered prompt sections and SHA-256
  fingerprint for the duration of a turn. The snapshot is still rebuilt on every
  iteration so dynamic backends cannot go stale, but an unchanged tool set is no
  longer re-rendered and re-hashed per iteration. The compiled request-context
  event and the `openprx::prompt` debug record now report the render cache's hit
  and miss counts.
- Canonicalize the provider-native tool array with the same ordering, so the
  tool list sent to native providers is also stable for a given exposed set.

## [0.8.112] - 13 September 2026

### Changed

- Remove the learning subscore from fitness weighting until PRX records a
  durable retrieval outcome linked to a later terminal result. Normalize the
  remaining four weights to full coverage and derive Final status from the
  configured `min_coverage`, so Final reports are structurally reachable.

### Fixed

- Reject `self/fitness/*` at the shared memory-write validator for SQLite,
  Lucid, Markdown, PostgreSQL, and WASM-backed persistent stores.
- Report legacy fitness row and projection counts in `prx doctor`, and support
  equivalent export-plus-transactional-delete migration for PostgreSQL.
- Attribute delayed evolution outcomes to the three closed fitness windows
  immediately after a change instead of the newest three reports in storage.

## [0.8.111] - 12 September 2026

### Changed

- Publish and document one explicit inventory of 54 static public tools plus
  bounded Skill, MCP, and WASM aliases.
- Project canonical tool schemas through the shared Gemini schema cleaner on
  both native serialization paths.

### Fixed

- Align executor and schema contracts for alternative inputs, action-specific
  messaging, Xin task updates, cron one-shot scheduling, and integration
  parameter constraints.
- Extend the canonical contract validator to schema-valued additional
  properties and exclusive alternatives. (Corrected in 0.8.123: the validator
  was reachable only from tests until runtime enforcement landed.)

## [0.8.110] - 12 September 2026

### Changed

- Add a canonical tool-argument contract layer that publishes action-specific
  requirements, alternative field sets, nested requirements, scalar and union
  types, enums, lengths, numeric bounds, and unknown-field rejection.
  (Corrected in 0.8.123: this release published those contracts to providers
  but did not enforce them at runtime; the production pipeline still checked
  only the root `required` list.)
- Give OS-process session workers the same general agentic orchestration
  surfaces as task-mode children, including sessions, sub-agent management,
  daemon messaging, image analysis, configuration reload, and Gateway controls,
  while preserving scope and approval enforcement.
- Stop reapplying request-local intent tiering inside process workers after the
  parent has explicitly selected and sealed the worker tool boundary.

### Fixed

- Prevent multi-action tools such as Xin, cron, MCP, Skills, Hooks, sessions,
  nodes, Git, Composio, and messaging from advertising structurally incomplete
  calls as valid.
- Preserve multiple explicitly allowed MCP, WASM, or Skill aliases backed by a
  single dynamic router when selecting process-worker capabilities.
- Refresh dynamic registries before resolving explicit child allowlists so a
  configured alias is not rejected merely because discovery had not run yet.

## [0.8.109] - 12 September 2026

### Changed

- Unify runtime autonomy guidance and request-local tool discovery: `TOOLS.md`
  now holds only environment notes, while the final `ToolSpec` snapshot supplies
  the exact executable capability list to every provider request.
- Stop injecting the preserved first-run `BOOTSTRAP.md` after
  `workspace/state/bootstrap.completed` is created.
- Replace memory-backed fitness reports with versioned, atomic daily snapshots
  under `workspace/self/fitness`, calendar scheduling over closed local days,
  evidence-derived optional subscores across root and process-worker events,
  explicit coverage/confidence, and real fitness trends for delayed evolution
  validation.

### Fixed

- Scope root provider credentials and URLs to their configured provider so CLI
  overrides and delegated agents resolve the target provider's route credential
  or active auth profile instead of inheriting an unrelated provider secret.
- Stop fitness telemetry from entering general recall, `MEMORY.md`, and
  `MEMORY_SNAPSHOT.md`; add a backed-up transactional migration command for
  existing `self/fitness/daily/*` memory rows and Markdown projections.

### Tests

- Cover daemon catch-up and idempotent skip behavior, efficiency medians,
  proactive job classification, fitness aggregation thresholds, PostgreSQL
  descendant event visibility, and PostgreSQL legacy migration SQL.

## [0.8.108] - 8 September 2026

### Fixed

- Declare the generated-binding `wit-bindgen` dependencies in all Rust PDK
  examples as intentional cargo-machete false positives, restoring the unused
  dependency CI gate without removing dependencies required to rebuild them.

## [0.8.107] - 8 September 2026

### Fixed

- Satisfy the strict CI lint gate for model allowlist helpers and WASM schema
  regression assertions without weakening the project-wide Clippy policy.

## [0.8.106] - 8 September 2026

### Security

- Upgrade `wasmtime` and `wasmtime-wasi` from 45.0.3 to 46.0.3 to resolve
  RUSTSEC-2026-0269, a high-severity WASI filesystem sandbox escape involving
  paths or symbolic links with trailing slashes.
- Refresh `chacha20` from the yanked 0.10.0 release to 0.10.2.
- Remove obsolete audit suppressions for advisories already resolved by the
  `lettre` and `wasmtime` upgrades.

## [0.8.105] - 8 September 2026

### Added

- Add exact-model tool allowlists under `tool_tiering.model_allowlists`, so
  smaller local models receive a bounded native-tool schema and matching
  prompt catalog without reducing the tool surface available to larger models.

## [0.8.104] - 8 September 2026

### Security

- Upgrade `lettre` from 0.11.19 to 0.11.23, removing the dependency version
  affected by RUSTSEC-2026-0141 while preserving the Rustls-only SMTP backend.

## [0.8.103] - 4 September 2026

### Fixed

- Register interruptible Telegram turns in receive order so an older task can
  never cancel a newer message because of Tokio scheduling order. Completion
  now also wakes the successor if a turn exits unexpectedly.
- Keep `HEARTBEAT.md` out of interactive channel prompts, preventing spurious
  heartbeat acknowledgements in normal conversations.
- Classify `openprx`, `prx`, and `vano` topics as PRX before checking the
  overlapping `openpr` alias.
- Persist quick-setup workspace selection beneath the explicitly supplied home
  directory instead of leaking state into the process user's real home.
- Preserve config-generation audit events emitted by filesystem watcher threads
  by scheduling them on the gateway's captured Tokio runtime.
- Normalize filesystem-watcher paths before config relevance checks and expose
  watcher readiness, so macOS `/var` aliases cannot drop hot-reload events.

### Tests

- Restore auto-save, workspace prompt injection, project inference, quick setup,
  webhook key uniqueness, and Chat session resume regressions to the default
  test suites.

## [0.8.102] - 4 September 2026

### Fixed

- Normalize macOS `/var` and `/private/var` aliases before enforcing
  workspace ownership, while preserving symlink-escape and traversal checks
  for media artifacts and evolution rollback writes.
- Reliably reap and classify macOS process groups and zombies after session
  cancellation, including detached descendants and PTY-backed commands.
- Run WASM plugins through one process-shared engine and use Wasmtime's
  POSIX-signal trap backend on macOS. Concurrent shell/session-worker activity
  can no longer abort the test process from Wasmtime's Mach-port handler.

### Tests

- Make provider deadline, Lucid context merge, process cleanup, and WASM
  resource-limit coverage deterministic under the default parallel test runner.

## [0.8.101] - 4 September 2026

### Fixed

- Give `sessions_spawn(mode="process")` workers the same runtime system
  prompt contract as Chat: exposed tools, workspace, skills, safety, and
  native-tool guidance are now present before agent-specific instructions.
  Workers no longer rely on a short generic delegate prompt that allowed a
  tool-capable model to return an unverified prose-only completion.

### Tests

- Assert that a worker prompt retains both the Chat tool contract and its named
  agent instructions.

## [0.8.100] - 4 September 2026

### Fixed

- Preserve a named delegate agent's non-empty system prompt when a
  `sessions_spawn(mode="process")` worker is created. The signed worker
  manifest now carries the same execution contract as task-mode delegation.

### Tests

- Assert that a process worker manifest retains the resolved agent prompt.

## [0.8.99] - 4 September 2026

### Fixed

- Publish structurally valid WASM tool schemas so strict function-calling
  providers, including K3, can call `wasm_plugins_manage`.
- Keep default-provider credentials and base URLs out of process-mode child
  agents that explicitly select a different provider. Qwen-parent/K3-child
  runs now use the K3 auth profile instead of receiving the Qwen key.

### Tests

- Cover WASM schema validity and process-worker credential isolation, then
  verify a full-screen Chat Qwen-to-K3 OS-process delegation end to end.

## [0.8.98] - 3 September 2026

### Fixed

- Pin capability routing to the exact user-authored turn input before recalled
  memory, shared events, middleware, or provider-history enrichment. TUI, CLI,
  Gateway, WhatsApp/other IM channels, and process session workers can no longer
  inherit stale MCP/WASM intent from contextual text.

### Tests

- Prove that positive MCP intent in recalled context cannot override a current
  Skill-only request that explicitly excludes MCP and browser tools.

## [0.8.97] - 3 September 2026

### Fixed

- Treat explicit negative capability instructions such as "do not use MCP" and
  "不要调用浏览器/MCP" as exclusions during intent routing. Forbidden dynamic
  capabilities no longer cold-start merely because their names appear in the
  user's constraint, while a later positive contrast still activates them.

### Tests

- Cover English and Chinese negative instructions, positive contrast clauses,
  and the shared model loop's dynamic refresh boundary.

## [0.8.96] - 3 September 2026

### Fixed

- Coalesce request-local system context into the leading system message before
  every provider call. Empty-response and tool-summary corrective retries,
  OS-paging recall, and WASM request middleware can no longer create an invalid
  mid-conversation system role for strict templates such as Qwen.

### Tests

- Exercise an empty-response retry against a provider that rejects every system
  message outside position zero, while proving the correction stays out of
  persisted conversation history.

## [0.8.95] - 3 September 2026

### Fixed

- Route each model turn before refreshing dynamic capabilities, so unrelated
  Skill, document, and ordinary chat requests no longer wait for cold MCP or
  WASM backends. Explicit MCP/WASM intents still refresh before their current
  schemas are exposed. The shared loop applies this ordering to TUI, CLI,
  Gateway, WhatsApp/Wacli, and every other IM channel.

### Tests

- Prove that a Skill/PDF turn does not refresh an MCP capability while an
  explicit MCP/browser turn still does.

## [0.8.94] - 3 September 2026

### Fixed

- Parse `name` and `description` from `SKILL.md` YAML frontmatter for installed
  workspace skills instead of exposing the `---` delimiter as their catalog
  description.
- Keep both Markdown and TOML skill catalog snapshots metadata-only and report
  every enabled skill as lazy; `skill_read` is the single instruction hydration
  path across TUI, CLI, Gateway, WhatsApp/Wacli, and other IM channels.

## [0.8.93] - 3 September 2026

### Changed

- Keep installed skill instructions out of the reusable system prompt and load
  `SKILL.md` through `skill_read` only when a skill is selected. TUI, CLI,
  Gateway, WhatsApp/Wacli, and other IM channels now share this metadata-only
  catalog behavior while retaining declared skill tool metadata.

### Fixed

- Remove completed-turn elapsed-time lines from the TUI transcript and status
  event bridge while preserving live working elapsed time and per-tool timing.

### Tests

- Exercise 1,000 progressing model/tool iterations without an artificial turn
  ceiling, alongside the persistent MCP and WASM 1,000-call stress regressions.

## [0.8.92] - 3 September 2026

### Fixed

- Resolve MCP, WASM, and skill aliases that are discovered after the unified
  execution service captures its startup catalog, requiring both current
  backend support and a live advertised spec before dispatch.

## [0.8.91] - 3 September 2026

### Fixed

- Preserve absolute installer `PATH` entries in generated launchd and systemd
  user services, so daemon-owned IM, gateway, hook, and scheduled turns can
  launch the same stdio MCP commands (`npx`, `uvx`, `bun`, and similar) as the
  interactive CLI.

## [0.8.90] - 3 September 2026

### Fixed

- Keep one ordinary main provider turn out of the TUI worker/session chrome, so
  internal command summaries such as `run npx ...` cannot appear below the
  composer; concurrent turns and explicit worker diagnostics remain visible.
- Separate transcript output from the gray status bar with a stable blank row
  and use the same neutral filled marker for completed tools and assistant text.
- Expose the live hook status/management tools in standalone `process_message`
  and process-isolated session-worker entrypoints.
- Make `allowed_tools = ["*"]` actually inherit the complete tool registry in
  task and process sub-agents, preserving dynamic MCP/WASM alias specs and
  named-call dispatch through the proxy boundary.
- Let explicit delegated allowlists select one dynamic MCP/WASM alias without
  exposing sibling aliases, and share the live Hooks/WASM control objects with
  in-process delegates across every model-running entrypoint.

## [0.8.89] - 3 September 2026

### Added

- Execute middleware, cron, provider, and storage WASM adapters through the
  current atomic plugin generation, with direct operational probes and real
  component fixtures.
- Resolve explicit `wasm:` providers and memory backends in chat, agent CLI,
  gateway, IM channels, daemon tasks, delegates, and session workers.

### Fixed

- Run all four middleware stages in the shared agent loop used by TUI, CLI,
  gateway, and IM delivery, isolating malformed plugin output.
- Publish action-specific required fields in the WASM management tool schema.

## [0.8.88] - 3 September 2026

### Fixed

- Make documented WASM hook manifests load by accepting both string event lists
  and `[[capabilities.events]]` pattern tables.
- Dispatch canonical `prx.lifecycle.*` topics to WASM hooks, support exact,
  wildcard, and legacy short-name matching, and expose invocation diagnostics.
- Align middleware, hook, and cron host registration with the canonical PDK KV
  result ABI and complete HTTP, memory, events, WebSocket, and WASI surface.
- Make the Rust `audit-hook` example build as a real component and connect live
  memory backends to non-tool adapters.

## [0.8.87] - 3 September 2026

### Added

- Add model-visible Hooks status and lifecycle management for validation,
  atomic replacement, refresh, recoverable removal, and synthetic event tests.
- Add model-visible WASM plugin install, update, enable, disable, refresh,
  inspection, and recoverable removal across chat, agent, gateway, and channel
  entrypoints.

### Fixed

- Register the shared WASM plugin runtime in `prx chat` and the runtime
  bootstrap instead of exposing plugin tools only in daemon/channel paths.
- Match the versioned WIT logging ABI's enum parameter so real plugin
  components instantiate and execute instead of appearing loaded but unusable.
- Surface isolated WASM adapter construction failures in plugin status and
  reject or roll back lifecycle changes that do not produce an operational
  adapter.
- Reuse the executing HookManager in chat and agent loops so control-plane
  diagnostics and changes address the same live hook generation.

## [0.8.86] - 3 September 2026

### Changed

- Make `/new` save the current chat and create a fresh session identity instead
  of aliasing `/clear`; `/clear` now explicitly keeps the current session.

### Fixed

- Reset the visible title, turns, context budget, token usage, child-session
  summaries, and pending interaction state when starting a new chat session.
- Defer `/new` and `/clear` behind an active provider turn so its ordered
  completion and durable save cannot cross the session boundary.

## [0.8.85] - 3 September 2026

### Fixed

- Hide cumulative provider-worker payload/token counters after all workers are
  terminal so completed turns and `/new` no longer display stale `wtok` state.

## [0.8.84] - 3 September 2026

### Fixed

- Enforce detached MCP child stderr through `rmcp`'s transport builder; its
  convenience constructor otherwise overwrote the command-level setting and
  continued leaking `npx` notices into the TUI.

## [0.8.83] - 3 September 2026

### Changed

- Align assistant and tool-call transcript hierarchy with Codex-style neutral
  filled markers, action wording, branch glyphs, and block spacing.
- Bound MCP output inserted into model history to 16K characters per call while
  retaining oversized results through the existing document reference path.

### Fixed

- Detach stdio MCP child stderr from the interactive terminal so `npx` and
  package-manager notices cannot overwrite the TUI input or status rows.
- Retry an empty model response once with a request-local corrective instruction
  and fail honestly if the retry is also empty instead of completing a false
  successful turn.
- Republish main-queue status after immediate dequeue so fast slash commands
  such as `/new` cannot leave a stale `queue:1` badge behind.

## [0.8.82] - 3 September 2026

### Fixed

- Keep one stateful stdio MCP client alive per configured server so browser and
  other session-oriented MCP tools retain context across separate calls.
- Serialize calls to the same stdio server, close cached sessions on explicit
  refresh or configuration changes, and reset failed transports without
  replaying potentially side-effecting calls.

## [0.8.81] - 3 September 2026

### Added

- Added approval-gated `skills_manage` lifecycle actions for creating,
  installing, updating, enabling, disabling, validating, synchronizing, and
  removing skills, with matching CLI update/enable/disable commands.
- Added executable `skill_execute` support and bounded dynamic aliases for
  declared shell, script, and HTTP skill tools.

### Changed

- Added a first-class `skill` tool adapter so declared skill tools use the same
  provider catalog and execution service as native, MCP, and WASM tools.
- Persist disabled workspace skills without deleting them and keep them visible
  to control-plane inventory while excluding them from prompts and execution.

## [0.8.80] - 3 September 2026

### Added

- Added catalog-backed `skills_list` and path-confined `skill_read` tools for
  workspace and lazy community skills.
- Added approval-gated `document_ingest` and stable-source `document_sync`
  tools for the durable document store.
- Added MCP status/error discovery and WASM plugin status/reload/root-call
  tools, with deterministic bounded alias exposure.

### Changed

- Extended the canonical tool catalog with backend-reported availability and a
  distinct WASM adapter kind; declared-only capabilities are no longer exposed
  to providers as executable ToolSpecs.
- Applied the same per-turn intent selection to native and prompt-guided tool
  catalogs, and refresh dynamic backends before each catalog snapshot.
- Rebuilt Document FTS together with Memory FTS during `memory_reindex`.
- Replaced line-number-based Markdown hydration and line splitting with
  CommonMark block parsing, nested heading paths, stable content anchors, and
  atomic list/code/table/frontmatter handling.

### Fixed

- Blocked SSRF/private-network targets and out-of-allowlist domains in
  `http_request`, including legacy numeric IP spellings, and bounded the body
  while streaming instead of after unlimited buffering.
- Retained MCP configuration/discovery failures for diagnostics and retried
  failed discovery with bounded backoff instead of silently treating a failed
  first attempt as initialized forever.
- Canonicalized the workspace root before applying ACL protection to Memory
  Markdown and SQLite files, closing a symlinked-temporary-directory bypass.
- Preserved dynamic specs, refresh hooks, cancellation dispatch, recipient
  routing, and runtime availability through the shared `Arc` tool adapter.

## [0.8.79] - 2 September 2026

### Added

- Completed Xin task management with editable tasks, immediate execution,
  retained run history, explicit cancellation, trusted-owner isolation, and a
  `prx xin` CLI.
- Exposed durable Goal/Step authoring and operations through both the `xin` LLM
  tool and CLI, including pause, resume, cancellation, removal, append, and
  ordered retry.

### Changed

- Retained completed user and agent Xin tasks until explicit removal so their
  execution results and run history remain inspectable.
- Active Xin task and Goal/Step leases now observe operator cancellation within
  one second and stop through the existing cooperative cancellation path.
- Cancelled active Xin tasks now retain a `cancelled` run-history record for
  post-run inspection.

## [0.8.78] - 2 September 2026

### Fixed

- Removed the five-minute wall-clock timeout from summary compaction. Slow
  local inference is now awaited and falls back to deterministic trimming only
  on a real provider error.
- Removed fixed context-overflow retry counts from Agent, Chat, Redux TUI and
  channel execution paths. Retries now continue while compaction measurably
  reduces history and stop only when no further progress is possible.
- Removed the hidden 1,000-message mid-turn trim and retired
  `agent.max_history_messages`; context retention is now governed by the
  configured token budget rather than arbitrary message counts.
- Calculated proactive compaction and OS-paging thresholds consistently from
  the usable input window after response-token reservation.
- Identified cl100k token measurements as a proxy instead of reporting them as
  the active model's real tokenizer.

## [0.8.77] - 2 September 2026

### Fixed

- Kept failed Xin tasks enabled and recurring work scheduled instead of
  silently disabling it after a persisted failure-count threshold.
- Made failed Goal/Step attempts return to the pending queue without a hidden
  retry ceiling; retry counters remain available for observability.
- Excluded Xin and cron lifecycle records from sub-agent session recovery, so
  scheduled work no longer appears as a process-like session.
- Reported whether memory hygiene actually ran or was skipped by its cadence
  gate instead of claiming completion for both outcomes.

### Changed

- Added Goal and Step state counts to `xin status`, separate from recurring
  task/loop counts.
- Removed the unused namespace due-task query that still accepted a batch
  limit, and retained legacy failure-cap database fields as ignored
  compatibility data.

## [0.8.76] - 2 September 2026

### Fixed

- Stopped Xin from registering duplicate fitness and memory-evolution schedules;
  existing persisted duplicates are disabled while their internal handlers stay
  available for backward-compatible goal recovery.
- Reported rescheduled Xin and cron work as pending instead of presenting it as
  a running, unmanageable sub-agent session.
- Changed generated core-memory Markdown backups to update by key and collapse
  stale generated duplicates, preventing repeated fitness reports from growing
  every future system prompt.

### Changed

- Removed the `xin.max_concurrent` and `xin.max_tasks` ceilings. Due work now
  starts without an arbitrary configuration cap, while leases, heartbeats,
  checkpoints and explicit cancellation continue to provide crash safety.
- Reduced the default Xin maintenance set to health checks, stale cleanup and
  memory hygiene; dedicated self-system workers remain the sole owners of
  fitness reporting and memory evolution.

## [0.8.75] - 2 September 2026

### Changed

- Removed the main-agent, chat, delegated-agent and spawned-session tool-round
  ceilings. Progressing work now continues until completion, explicit failure
  or user cancellation; the TUI reports `Step N` without a false denominator.
- Retired `agent.max_tool_iterations`, per-agent `max_iterations`, and the
  `sessions_spawn` `max_iterations` argument. Existing configuration keys are
  ignored with a migration warning instead of stopping startup.
- Removed wall-clock deadlines from delegated model calls, task-mode
  sub-agents, process-isolated session workers, and shell subprocesses. The
  `sessions_spawn.timeout_seconds` argument and the 24-hour
  `runtime.idle_hang_max_total_secs` ceiling are retired; active work now ends
  only through completion, a real error, no-progress hang detection, or an
  explicit cancellation such as `prx tasks kill`.

## [0.8.74] - 2 September 2026

### Fixed

- Preserved plain UTF-8 text while enabling terminal keyboard enhancements so
  Chinese and other IME-composed input renders correctly in the full-screen
  chat composer, including direct iTerm2 and tmux sessions.

## [0.8.73] - 2 September 2026

### Fixed

- Redirected chat tracing before configuration loads so routine startup logs
  remain in `~/.openprx/chat.log` instead of reappearing in the terminal when
  the full-screen TUI exits.

## [0.8.72] - 2 September 2026

### Fixed

- Removed the post-response `Thinking` card from the primary TUI transcript so
  each turn ends with a single `Worked for` summary and the assistant answer.
  Reasoning data remains available in the verbose transcript, and hidden
  reasoning no longer intercepts the tool-card `Tab` shortcut.

## [0.8.71] - 2 September 2026

### Changed

- Replaced the empty assistant waiting cursor with transcript-native, animated
  turn activity and elapsed time; running tools now own their spinner, completed
  turns retain a compact duration summary, and the pinned status bar no longer
  duplicates generation or turn-count state.
- Updated the main TUI composer prompt to `›` while retaining `>` for ASCII
  fallback and explicit labels for attached session targets.

## [0.8.70] - 2 September 2026

### Fixed

- Removed the OpenAI-compatible buffered-response two-minute deadline and
  stopped replaying ambiguous transport failures through the Responses API.
- Added WhatsApp typing and paused presence updates to long-running wacli turns.

## [0.8.69] - 31 August 2026

### Added

- Added an explicit wacli Message Yourself mode that accepts linked-account
  messages as owner input and uses wacli's opt-in self-send path for replies.

### Fixed

- Correlated self-chat replies with wacli's returned outbound message IDs so
  webhook echoes cannot re-enter the agent as new commands, including the race
  where an echo arrives before the send subprocess returns.

## [0.8.68] - 31 August 2026

### Fixed

- Included configured wacli channels in `prx channel list`, `prx channel doctor`,
  and `prx status`, and documented wacli in the channel command help.

## [0.8.67] - 31 August 2026

### Fixed

- Prevented healthy OpenAI-compatible SSE generations from being terminated by
  the non-streaming two-minute whole-response timeout.
- Prevented Chat and Agent streaming paths from replaying a request after any
  text, reasoning, or tool-call output has already been emitted.

## [0.8.20] - 26 July 2026

### Added

- Added pre-built-first `install.sh` and `install.ps1` entry points with exact
  version selection, SHA-256 verification, and atomic binary installation.
- Added installer and Rust toolchain contract gates to normal CI and the release
  workflow.

### Changed

- Aligned the declared, local, CI, audit, release, and Docker Rust toolchains on
  Rust 1.97.1.
- Updated the README and installation/configuration references to the real
  `prx` CLI and current release asset names.

### Fixed

- Removed host-shell ACL, command-text policy, OS sandbox, environment clearing,
  and synthetic PATH enforcement from interactive ShellTool, background shell,
  PTY, Cron shell, and Xin shell execution paths. Direct commands now inherit
  the parent environment and support normal host paths, `/dev/null`, variables,
  substitutions, pipelines, and background-process bookkeeping.
- Removed the obsolete `[autonomy.sandbox]` schema/templates and Landlock build
  feature so deleting the old switch cannot silently activate isolation.

## [0.8.12] - 17 July 2026

### Fixed

- Kept synchronous PostgreSQL Cron operations and client shutdown off Tokio
  runtime threads, preventing deployed `cron` commands and scheduler calls from
  panicking while initializing or querying the shared PostgreSQL store.
- Made every release platform required, failed artifact uploads on missing
  files, and added a 20-file completeness assertion before publication.
- Added a Windows MSVC check to normal Rust CI so Windows compile failures are
  caught before an immutable release tag is created.

### Release status

- `v0.8.11` published a complete cross-platform GitHub Release, but its Stage 5
  deployment was rolled back after the isolated PostgreSQL legacy-Cron check
  reproduced the Tokio runtime panic. Production returned to healthy 0.8.7.

## [0.8.11] - 17 July 2026

### Fixed

- Restored the Windows release build by making the OpenRC shell-quoting helper
  available to the platform-independent script renderer.
- Prevented the release job from publishing incomplete assets when a required
  platform build fails.

### Release status

- `v0.8.10` was stopped before deployment after its Windows build failed. Its
  incomplete GitHub Release was removed and its immutable tag retained. Upgrade
  directly from the deployed 0.8.7 baseline to 0.8.12.
- `v0.8.11` was later stopped during Stage 5 and rolled back after its isolated
  PostgreSQL Cron deployment check exposed a Tokio runtime panic.

## [0.8.10] - 17 July 2026

### Fixed

- Made Cron's SQLite schema upgrade add the legacy `cron_runs.attempt_id` and
  `worker_id` columns before creating the attempt-identity index. Existing
  0.8.7 workspaces now initialize the scheduler without losing run history.

### Release status

- `v0.8.8`, `v0.8.9`, and `v0.8.10` were stopped before deployment. 0.8.9 exposed
  this legacy Cron upgrade-order defect during Stage 5 local-binary deployment;
  production was rolled back atomically and remained on 0.8.7. The 0.8.10
  Windows release build later failed, so its incomplete GitHub Release was
  removed without moving its tag. Upgrade directly from 0.8.7 to 0.8.12.

## [0.8.9] - 17 July 2026

### Fixed

- Preserved the immutable SQLite version-4 and PostgreSQL version-5 migration
  checksum anchors published by 0.8.7. New MessageEvent execution-metadata
  columns now have new migration registry versions instead of rewriting
  previously applied history, so read-only preflight and startup accept an
  existing 0.8.7 ledger.

### Release status

- `v0.8.8` was stopped before deployment after its production-ledger preflight
  found the checksum regression. Upgrade directly from 0.8.7 to 0.8.9.

## [0.8.8] - 16 July 2026

### Added

- One process-level `ConfigGenerationManager` now owns runtime configuration
  publication. Turns, messages, Cron/Xin work, webhooks, channels, and runtime
  events retain a typed, pinned configuration generation.
- Durable event, idempotency, usage/cost, process ownership, readiness, and
  recovery contracts now cover both SQLite and PostgreSQL-backed operation.
- Stage 9 runtime domains add bounded Nodes, a process-level Skill catalog,
  generation-owned Plugins/hooks, bounded media artifacts, and truthful
  provider routing/cost settlement.

### Changed

- Configuration reloads are transactional and field-aware. Snapshot-hot fields
  apply to newly admitted work, rebuild-and-swap fields publish only after
  candidate readiness, and process-only fields are reported as
  `restart_required` without falsely advancing active state.
- Chat and agent turns share authoritative tool execution, terminal commit,
  MessageEvent, approval, and usage/cost paths. Tool calls use durable
  reserve/execute/commit/replay semantics.
- Cron, Xin/Heartbeat, Channels, webhook, and self-system supervisors use
  generation fencing, readiness, rollback, and no-overlap replacement.
- `prx chat` now defaults to the fullscreen terminal UI on TTYs. Native terminal
  scrollback is no longer the chat transcript surface; use in-app scrolling while
  chatting and `/export [md|json]` to save a transcript. `PRX_TUI=0 prx chat`,
  `prx chat --plain`, and non-TTY stdin still use the plain reedline fallback.
  The previous `[chat].tui_mode` / `PRX_TUI_MODE` inline-vs-fullscreen selector
  is removed because fullscreen is now the only TUI renderer.

### Fixed

- Prevented partial terminal commits when workspace validation or usage/cost
  settlement fails; retries remain recoverable and idempotent.
- Scoped MessageEvent idempotency by workspace in both persistence backends.
- Enforced at-most-once tool execution across Chat and agent entry points.
- Made process kill, Cron leases, webhook ingestion, service status, health,
  readiness, migration diagnostics, and configured-backend selection truthful.

### Security

- **BREAKING** — `OPENPRX_APPROVAL_OVERRIDE` env unset 不再静默 auto-approve
  supervised 模式下的 tool approval。TUI 卡片 + Y/N 键盘 (T5-1 完整版) 留
  Task #11；在 UI 接通前 fail-safe deny。显式设置 `OPENPRX_APPROVAL_OVERRIDE=allow`
  可恢复旧行为；`deny` / `no` / `n` / `0` 与未设置一致拒绝。Codex S5 P0-3 反馈
  "绝不静默 auto-approve"。`autonomy_level=Full` 不走此路径，行为不变。

## [0.3.0] - 19 March 2026

### Added

- **Xin (心) autonomous task engine** — Configuration-driven heartbeat
  scheduler for system-level autonomous work.
  - 5 built-in system tasks: health check, stale cleanup, memory
    evolution, fitness report, memory hygiene
  - 3 execution modes: Internal (Rust fn), AgentSession (LLM),
    Shell (command)
  - SQLite-backed task persistence with execution history
    (`xin/tasks.db`)
  - LLM tool (`xin`) with 7 actions: list, add, get, remove, status,
    pause, resume
  - Configurable: interval, max_concurrent, max_tasks, stale_timeout,
    builtin_tasks
  - Evolution/fitness integration mode — xin can take over standalone
    schedulers
  - Supervisor with exponential backoff restart and health monitoring
- **Chat module** — Extracted conversational session management with
  named constants
- **Terminal channel** — Dedicated terminal-based messaging channel
- `SecretStore::decrypt_and_migrate()` — Auto-migrate legacy `enc:`
  to `enc2:` (ChaCha20-Poly1305 AEAD)
- `SecretStore::needs_migration()` / `is_secure_encrypted()` — Secret
  format detection
- **Telegram mention_only mode** — Bot only responds to @-mentions
  in group chats

### Security

- **26-finding comprehensive audit** — Full regression audit of 170K+
  LOC, all findings fixed:
  - (C-1) SQLite foreign keys enabled in memory backend
  - (C-2) Cron atomic job claiming prevents double-execution
  - (C-3) SSRF DNS rebinding defense with resolved IP validation
  - (H-1..H-8) Memory LRU eviction, content hash expansion (128-bit),
    tool argument schema validation, MCP debug log redaction, rate
    limiting for web_fetch/http_request
  - (M-1..M-11) Optimistic concurrency for xin/cron stores, magic
    number constants, flaky test serialization
  - (L-1..L-4) Code quality improvements
- **Web console hardening** — 9 additional fixes:
  - (C-1) Rate limiter time arithmetic safety
  - (C-2) Config dual-store atomic update (Mutex + ArcSwap)
  - (C-3) Upload path traversal defense — reject absolute paths
    and `..` components
  - (H-1) Auth middleware now supports cookie authentication with
    CSRF protection
  - (H-2) Skill install URL validation — strict host parsing
    prevents prefix bypass
  - (H-3) WebSocket log stream connection limit (max 64 concurrent)
  - (M-1) Extended sensitive key detection patterns
  - (M-3) Pagination clamp allows small page sizes
  - (L-1) API error responses no longer leak internal Rust error
    details
- **Legacy XOR cipher migration**: `enc:` prefix deprecated,
  auto-migrated to `enc2:`

### Fixed

- **Flaky proxy cache test** — Added `Mutex` serialization to prevent
  global cache race condition
- **Onboarding channel menu** — Enum-backed selector instead of
  hard-coded numeric match arms
- **OpenAI native tool spec** — Owned serializable structs for tool
  schema validation
- **Router audit fixes** — Provider reachability filtering, lock-safe
  async persistence, reserved `router/` namespace

### Deprecated

- `enc:` prefix for encrypted secrets — Use `enc2:`
  (ChaCha20-Poly1305) instead

## [0.2.1] - 11 March 2026

### Added

- **LLM Router Phase 1-5** — Delivered heuristic routing, capability
  registry, feedback loop updates, KNN semantic routing, and Automix
  adaptive escalation.

### Fixed

- **Router audit fixes** — Applied critical/high audit hardening for
  provider reachability filtering, lock-safe async outcome persistence,
  and reserved `router/` namespace enforcement.

## [0.1.0] - 13 February 2026

### Added

- **Core Architecture**: Trait-based pluggable system for Provider,
  Channel, Observer, RuntimeAdapter, Tool
- **Provider**: OpenRouter implementation (access Claude, GPT-4,
  Llama, Gemini via single API)
- **Channels**: CLI channel with interactive and single-message modes
- **Observability**: NoopObserver (zero overhead), LogObserver
  (tracing), MultiObserver (fan-out)
- **Security**: Workspace sandboxing, command allowlisting, path
  traversal blocking, autonomy levels (ReadOnly/Supervised/Full),
  rate limiting
- **Tools**: Shell (sandboxed), FileRead (path-checked), FileWrite
  (path-checked)
- **Memory (Brain)**: SQLite persistent backend (searchable, survives
  restarts), Markdown backend (plain files, human-readable)
- **Heartbeat Engine**: Periodic task execution from HEARTBEAT.md
- **Runtime**: Native adapter for Mac/Linux/Raspberry Pi
- **Config**: TOML-based configuration with sensible defaults
- **Onboarding**: Interactive CLI wizard with workspace scaffolding
- **CLI Commands**: agent, gateway, status, cron, channel, tools,
  onboard
- **CI/CD**: GitHub Actions with cross-platform builds (Linux, macOS
  Intel/ARM, Windows)
- **Tests**: 159 inline tests covering all modules and edge cases
- **Binary**: 3.1MB optimized release build (includes bundled SQLite)

### Security

- Path traversal attack prevention
- Command injection blocking
- Workspace escape prevention
- Forbidden system path protection (`/etc`, `/root`, `~/.ssh`)
