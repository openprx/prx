# Ramflux channel

The `ramflux` channel connects prx to a local `ramflux-agent-host` over one
Unix socket. ramflux is the transport and trust layer. prx is the execution
layer: model, tool loop, and memory. The socket is the only shared surface.

## Trust boundary

prx does not hold ramflux account keys, does not sign, and does not know
nodes, gateways, or custody. The agent-host owns those. The socket file is
mode `0600`, owned by the same Unix user as prx. prx refuses to listen when
the path is a symlink, the owner uid differs, or the mode is not `0600`.

Default path: `<workspace>/ramflux/run/ramflux.sock`. The channel creates
`ramflux` and `run` at mode `0700` and leaves the workspace mode alone.
Override with `socket_path`. The socket's parent must be mode `0700` and owned
by this user. prx refuses an existing parent that is looser. Accept uses
`UnixStream::peer_cred` and refuses a different user.

The account data key is a 32-byte DEK wrapped by the prx secret store's file
key (`.secret_key`). It is not stored in the OS keychain or Secret Service.
That file key is the current custody boundary. Overwriting a file with zeros
cannot promise erasure on a copy-on-write filesystem such as APFS or btrfs;
destroying the DEK is what makes a leftover ciphertext unreadable.

A channel instance remains bound to its first account; changing accounts
requires a new instance. This also keeps queued work and replay in that account.
One connection is active. A second connection is rejected. After a disconnect
the host handshakes again. prx replays unacknowledged `reply.*` frames and
marks turns that were still running as `failed` with code `bridge_lost`.

## Handshake and authorization

The host sends `bridge.hello`:

| Field | Meaning |
| --- | --- |
| `protocol_version` | Must be `"1"`. |
| `agent_account_id` | AI account this connection serves. |
| `capabilities` | Host capability names. |
| `authorization` | `full` or `supervised`, from the host's full-authorization switch. |

prx replies `{ok, prx_version, effective_autonomy}`.

| Config `authorization_mode` | Host says | Effective |
| --- | --- | --- |
| `follow_rf` (default) | `full` | `full` |
| `follow_rf` | `supervised` or not yet hello | `supervised` |
| `supervised` | either | `supervised` |
| `full` | `full` | `full` |
| `full` | `supervised` or not yet hello | `supervised` |

Config alone never selects `full`. Chat text is never an approval. A decision
that does not arrive before `expires_at_ms` is cancelled.

Approval matrix:

| prx-local decision | ramflux effective | What happens |
| --- | --- | --- |
| Allow | full | The tool runs. Nothing is put on the wire. |
| Allow | supervised | The channel asks. prx sends `approval.request` and waits. |
| Ask from prx autonomy, `requires_explicit_grant`, or a decision binding | full or supervised | The durable prx approval path records the ask. prx sends `turn.status` `awaiting_approval` with the prx approval id. The bridge does not grant it. |
| Deny | either | The tool does not run. |

A delegated tool uses the same gate as its parent run. In supervised mode it
sends `approval.request` and blocks until the host answers.

`idempotency_key` on `reply.*` is `{internal_run_id}:{seq}`. Replay sends the same
body, so the key survives a reconnect. A new turn after restart uses a fresh
internal run UUID, even if the host reuses its correlation ID.

Bridge errors:

| Code | Meaning |
| --- | --- |
| -32001 | The account is paused. |
| -32002 | `bridge.hello` is required. |
| -32003 | Protocol version mismatch, or the account is not authorized after removal. |
| -32004 | A second bridge connection was rejected. |
| -32005 | `queue_full`. The message says `queue_full depth=N`. |
| -32006 | The peer uid was refused. |
| -32601 | Unknown method or a backend capability that cannot enforce transactional erasure. |
| -32602 | Missing or invalid method parameters. |
| -32603 | Internal storage, eraser, or transport failure; no successful erase is acknowledged. |

