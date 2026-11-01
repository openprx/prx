# Web Console

See [provider management](provider-management.md) for inspection, configuration
transactions, credential references and recovery.

Browser-based management interface for OpenPRX. Located at `console/` in the repository.

## Features

- Configuration editor
- Overview health from `GET /api/status` (generation, revisions, reload, restart, provider degradation)
- Process log stream (`/api/logs/stream`), labeled separately from the job state machine
- Schedules for the authenticated API owner (`/api/cron/*`)
- Workspace usage and cost (`GET /api/cost`)
- Skill lifecycle and the controlled experiment catalogue, inside Skills
- Decision plane, hooks, providers, channels, and configuration changes
- Session list

Memory search, recall, store, and forget are not offered. Evolution, fitness, doctor, and the other non-browser rows stay on the CLI. Engineering green on the lifecycle and experiment screens is not live or benefit acceptance.

## Where conversations come from

Create an empty persisted conversation with authenticated `POST /api/sessions`
and JSON `{}`. The server creates its UUID, binds the authenticated token's
owner, and returns HTTP 201 with `session_id`, `status: "empty"` and
`message_count: 0`. No model call or synthetic message is created. Owner, channel
and ID overrides are rejected. Read-only autonomy refuses creation. SQLite and
PostgreSQL support the operation; other backends must implement the explicit
creation contract or return an error.

Use the returned ID with `POST /api/sessions/{id}/message`. Accepted messaging
channel turns also create records in this list. Webhook requests, `prx agent`
and terminal `prx chat` have separate histories and do not implicitly create
Console sessions. The existing Console list continues to show these persisted
records; the API is available to clients without manually inserting database rows.

## Tech Stack

- Svelte SPA (Vite)
- Served by the OpenPRX gateway (embedded static files)

## Build

```bash
cd console
bun install
bun run build
```

## Long-running requests

### Decision configuration

Open **Decision plane** to read the current enabled state, backend health,
circuit state, exclusions, backend timeout, current numeric policy thresholds
and store counts. Configuration forms propose
changes; they do not prefill the current configuration. Enable/disable,
remote access, backend (including timeout), credential reference and policy changes first open
the shared plan review. Apply sends that saved mutation set with the plan's
revision; a stale revision requires a new preview. Remote access changes
retain the server's high-risk confirmation and authorization checks.
Blank policy inputs leave those settings unchanged. Blank backend endpoint,
model, snapshot and timeout inputs are omitted; the server retains existing
values, except that switching to a non-hosted kind clears the hosted snapshot.
The credential reference field accepts a reference, never a secret value;
`none` or an empty reference explicitly proposes clearing it. These changes
still require plan review and apply.

Leaving a decision, provider, model or routing page disposes its pending change
flow. A late response cannot trigger another confirmation send after teardown.
An already-sent transaction may still finish; leaving the page is not rollback.
Provider selection and model/routing refreshes discard superseded responses;
old provider details and route rows are cleared when their selection changes.

The credential replacement panel first reads the configuration revision,
then accepts a new secret in a password field. Submission sends it only in
the write request body with `If-Match`; it never loads the existing secret.
The input is cleared on submission, cancellation and page teardown. Failures
use a generic message and are not retried automatically. A changed revision
requires preparing the change again. A successful write with `previous_left`
warns that the previous credential remains stored, so rotation is not fully
complete. A status-refresh failure after a confirmed write is reported separately
and does not instruct the operator to repeat the secret write.

The backend test panel separates builtin, local configured and live tests.
Live tests first obtain the server confirmation response, then require a
separate confirmation before sending the neutral fixture. The confirmation
carries the server's revision in `If-Match`, binding the active configuration
generation and requested backend. A changed generation returns a conflict
without sending; review the target again. They may be billed.
The panel preserves the returned outcome, sent flag and backend attempts.
It also shows the serving backend's answered primitives, restricted to `choice`,
`score` and `noul`; unknown labels are not presented as coverage. A fallback's
covered primitives do not prove conformance of the requested backend;
fallback and not-tested results are not presented as a successful test of the
requested backend. Failed or uncertain requests are never retried automatically.

