# Ramflux channel

The `ramflux` channel connects prx to a local `ramflux-agent-host` over one
Unix socket. ramflux is the transport and trust layer. prx is the execution
layer: model, tool loop, and memory. The socket is the only shared surface.

## Trust boundary

prx does not hold ramflux account keys, does not sign, and does not know
nodes, gateways, or custody. The agent-host owns those. The socket file is
mode `0600`, owned by the same Unix user as prx. prx refuses to listen when
the path is a symlink, the owner uid differs, or the mode is not `0600`.

Default path: `$XDG_RUNTIME_DIR/prx/ramflux.sock`, or `<workspace>/ramflux.sock`
when `XDG_RUNTIME_DIR` is unset. Override with `socket_path`. The parent
directory must be mode `0700` and owned by this user. prx creates a missing
parent at `0700` and refuses an existing parent that is looser. Accept checks
the peer uid and refuses a different user.

The account data key is a 32-byte DEK wrapped by the prx secret store's file
key (`.secret_key`). It is not stored in the OS keychain or Secret Service.
That file key is the current custody boundary. Overwriting a file with zeros
cannot promise erasure on a copy-on-write filesystem such as APFS or btrfs;
destroying the DEK is what makes a leftover ciphertext unreadable.

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

Config alone never selects `full`. In `full`, tool calls run and still emit
`tool.event`. In `supervised`, prx sends `approval.request` and waits for
`approval.decision`. Chat text is never an approval. A decision that does not
arrive before `expires_at_ms` is cancelled.

`prx channel doctor` prints the socket path, the permission check, and the
effective mode, for example `follow_rf→supervised`.

Liveness: `health_check` is true when the socket writer is up and `bridge.ping`
was seen in the last 30 seconds.

## Bridge protocol v1

Newline-delimited JSON-RPC 2.0. Frames larger than 1 MiB are rejected.
Requests should carry `idempotency_key` inside `params`. prx deduplicates
inbound keys for ten minutes. Every outbound frame carries `turn_id`.

### Host to prx

| Method | Role |
| --- | --- |
| `bridge.hello` | Handshake. |
| `bridge.ping` | Liveness. |
| `inbound.message` | User message. `sender` is a principal, `chat.conversation_id` is the reply target. |
| `inbound.surface_action` | A person activated a card. Not a tool approval. |
| `inbound.task_event` | Task event passthrough. |
| `inbound.control` | Membership and presence. `agent_revoked` erases that conversation and keeps the account key. `agent_removed` erases the account, destroys the data key, and leaves a removal marker until `agent_added`. `agent_offline` pauses the account. `agent_online` resumes it. |
| `inbound.message_deleted` | Hard-delete that message's stored bytes. Reply `{erased: true}`. |
| `approval.decision` | `approved`, `denied`, or `expired`. |
| `turn.cancel` | Cancel one turn. |

### prx to host

| Method | Role |
| --- | --- |
| `reply.text` | Text. `final` marks the last segment. |
| `reply.surface` | `create`, `patch`, `stream_append`, or `close`. Blocks are declarative. `fallback_text` is required when blocks are present. |
| `reply.task_update` / `reply.task_propose` | Task state and proposals. |
| `reply.attachment` | Bytes instead of an `[IMAGE:path]` marker. |
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
turn when `on_new_message = "interrupt"`. The host enforces
`turn_wall_clock_secs` (default 120) with `turn.cancel`.

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
the next `bridge.hello` unwraps it again. Associated data is the account id
and the conversation id, so ciphertext cannot be moved between conversations.

The vector index is not given plaintext. If a backend cannot store an
encrypted vector, this namespace is not indexed.

The first time the channel opens a directory that still holds pre-existing
plaintext for this channel, that plaintext is deleted and a notice is logged.

`inbound.message_deleted` and `agent_revoked` overwrite and remove the
ciphertext, media, drafts, tool events, and, for revocation, the wrapped key.
`agent_removed` keeps the bytes and records `agent_removed_at` so later turns
do not retrieve that conversation until the agent is added again.