`prx channel doctor` prints the socket path, the permission check, and the
effective mode, for example `follow_rf→supervised`.

Liveness: `health_check` is true when the socket writer is up and `bridge.ping`
was seen in the last 30 seconds.

## Bridge protocol v1

Newline-delimited JSON-RPC 2.0. Frames larger than 1 MiB are rejected.
Requests should carry `idempotency_key` inside `params`. prx deduplicates
inbound request keys for ten minutes. Durable input receipts use
`channel:ramflux:{hash(account)}:{hash(conversation)}:{message_id}`, so a
message id reused by another account or conversation cannot collide. Every outbound frame carries `turn_id`.

An optional host `turn_id` on `inbound.message` or `inbound.surface_action`
is a correlation ID: 1–128 ASCII bytes from `[A-Za-z0-9._:-]`. Invalid IDs
return JSON-RPC error `-32602` before admission or acknowledgment. PRX still
mints a fresh internal UUID for each run, used by approval, registry, terminal,
history, usage, and deletion. Without a host ID, the wire carries that UUID.
Every outbound turn frame carries the host ID when supplied, including
approval, attachment fetch, streaming text/surfaces, tool/task frames and the
final surface's `final-<turn_id>` ID.

Host turn IDs should be unique. A duplicate in the same conversation is
rejected while reserved, queued, running, or retained by unacknowledged
outbound frames. Reuse is accepted only after the previous turn and its replay
are retired; it creates a new internal run. Another conversation can use the
same host ID independently. Replaying a message with the same message ID and
turn ID remains a no-op; changing its turn ID is rejected within the in-memory
message replay window (ten minutes, at most 256 messages). Revocation clears
that conversation's replay checks. Reconnect replay
keeps the original host ID. Replay and reservations are volatile and empty
after a process restart; durable UUID records remain readable.

`turn.cancel` always targets the current live, bound holder of the host ID in
that conversation. A delayed cancel after accepted reuse cancels the new
holder. Unknown, retired, and queued but unbound IDs return `ok:false`.
Surface actions reserve their host ID before `ok:true`, then pass it to the
derived turn; asynchronous admission failure releases it. Task events do not
start a turn and ignore their optional `turn_id`: they target the latest bound
turn in the conversation, as before.

### Host to prx

| Method | Role |
| --- | --- |
| `bridge.hello` | Handshake. |
| `bridge.ping` | Liveness. |
| `inbound.message` | User message. `sender` is a principal, `chat.conversation_id` is the reply target. |
| `inbound.surface_action` | A person activated a card. Not a tool approval. |
| `inbound.task_event` | Host task intent: `start`, `accept`, `decline`, `cancel`, or `resolve`; see lifecycle mapping below. |
| `inbound.control` | Membership and presence. `agent_revoked` erases that conversation and keeps the account key. `agent_removed` erases the account, destroys the data key, and leaves a removal marker until `agent_added`. `agent_offline` pauses the account. `agent_online` resumes it. |
| `inbound.message_deleted` | Hard-delete that message's stored bytes. Reply `{erased: true}`. |
| `approval.decision` | `approved`, `denied`, or `expired`. |
| `turn.cancel` | Cancel one turn. |

### prx to host

| Method | Role |
| --- | --- |
| `reply.text` | Text. `final` marks the last segment. |
| `reply.surface` | `create`, `patch`, `stream_append`, or `close`. Blocks are declarative. `fallback_text` is required when blocks are present. |
| `reply.task_update` | Runtime task and tool state. |
| `reply.task_propose` | Deferred runtime producer; see FOLLOWUPS. |
| `reply.attachment` | Deferred runtime producer; see FOLLOWUPS. |
| `reply.typing` | Typing indicator. |
| `tool.event` | `started`, `progress`, `finished`, or `failed`. |
| `approval.request` | Redacted arguments plus `args_hash` (SHA-256 of canonical JSON of the full arguments), `risk`, and `expires_at_ms`. |
| `attachment.fetch` | Host returns `{bytes_b64, mime}` for one object referenced by the current turn. |
| `turn.status` | `started`, `thinking`, `tool_running`, `awaiting_approval`, `done`, or `failed`. |