The evaluation panel reads owner-scoped reports over a UTC date window, with
both entered dates included. A displayed report retains its successful request's
window even when the form is edited afterward; failed requests do not relabel
old results as a new report.
It displays coverage, agreement, calibration, outcome/backend rates, latency,
byte totals and counterfactual estimates with their confidence intervals.
Unavailable metrics remain unavailable, and agreement is not causal proof.
This read does not save a report. To preview calibration, enter the identifier
of a report already saved by `prx causal evaluate`; the shared plan review
then shows the server-validated changes before a revision-bound apply.
New reports persisted with `prx causal evaluate --owner …` also receive a
separate ownership receipt under the durable decision key. It binds the
workspace, owner and exact report bytes without storing the owner text in the
receipt. Historical reports are not rewritten or assigned an inferred owner;
reports without a verifiable receipt are not eligible for owner-scoped listing.
A missing durable key refuses an owner-scoped report write.

### Decision inspection

Open **Decision inspection** to list up to 100 decisions visible to the current
authenticated account. Time filters use UTC and the result list displays its
successful request's half-open interval (start included, end excluded). Editing
the form does not change the displayed list's recorded query window. Details
also show the record timestamp and decision-cache flag. Narrow the window to examine
older records, or enter a decision identifier to load it directly. Details
show the chosen directive, backend, policy, candidates, recorded action/target
probability distributions, per-pass gate counts and recorded outcome
classifications. An outcome join means records have been linked; it does not
by itself establish successful execution. Missing outcomes remain unknown.
Router history/readmission evidence is historical: it cannot authorize a new
provider attempt, and a recorded refusal does not prove a premium or vision call
was sent. Provider/model/routing configuration success likewise does not establish
that KNN ran or that a billed auxiliary request was admitted. The
[routing and cost lifecycle](provider-routing-cost-lifecycle.md#embedded-local-routing)
describes the local candidate's supported path and pending acceptance.
A `delegated` parent outcome records completed dispatch with partial completeness;
child completion remains separate. Details show up to 64 owner-scoped child
outcome links, with a truncation notice when more exist. Each link opens the child
decision and shows its recorded classification and completeness. Missing child
links leave completion unknown; child costs are not added to the parent record.
The page uses an explicit field projection rather than a raw decision record
dump. Its optional projection section reads only the historical redacted state
actually saved by the recording policy. It never reconstructs missing state
from current input. Reads share the decision owner scope, reject oversized
payloads, and reapply display redaction. Additional display masking is labeled;
projection contents are excluded from debug output. Distribution labels are restricted to canonical
actions and targets; absent, legacy or invalid maps remain unavailable without
fabricated probabilities or renormalization. Gate details are read under the
same owner scope and reduced to pass/approval counts and blocked gate numbers.
Operation digests and arbitrary diagnostic strings are not exposed. Missing,
legacy or malformed gate summaries remain unavailable.

### Decision approval and resume

In a conversation, expand **Review a pending operation**, enter the decision
identifier from the waiting reply, and load its details. Inspect the tool,
effect and redacted arguments before approving. Approval, rejection,
revocation and execution are separate buttons. The panel sends no display
arguments back to the server; execution uses the saved original call. Review
and revocation acknowledgements are checked against the inspected decision;
the panel shows single-use status and the returned approval expiration. A
conversation-refresh failure after an execution response is distinguished from
an unknown execution result. A failed execution request is shown as an unknown result and is never retried
automatically. Check conversation history before another attempt.

When the decision plane is enabled, an authenticated owner can list pending
calls with `GET /api/sessions/approvals?limit=&cursor=`. The list contains only
rows whose owner is the authenticated token. A disabled decision plane returns
409. An owner with no pending calls receives an empty `approvals` array. Rows
owned by anyone else are omitted. Each item has `session_id` plus the same
redacted projection as the per-session inspect route: `decision_id`, `tool`,
`arguments`, `arguments_redacted`, `original_call_available`, `effect`,
`call_digest`, `pending_expires_at` and `single_use`. `limit` defaults to 50
and is capped at 100. `cursor` is opaque; pass the previous response's
`next_cursor` to continue. A denied review (`approved` false on the
`decision-approval:{id}:review` row) is omitted. A row whose operations are
all resolved is omitted. An approved call that is still waiting to resume
stays listed. A storage or service failure returns 503, not 409. Review,
revocation and resume stay on the per-session routes.

An authenticated owner can inspect one pending call with
`GET /api/sessions/{id}/approvals/{decision_id}`. The response
contains the tool, redacted arguments, call digest and pending expiration;
`arguments_redacted` identifies a changed display and `original_call_available`
indicates whether an authenticated encrypted snapshot exists. This projection
contains no reusable grant and does not assert that execution is currently
approved. Missing, expired or inaccessible records return a conflict response.

An authenticated operator can review a
pending call with `POST /api/sessions/{id}/approvals/{decision_id}` and JSON
`{"approved":true}` (or `false` to reject). Review records a decision-scoped
approval; it does not execute the operation. An initial `RequestApproval`
directive closes with the decision identifier and effect category, without
entering the ordinary model/tool loop. Its outcome is `awaiting_approval`,
not operation success. The pending record must be durable before this close
is issued.

Console review, rejection, revocation and resume are authenticated owner control
operations. Read-only policy still refuses them with HTTP 403 and the existing API rate limits
still apply. They do not require a prior autonomous runtime grant merely to
review an operation: the signed owner review is the grant issuance step.
Each request verifies the durable pending owner, workspace, session, source and
current publication. Resume independently verifies and consumes the original
single-use exact-call grant, including the current schema, path and arguments;
control authorization never grants general filesystem permission. Control
requests are audited separately from signed review and execution evidence.

To revoke an issued, unexpired approval, use
`DELETE /api/sessions/{id}/approvals/{decision_id}`. This records revocation
without deleting the audit history. Repeating a matching revocation is safe
while the approval remains current. Revocation and execution consumption
compete for the same durable resolution; an already consumed approval cannot
be reported as revoked. Missing, expired, conflicting or consumed approvals
return a conflict response, and revocation never stops an operation that has
already claimed its grant.

Resume through `POST /api/sessions/{id}/approvals/{decision_id}/resume` with
`{}` to recover the original call from its authenticated encrypted snapshot.
Older records without a snapshot require
`{"tool":"tool_name","arguments":{}}`, supplying the original approved tool
and arguments; this explicit form also remains supported for newer records.
Do not copy redacted display arguments into an explicit resume request.
The route requires the same authenticated owner, session and
actor, a current unexpired approval, and current policy permission. Unknown
request fields are rejected. Ordinary session messages never trigger resume
by resembling this JSON.

Resume creates a new recorded input and reruns the decision gates and selection.
Approval permits the exact call; it does not force an execution decision. A
clarification or refusal closes the turn without dispatch. An unconsumed,
unexpired approval can still be revoked through the owner control above.

When the decision selects execution, the route dispatches the approved call
directly. A changed call, schema, catalog, resolved operation or authority is
refused. Approval is consumed once after execution preparation and reservation;
a failed or uncertain attempt does not restore it. No main-model response or
usage is invented for direct execution. The execution result includes
`execution_status` and a reply; a finished job alone does not establish operation
success. A closed clarification or refusal returns its reply without an
execution result.
The route supports the same synchronous and asynchronous job modes below.

### Private files

In the candidate runtime, Console `file_read`, `file_write` and `file_edit`
address the authenticated owner's private file space. For example,
`notes/result.txt` belongs to that owner within the current workspace identity.
Another owner using the same path addresses a different file. Console does not
import or write files from the shared workspace. Ordinary Chat file paths keep
their existing workspace meaning.

Use relative paths. Absolute paths, parent traversal, symbolic links, hard links
and special files are rejected. Reads and edits are limited to 10 MiB files.
Writes still require the current execution policy and, when requested, exact
owner approval. Approval review and resume resolve the same private target;
changed owner, root identity or call data requires a new approval. Private
write/edit results report the affected logical path and byte or replacement
count, without a shared-workspace diff preview.

Storage uses `owner-files/v1` beneath the existing configuration directory,
outside the shared workspace, with private directories and files. Configuration
layouts that overlap these spaces or lack safe ownership/permissions leave
private file capabilities unavailable; there is no fallback to shared files.
Directories are created only during an authorized write. Unsupported platforms
also leave these capabilities unavailable. This is isolation between Console
owners, not protection against an administrator controlling the host account.
No new configuration option or MCP file permission is introduced.

Private file namespaces also depend on the configuration directory's filesystem
identity. Moving, recreating or restoring that directory onto another filesystem
can select a new namespace. Existing private files are not automatically imported
or remapped. Preserve the old data and arrange an explicit migration that verifies
its owner; this candidate provides no automatic migration command. Previously
issued approvals cannot be reused across a root identity change. Obtain fresh
approvals when required by the current execution policy.

Signed child-worker source snapshots now retain the isolation flag explicitly.
Older snapshots without that provenance are refused and need a newly issued
child task; they are not restored with weaker permissions.

### Job lifecycle

The gateway has no request timeout. Routes that can start an agent turn —
`POST /webhook`, `POST /api/sessions/{id}/message`, `POST /mcp/v1/tools/call` —
run the turn as a detached job rather than inside the HTTP request future, so a
closed tab, a proxy hang-up, or a client abort no longer destroys work that has
already started committing side effects.

Two ways to collect the result:

- **Wait** (default): the request answers with the job's result, exactly as
  before. The only change is that the job outlives the connection.
- **Async**: send `?mode=async` or the header `Prefer: respond-async`. The
  request returns `202 Accepted` immediately with `job_id`, `work_id`, and poll
  and cancel URLs.

| Endpoint | Purpose |
|----------|---------|
| `GET /api/jobs` | Every retained job, newest first |
| `GET /api/jobs/{job_id}` | One job: status, elapsed, HTTP status, result or error |
| `POST /api/jobs/{job_id}/cancel` | End the job and its descendants |
| `GET /api/runtime/tasks` | Every registered work item, jobs included |
| `POST /api/runtime/tasks/{id}/kill` | End that item and, by default, its lineage |
| `POST /api/runtime/tasks/{id}/message` | Hand a running sub-agent an operator instruction |
| `POST /api/channels/{name}/send` | Send one message on a configured channel, on behalf of an entry point that owns none |

`POST /api/channels/{name}/send` accepts `{recipient, message, as_voice?}`. The
response is `{channel, delivered, detail}`. The Console shows those fields and
does not treat a missing field as delivered. A client timeout or a dropped
connection is uncertain delivery: the Console does not send the message again.
`403` means `send_allow` refused the operator plane. Channel `status` on
`GET /api/channels/status` is the literal `configured`; it is not a health check.

`POST /api/config/reload` returns `{success, message}`. `message` is the reload
tool's output string. HTTP 200 does not mean every participant reloaded. The
Console then reads `GET /api/status` and shows `config_reload_status`,
`config_reload_in_progress`, `restart_required`, the active and desired source
revisions, the active generation, participants, and `last_config_reload_failure`
when those fields are present.

Job status is one of `running`, `succeeded`, `failed`, `cancelled`. Finished
jobs are retained for one hour; running jobs are never evicted. The `work_id` in
a job's payload is its runtime-registry id, so `prx tasks list` and
`prx tasks kill <work_id>` operate on the same item from the CLI.

`{id}` on both mutating endpoints accepts **either** address space: the run id
(`prx tasks list` prints it next to the name) or the registry `w42`. Only the
run id means anything outside this process — `w42` is a process-local counter —
so a caller that is not an operator sitting at this machine should always use
the run id.

`/message` is the control-plane half of the `sessions_send` tool: the message
travels the sub-agent's own bounded steering queue, so the run cancels what it
is doing, takes the instruction on board as an operator turn, and carries on.
The CLI form is `prx tasks send <id> "<message>"`. Items that expose no
steering channel — an agent turn, a tool call, a job — answer `409` rather than
accepting a message nobody will read, and a busy target parks the caller
instead of timing out.

A `200` reports `outcome: "queued"`, and the word is exact: **queued is not
read**. The endpoint can see that the message was accepted onto the target's
steering queue; it cannot see whether the run ever takes it off again, and
nothing in this runtime expires to make that visible later. The `409` above
only rules out targets with no channel at all — a run that registers a
steering channel and then never drains it (a task-mode sub-agent running
without tools does exactly this) accepts the message and consumes it never.
Treat a `200` as "handed over", not as "acted on", and confirm the effect by
watching the target rather than by reading the response.

## Batches in the task listing

A `spawn_batch` fan-out shows up in `prx tasks list` as one unit. Its members
carry a `batch_id` (also present in `--json` and in `GET /api/runtime/tasks`),
and the table gathers them under a header at the position of the first member:

```
Running work items (4):
ID       KIND          ELAPSED  STATE          NAME
w1       sub_agent         41s  running        gateway:webhook:anonymous  (run 8c28…)
  batch batch-0fab239c-65b3-4314-a1e7-f46ca22de68e — 3 member(s):
  w3     sub_agent         38s  running        sub-agent  (parent w2)  (run 5332…)
  w4     sub_agent         38s  running        sub-agent  (parent w2)  (run 9e38…)
  w5     sub_agent         37s  running        sub-agent  (parent w2)  (run 1887…)
```

The `(parent w2)` on every member points at a row that is no longer there — `w2`
was the `spawn_batch` tool call, and it was deregistered as soon as it returned.

A listing with no batches renders exactly as it always did.

The grouping is not cosmetic. Batch members are **siblings**, not a lineage:
their common parent is the tool call that launched them, and that row is
deregistered the moment the launching call returns — long before the members
finish. Without the recorded batch id, a fan-out is unrecoverable from the
registry, and its members are indistinguishable from unrelated concurrent runs.

For the same reason, the batch id is a **third kill address**, alongside a run
id and a `w42`:

```bash
prx tasks kill <batch-id>              # end every member, and what each started
prx tasks kill <batch-id> --no-cascade # end the members only
```

This is not a second termination mechanism: the batch resolves to its member
rows and each one gets the ordinary lineage cascade. Every target is signalled
before any is verified, so ending a fan-out of any width costs one verification
window rather than one per member.

`--no-cascade` stops at the member rows themselves, and both execution modes
answer it. A task-mode member is aborted through the handle on its row. A
process-mode member is not — aborting its monitor would strand the OS worker
that monitor alone may signal and reap — so its row carries a cancellation
token instead, and cancelling it asks that same owner to terminate the worker
and reap it, exactly as the `kill` action of `sessions_spawn` does. What
`--no-cascade` leaves running is whatever a member started *below* itself: a
shell, an `ffmpeg`, a sub-agent of its own. Use it only when you mean that;
otherwise kill without it.

## A delivery in flight is itself a task

`POST /api/runtime/tasks/{id}/message` registers the delivery in the work
registry for exactly as long as it takes, so a parked send appears in
`prx tasks list` as `steer → w42 <target name>` and disappears the moment it
lands:

```
w3       sub_agent       1m00s  running        sub-agent (process)  (run ce2d…)
w1037    tool_call           9s  running        steer → w3 sub-agent (process)  (run ce2d…)
```

This matters because the steering queue is bounded and nothing here expires on a
clock. A send to a run that has stopped draining its queue parks indefinitely,
and until it had a row of its own that parked delivery was invisible everywhere
at once: the target looks idle, the caller is a different process, and no
timeout will ever end it. In a runtime with no wall clock, the only backstop is
*being seen and being killable by hand* — so the row is registered with its own
cancellation token, and killing it releases the caller with a `409` naming the
delivery as ended, without the message ever reaching the target.

## Cross-entry visibility is operator-level, and it is one-way

`prx tasks list`, `prx tasks kill`, `prx tasks send`, and their chat equivalents
(`/sessions --daemon`, `/kill --daemon`, `/steer --daemon`) all read the work
registry of the **daemon** process through this control API. That gives an
operator one view across entry points: a run started by a Signal message, by a
webhook or by MCP all appear in the same listing, and so does work another
`prx` process creates *inside* the daemon through this API — the steer delivery
row above is exactly that. Work that another `prx` process runs itself does not
appear: that process has its own registry, which nothing here reads.

Two limits are deliberate, and neither is fixed by this API.

**It is an operator plane, not a session plane.** The gateway authenticates a
local operator by bearer token; it does not scope work to the calling user's
memory principal. A chat user running `/sessions --daemon` therefore sees *every*
run the daemon holds, including runs other people's messages started. That is a
property of the token, not of an identity model — the two identity spaces have
not been unified.

**Chat's own work is not in it.** `prx chat` runs no gateway, so it has no
control API of its own and no registry anyone else can read. Concretely:

| Work | In `prx tasks list`? |
|---|---|
| Daemon turns, sub-agents, tool calls, child processes | yes |
| Sub-agents a daemon-side `sessions_spawn` started | yes |
| Cron jobs, detached HTTP jobs | yes |
| A turn running inside `prx chat` | **no** |
| A `/bg` sub-agent started inside `prx chat` | **no** |
| A shell child process started inside `prx chat` | **no** |
| A run in another `prx` process that is not this daemon | **no** |

The column answers only the daemon's listing; the paragraph below says where
chat's own work *is* visible.

So the arrow points one way: chat can see and steer the daemon, the daemon
cannot see chat. Chat sessions are still visible and killable from **inside that
chat** (`/sessions`, `/kill`, `/steer` without `--daemon`), which is where the
process that owns them is.

Closing the reverse direction is not a matter of adding an endpoint. It would
mean either running a gateway inside every chat process — a second listener, a
second auth surface, a port per terminal — or routing visibility through shared
memory, which first requires unifying the operator identity and the chat user's
memory principal that the section above says are separate today. Both are
independent pieces of engineering, and neither is pretended to be done here.

The read-only `GET /api/causal/reports/saved` catalog returns only authenticated owner report IDs and timestamps. It scans at most 4096 directory entries and verifies at most 128 canonical report files; `complete: false` denotes a bounded or unavailable catalog. Unbound historical and operator reports remain invisible. Results are sorted by creation time within the inspected subset; an incomplete result is not a newest-report guarantee. Owner report writes and reads share a 2 MiB limit. The Console can load this catalog and select a report ID for the existing explicit calibration preview; selecting alone applies no change.

Decision inspection also projects recorded Noul probabilities for canonical global and candidate questions. Missing, legacy or malformed feature maps remain unavailable; the view neither fills missing probabilities nor renormalizes them. Candidate feature names are restricted to canonical action/target keys and optional eight-digit hexadecimal namespace discriminators.

Ordinary Console session messages use the pinned source-bound built-in decision service even when the causal plane is disabled. Disabled mode may use authenticated in-memory input when no durable event exists, omits causal settlement/rejoin, and keeps normal transcript persistence and cancellation cleanup. Approval/revoke/resume endpoints remain unavailable while disabled. Gateway default-provider construction binds the exact PMM-resolved model and credentials in both modes.

## Model levels and parameter settings

Open **Model settings** on the Models page to load that model's release-catalog
levels and parameter declarations. Numeric controls use the declared bounds
and steps; enumerated controls list only declared values. Empty controls keep
the level's value. Review the settings change before applying it through the
existing transaction flow; revision checks and history rollback still apply.
The backend refuses invalid durable values before any write. See
[model settings and API examples](model-catalog.md#saving-a-model-level).

### Exact-call review coverage

Supervised exact-call collection includes named Shell and HTTP operations in
addition to file, fixed-operation MCP and lifecycle mutations. Approval binds
all normalized arguments; explicit resume executes the stored call once and
retains tool-specific policy. A Console owner's existing isolation restrictions
still apply, so this does not grant Console access to host Shell. The plain
agent CLI has no review adapter; use Chat or Console for this workflow.

An operator can call the authenticated plugin reload route through
`prx plugin reload NAME --gateway-url URL --token-from-stdin --yes`. The command
uses an existing paired token supplied on stdin and reports the returned plugin
generation. It does not start or restart the gateway.

`prx plugin enable NAME --gateway-url URL` previews a disabled-enable and does
not contact the gateway. `--token-from-stdin --yes` posts
`POST /api/plugins/NAME/enable` with body `{}` and reports the gateway's plugin
generation. It does not build a local plugin runtime, does not write trust, and
does not change `[plugins] enabled`. Read-only autonomy refuses before the
request. A name absent from the loaded generation is not first-discoverable by
enable or reload.

The Console plugin routes use that same owner:

| Route | Body | Risk |
| --- | --- | --- |
| `POST /api/plugins/{name}/enable` | `{}` | Low |
| `POST /api/plugins/{name}/disable` | `{}` | Low |
| `POST /api/plugins/refresh` | `{}` | Low. Rebuilds every eligible plugin. Not first discovery of one name. |
| `POST /api/plugins/install` | `{source, sha256, manifest_sha256, permissions}` | High. Requires `?confirm_high_risk=true`. |
| `POST /api/plugins/update` | same as install | High. Requires `?confirm_high_risk=true`. |
| `DELETE /api/plugins/{name}` and `POST /api/plugins/{name}/remove` | POST body `{}` | High. Requires `?confirm_high_risk=true`. Revokes trust and returns `recoverable_backup`. |

Install and update use one operator action (TRANSPORT-GAPS option A). The
handler checks `sha256` and `manifest_sha256` against the workspace source
directory, then calls `PluginManageTool` install or update with those hashes
as `expected_sha256` and `expected_manifest_sha256`. The manage tool checks
them again against the bytes it reads in the call that records trust. A
mismatch is HTTP 400, with no trust record and no `plugins/<name>`. That tool
records trust after a successful compile. A prior `prx plugin trust` record is
not required, and the handler does not record trust itself. Unknown JSON fields
are rejected. A missing runtime answers `503`. Read-only autonomy answers `403`
before that check. A partial enable that rolls back is an error, not success.
A directory that appears under `plugins/` after the runtime started, and is not
in the loaded generation, answers 404 from enable: it is not first-discoverable
by enable or reload. HTTP 409 is reserved for a name the loaded generation
already contains.

`GET /api/plugins` lists the loaded generation only. It does not report separate
trusted, enabled, healthy, or listening facts, and it does not include artifact
hashes.

## Operations surfaces

These routes sit behind the same paired-token middleware as the rest of `/api`. The browser does not send `owner_id`, a trusted scope, or an approval grant. A missing or foreign cron id both return `404` with `Job '{id}' not found`. Event rows omit `payload_json`.

### Overview

Overview reads the existing status payload. Generation, the active and desired config revisions, reload status, restart-required participants, and provider degradation render from that payload. A missing value renders as unknown. An empty restart list is "not required". This screen does not add a health route.

### Process logs

`/logs` is the process log socket, not the job state machine. The page keeps a connected marker and a disconnected-stale marker, bounds the on-screen buffer, and exports only the redacted lines currently on screen. The socket stays outside the bearer middleware and uses the paired cookie, as before.

### Schedules

`GET /api/cron/jobs`, `GET /api/cron/jobs/{id}` (with events), `GET /api/cron/jobs/{id}/runs`, and `GET /api/cron/status` list the jobs whose stored owner is the API token owner. The server scope is `_zc_scope { owner_id, owner_isolated: true }` with `_zc_scope_trusted`. It does not send a channel or a sender. `cron` is on `OWNER_ISOLATION_AWARE_TOOLS` because every tool action either records that owner at creation or admits only that owner's jobs through `CallerScope`. `POST /api/cron/jobs` accepts exactly one of `expression`, `at`, `every_ms`, or `delay`, plus `command`. `tz` is only valid with `expression`. `PATCH`, pause, resume, and `DELETE` use the same owner filter. A foreign id on those mutations is the same `404` as a missing id. `POST /api/cron/jobs/{id}/run` claims the existing job and runs it through the scheduler's preauthorized manual path. The response is `{job_id, status, duration_ms, output}` and does not include a run id. The page then reloads the run list once. There is no cron-disabled flag: status reports `enabled: true` and `disabled_state: "not_configured"`. A store that cannot be opened is `503`, not an empty list. Every mutation calls `SideEffectGate::authorize_resource_operation` through `authorize_resource_mutation`, with no browser grant. Create, an update that stores a `command`, and run-now are `ResourceRiskLevel::High` (`gateway_api:cron:create`, `gateway_api:cron:update`, `gateway_api:cron:run`). Pause, resume, remove, and an update that does not change the command are `ResourceRiskLevel::Medium`. Reads are not gated. Under `autonomy = supervised` those Medium and High operations return `403` until a runtime grant exists, the same rule as `POST /api/hooks`. A command that is stored or run is also passed to `SideEffectGate::authorize_command_execution("cron", command, None)`, which is `SecurityPolicy::validate_command_execution`. A forbidden path is `403`. The scheduler's `run_job_command` does not apply that command policy later. `prx cron mark-operator` stays on the CLI.

### Memory

`POST /api/memory/search`, `/recall`, `/store`, and `/forget` call `memory_search`, `memory_recall`, `memory_store`, and `memory_forget`. The server writes `_zc_scope { owner_id: <ApiPrincipal.owner_id>, owner_isolated: true }` and `_zc_scope_trusted`. It does not guess a channel or a sender, and a body `owner_id` is `400`. Those four tools are on `OWNER_ISOLATION_AWARE_TOOLS`. A read for that scope has no channel triple, so the non-public arm matches `owner_id` and the caller's own rows stay visible. Public rows stay shared. Forget uses the tool's existing approval path and does not add a browser grant. A successful body includes `scope: "owner"` and `source` (the backend name). An empty search or recall is `200` with `empty: true`, not an unavailable state. A missing key and another owner's key both return `404` `{"error":"memory not found"}`. A backend whose name is `none` is `503`. `prx memory reindex` and `prx migrate` stay on the CLI.

### Cost

`GET /api/cost?window=today|month|all` reads `workspace_cost_report` for the gateway workspace. The scope is `workspace`; the report is not isolated per token. `reservation_estimates: true` applies only to reservation amounts, which are not settled charges. The field is not a top-level `estimates` flag beside `cost_usd`. `unpriced_usage` is the text `omitted`. A settled `cost_usd` of 0 is a real zero. A missing price is not filled in as zero. `as_of_utc` is the ledger snapshot time, not a config revision. A bad window is `400`. A report error is `503`, not an empty zero report. The page chooses the scope sentence from `report.scope`.

### Skill lifecycle

`GET /api/skills/lifecycle/status`, `GET /api/skills/lifecycle/proposals`, and `GET /api/skills/lifecycle/proposals/{id}` call `skills_manage` after the server installs a trusted scope whose `owner_id` is only the API principal and whose `owner_isolated` flag is set. `skills_manage` limits an isolated caller to status, list, inspect, and cancel. Publish and rollback therefore return `403` `publish and rollback need the operator's full session`. They need the operator CLI session. The page says that and does not send a grant. Cancel can reach the service. An owner mismatch is `403` and the body does not echo the configured owner scope. Unknown proposals and revisions are `404` `{"error":"not found"}`. A stale generation is `409`. Tool failures carry a `lifecycle_code`. `CommittedRepairRequired` is `409` `{"error":"committed with repair required","committed_repair_required":true,"transaction_id":...}` and is not described as rolled back. `replayed: true` is a replay, not a new generation. A disabled lifecycle is `503`. Import and the operator lifecycle commands stay on the CLI. The screen states that engineering green is not live or benefit acceptance.

### Lifecycle experiments

`POST /api/skills/lifecycle/h2` accepts `{action, request}`. `action` must be one of the thirteen `lifecycle_experiment_*` catalogue actions. Study actions and any other action are `409`. Request keys that would set an owner, workspace, MAC, pins, grant, or proof are `400`. The handler does not mint an H2 invocation and does not submit `lifecycle_experiment_run` as a console job: the job cleanup owner does not fit an existing job kind. With no controlled profile the route is `403`. With a profile attached it is `503` describing that mint gap. `ControlledH2Profile` route allowlist is unchanged, so a controlled gateway still refuses this path in middleware before the handler. The page says that. It does not offer study actions and does not poll a job. Engineering green on this screen is not live or benefit acceptance.

### Still on the CLI

Identity, health, metrics, pairing, CLI evaluate, skill import, operator lifecycle, the skill-file tool, H2 study, xin, evolution, fitness, approval grants, EU audit, doctor, and OS service install are not console CRUD screens. A deployment that lacks a route shows an explicit unavailable state rather than a control that pretends to succeed.
