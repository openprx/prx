# Remote Nodes

PRX can execute bounded commands and file operations on an authenticated
`prx-node` process over JSON-RPC/HTTP2. The live actions are `list`, `status`,
`exec`, `read`, `write`, and `cancel`.

## Runtime ownership

The process-long `NodeManager` reuses one client per effective node
configuration. This preserves the HTTP/2 pool and circuit-breaker state across
tool calls. Changing a node endpoint, credential, timeout, or retry setting
replaces only that cached client.

Transport retries reuse one JSON-RPC request ID. The server single-flights and
replays `node.exec_shell`, `node.write_file`, and `node.cancel` outcomes for
that ID, rejects an ID reused with different parameters, keeps the outcome for
60 seconds after completion, and caps the replay table. This prevents a lost
response from executing a mutation twice. The replay table is process-local;
it is not a durable cross-restart ledger.

## Client configuration

```toml
[nodes]
enabled = true
request_timeout_ms = 15000
retry_max = 2

[[nodes.nodes]]
id = "prx-node-1"
endpoint = "https://node.example.com:8787"
bearer_token = "replace-with-secret"
hmac_secret = "replace-with-signing-secret"
enabled = true
```

Plain HTTP endpoints are accepted only for `localhost` or `127.0.0.1`.

## Node server configuration

```toml
[nodes.server]
listen_addr = "0.0.0.0:8787"
bearer_token = "replace-with-secret"
hmac_secret = "replace-with-signing-secret"
sandbox_root = "/srv/prx-node"
exec_timeout_ms = 15000
max_output_bytes = 1048576
max_concurrent_tasks = 8
task_result_ttl_ms = 3600000
allowed_commands = ["echo"]
blocked_commands = []
tls_required = true
tls_cert = "/etc/prx-node/tls.crt"
tls_key = "/etc/prx-node/tls.key"
```

`max_output_bytes` caps retained command stdout/stderr and each file read/write
payload. The RPC request and response bodies also have fixed upper bounds.
Command pipes are continuously drained, but bytes beyond the retention limit
are discarded rather than buffered.

## Filesystem boundary

On Unix, file reads and writes start from one long-lived sandbox directory
descriptor and open every path component relative to the preceding descriptor
with symlink following disabled. The final target must be a regular file. This
keeps validation and opening in one descriptor-relative operation chain and
prevents parent/final symlink swaps from escaping the root. Missing parents are
created one component at a time only when `create_dirs` is requested.

Path-safe node file RPC is fail-closed on unsupported non-Unix platforms.
Command execution still has its separate command allow/deny policy; a sandbox
working directory is not a general command filesystem sandbox.

## Callback policy

Async command callbacks accept public HTTPS URLs only. Credentials, local
names, literal private addresses, and any hostname resolving to a private,
loopback, link-local, reserved, or documentation address are rejected. DNS is
resolved immediately before delivery, every answer is checked, the approved
addresses are pinned into a no-proxy client, and redirects are not followed.

## Security boundary

- Bearer authentication is mandatory; optional HMAC signing adds request
  integrity and timestamp freshness.
- Non-loopback listeners require valid TLS material by default.
- Command allowlist and blocklist checks apply before process creation.
- Timeout and cancellation terminate and reap the child process.
- Command output, file payloads, RPC bodies, mutation replays, concurrent async
  tasks, and completed task retention are bounded.
- The Nodes tool still passes through PRX autonomy, approval, audit, and shared
  task-event boundaries before a remote mutation is requested.

Rollback is a revert of the Nodes hardening commit. No configuration migration
is required; existing keys remain compatible.