Surface blocks must use an allow-listed kind, must not contain script or HTML,
and may only use `http` and `https` URLs. Over-limit surfaces are not sent;
prx emits `turn.status` `failed` with code `surface_rejected`.

`[IMAGE:path]` markers are not interpreted on this channel.

## Ordering

One turn runs per `conversation_id` (`max_in_flight_per_conversation`, default
1). A new message while a turn is running is queued, or interrupts the running
turn when `on_new_message = "interrupt"`. PRX supplies
`turn_wall_clock_secs` (default 120, minimum 1). When that duration elapses,
PRX marks the turn budget-expired and cancels it. If the turn then returns
without a terminal status, `turn.status` is `failed` with code `turn_timeout`.
Paths that write a terminal status after the model loop, including an
interrupting newer message, report `cancelled`. The host may also send
`turn.cancel`; that explicit cancel reports `cancelled` unless the
budget-expired marker is already set.

## Config

```toml
[channels_config.ramflux]
authorization_mode = "follow_rf"
max_in_flight_per_conversation = 1
on_new_message = "queue"
turn_wall_clock_secs = 120
encrypt_at_rest = true

[channels_config.ramflux.surface_limits]
max_blocks = 32
max_actions = 16
max_bytes = 65536

[channels_config.ramflux.attachment_fetch]
max_bytes = 5242880
max_per_turn = 4

[channels_config.ramflux.accounts.agent_a]
system_prompt_file = "personas/agent_a.md"
model = "example-model"
provider = "example-provider"
scope_profile = "default"
cross_conversation_retrieval = false
```

Unknown keys are rejected. Account ids must not contain a path separator.
Memory for an account is namespaced as `agent_account_id/conversation_id`.
Cross-conversation retrieval is off unless the account sets it.

## At rest

When `encrypt_at_rest` is true (the default), this channel's memory content,
media files, and tool-event records are sealed with ChaCha20-Poly1305. Each
account has a data key wrapped by the prx `SecretStore` master key. The data
key lives only in process memory. `agent_offline` and shutdown zeroize it;
the next `bridge.hello` unwraps it again. The wrapped key stays on disk until account removal. Associated data is the account id
and the conversation id, so ciphertext cannot be moved between conversations.

The vector index is not given plaintext. If a backend cannot store an
encrypted vector, this namespace is not indexed.

The first time the channel opens a directory that still holds pre-existing
plaintext for this channel, that plaintext is deleted and a notice is logged.

`inbound.message_deleted` removes the message and all indexed derived data,
including autosaves, message events, ingress receipts, drafts, and tool/media
objects. The SQLite writer records ownership in the same transaction as each
data row. A content-free, durable tombstone prevents deleted input from being
executed again. Queued input is removed; active work is cancelled and its
writer fence is closed before erasure is acknowledged. The index remains
retryable through checkpoint failure. Erasure also rebuilds the remaining FTS index to discard obsolete segments.
`secure_delete` overwrites freed pages;
a TRUNCATE checkpoint empties the WAL, so VACUUM is unnecessary.

`agent_revoked` removes one conversation's data and keeps the wrapped account
key. Explicit cross-conversation retrieval configuration widens that revocation
to the account's retrieval scope. `agent_removed` erases all account data,
destroys the wrapped key, and records an account removal marker. The same-user
host can deliver `agent_added` before a successful hello to clear that marker.
After a restart, a rejected hello identifies the removed account; with exactly
one configured account, an add can identify it directly. A new hello then
unlocks a fresh key. Account removal markers cannot collide with a conversation
named `account`.

Process workers cannot carry the in-process memory capability; scoped turns
refuse that execution mode before creating worker storage. In-process
delegation retains the approval context and indexed memory capability.

The live channel requires SQLite transactional erasure. Other backends return
an unsupported capability error rather than claiming erasure. Channel content
objects reside in the same database as their ownership index. Native memory, session and document tools select the same per-message
capability. Conversation metadata and notices use this view as well. Queued
causal telemetry retains its run's memory capability and configuration
generation across a shared decision writer. Unsupported scoped operations fail
with a typed capability error.
Memory event payloads and drafts use the same encryption view; `session_id` is keyed, and
new version-2 ingress capsules use a keyed input MAC. Legacy version-1
capsules are accepted only outside protected scopes. Sender, session key,
conversation id, timestamps and chat-profile metadata remain visible; content
length also remains observable. Restart hydration reads that scope and
keeps the newest timestamped turns. Erasure rebuilds cached history from the
remaining persisted message identities, without positional matching.

## Lifecycle and actions

prx emits `started` on turn entry, `thinking` before model work,
`tool_running` while tools run, `awaiting_approval` at a real approval wait,
and `done` after the final surface. Terminal errors emit `failed`.

| Host task kind | Task update state / action |
| --- | --- |
| `start` | `running` |
| `accept` | `running` |
| `decline` | `rejected`; cancel active work |
| `cancel` | `cancelled`; cancel active work |
| `resolve` | Task `done`; the parent turn retains its own lifecycle |

`tool_running` and `awaiting_approval` come from prx execution and approval
state, not invented host event kinds. Final surfaces precede `done`.
Surface actions retain their structured `values` and original conversation
kind, including after a turn has closed, and enter the normal input queue.
They never grant tool authority. The host validates ramflux grants and sends
the dedicated `approval.decision`; its pending conversation and initiating
principal/device must match. A forged `approval_id` in surface values cannot
approve anything. Delegated tasks carry the explicit active-turn context.

Deletion requires a completed hello and an unpaused, unlocked account. Invalid
parameters, an unavailable eraser, a failed checkpoint, or an unsupported
backend produce a JSON-RPC error instead of `{erased:true}`.

## FOLLOWUPS

| Item | Status | Boundary |
| --- | --- | --- |
| Runtime `reply.task_propose` producer | Deferred | Wire encoding exists; the turn loop does not autonomously propose tasks. |
| Runtime `reply.attachment` producer | Deferred | Wire encoding exists; generated attachments are not sent by the turn loop. |
| Replay across process restart | Deferred | Unacknowledged replay is currently process-local. |
| Remaining low-priority review findings | Deferred | Cursor/action deduplication, metadata hardening, and bounded-resource refinements remain separate followups. |

### Admission recovery and snapshots

Surface actions enter the same conversation admission policy as messages. An
immediately admitted action is delivered at its position in the bridge event
stream; queued actions retain conversation queue order. The forwarder never
sends an action back into its own bounded input channel.

An erase refusal does not suspend or cancel input. Once an erase is accepted,
queued work stays suspended until that job succeeds or reports an error. Every
error releases the admission barrier and deletion marker before the host receives
the error, so later inputs can run. A begun deletion retains its durable tombstone
and retry metadata on failure; the host must retry to complete the erasure. Only
a completed erase returns `erased:true`. A stopped worker follows the same error
and admission recovery path.

Scoped core memories carry a Ramflux channel, keyed account owner and keyed
conversation session identity. Metadata attribution keeps the sealed payload out
of plaintext topic classification and its unindexed copies. They are excluded from the operator snapshot. Export and cold hydration
also exclude legacy `rf:` keys; hydration rejects Ramflux origin markers. An
existing snapshot is rewritten even when no operator rows remain, so an older
scoped-only export cannot survive the next export. Deleting
a scoped row therefore needs no snapshot mutation. Operator snapshots keep their
existing operator-only attribution rules.
