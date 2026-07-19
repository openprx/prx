# PRX full-audit remediation plan (2026-07-19)

## Objective

Close the deployed self-check gaps as one bounded change set. Causal Tree remains
the only intentionally disabled capability. Every other capability must report
its real state without a legacy boolean gate, and self-checks must distinguish
available, configured, ready, tested, and unavailable states.

## Confirmed defects

1. Configuration accepts and silently discards obsolete or misspelled keys.
2. Generated configuration templates contain keys that the schema ignores.
3. Audit JSONL writes and rotation are not serialized across logger instances.
4. Host shell execution is direct, but several call paths retain dead
   `SecurityPolicy` or `ShellAuthorization` dependencies.
5. Long-lived health components become stale because their owners never refresh
   them; doctor omits those degraded components and does not detect wacli.
6. The chat self-check infers disabled state from empty configuration and does
   not prove runtime registration or execution.

## Implementation boundaries

### Configuration

- Detect unknown configuration paths after all config fragments are merged.
- Preserve compatibility only for explicitly mapped migrations.
- Migrate the active legacy memory embedding table and HTTP response-size key.
- Remove obsolete enable flags from the active configuration fragments.
- Make generated templates use schema-native keys and semantics.
- Add tests that fail when a generated non-comment key is ignored.

### Audit log

- Share one writer/rotation lock per canonical audit path within the process.
- Serialize one complete JSON object plus newline before writing.
- Add a concurrent multi-logger test that parses every emitted JSONL line.
- Preserve the existing log before repairing concatenated JSON records.

### Shell execution

- Remove `SecurityPolicy` and `ShellAuthorization` from host shell, PTY, Cron,
  and Xin execution signatures when they are not enforcing anything.
- Pass an explicit working directory instead of a security-policy container.
- Keep remote-node isolation, WASM isolation, and file-tool policy out of scope.

### Health and capability truth

- Refresh long-lived component health from the owning loops.
- Make doctor surface every degraded health component and recognize wacli.
- Expose capability states using explicit dimensions rather than an `enabled`
  guess: compiled, registered, configured, ready, and tested.
- Treat an empty hook/plugin/server catalog as available-but-unconfigured, not
  disabled. Causal Tree may report intentionally disabled.

## Verification matrix

| Area | Required evidence |
| --- | --- |
| Config | zero unreported unknown paths; legacy aliases migrate; templates round-trip |
| Audit | concurrent JSONL test; every line in new deployed log parses |
| Shell | no host execution signature depends on SecurityPolicy/ShellAuthorization |
| Health | channels and evolution_judge remain fresh beyond TTL; doctor agrees |
| Capabilities | runtime/API state matches config and registered tools |
| Regression | fmt, check all features, focused tests, full lib tests |
| Deployment | main merge, release binary swap, daemon restart, stable logs |
| Chat | deployed `prx chat` in tmux `demo`; K3 queries each capability and compares evidence |

## Release gates

- No silent configuration loss.
- No invalid JSONL in the post-deploy audit log.
- No false-green health or doctor result.
- No K3 PASS for an unexecuted capability.
- Any external dependency that cannot be exercised must be reported as
  `AVAILABLE_UNCONFIGURED` with the exact missing dependency, never as disabled
  or passed.

## Implemented changes

### Configuration and defaults

- Configuration loading now records unknown paths across the base file and all
  fragments, rejects unmapped keys, and retains only explicit legacy
  migrations.
- Generated configuration examples now round-trip through the active schema.
- The default memory embedding configuration uses the local OpenAI-compatible
  Ollama endpoint, `nomic-embed-text:latest`, and dimension 768 rather than an
  unusable placeholder.
- Legacy capability booleans were removed from the active configuration. Causal
  Tree is the sole intentional feature gate and remains disabled by operator
  choice.

### Shell, audit, and runtime health

- Direct host shell, background shell, PTY, Cron shell, and Xin shell execution
  no longer accept or construct `SecurityPolicy`/`ShellAuthorization` objects.
  Working-directory handling is explicit at each call site.
- Audit records are serialized under a canonical-path process mutex and an
  inter-process file lock. Each append writes exactly one JSON object and one
  newline. The previously damaged log was backed up before 16,296 historical
  events were recovered.
- Runtime owners now refresh their own health heartbeats. Readiness and doctor
  expose degraded owners instead of inferring health from process existence.
- MCP, HTTP request, web fetch, and web search tool surfaces are always
  registered. Missing MCP servers, hook/plugin/skill instances, credentials, or
  HTTP allowlists are reported as available-but-unconfigured.

### Defects found by deployed K3 self-check

The live chat audit and final quiet-period check found three defects that
focused tests had not covered:

1. With zero outbound MCP servers, `mcp_call` emitted an empty JSON Schema
   `enum`. Kimi rejected the entire tool schema with HTTP 400 before the model
   could answer. Empty candidate sets now omit the enum while the runtime still
   returns the precise unconfigured-server error.
2. Fitness telemetry contains long fractional numbers. The payment-card filter
   could extract their digits and, by chance, accept them under Luhn, rejecting
   valid fitness reports and delaying readiness. Decimal-adjacent digit runs are
   now excluded while genuine card-number patterns remain detected.
3. Gateway health was refreshed at startup and by `/health` requests, but not by
   an owner-controlled loop. A quiet listener therefore became stale after 60
   seconds even though it was serving. The gateway now runs a 20-second health
   heartbeat, cancels and joins it when the listener exits, and cannot leave a
   stale heartbeat task behind during supervisor restart.

## Test and deployment receipt

### Source and artifact

- Main remediation merge: `bf5514cb`.
- Embedding-template follow-up: `beb6a84a`.
- Empty MCP enum fix: `9e935476`.
- Decimal telemetry filter fix: `445da942`.
- Quiet-gateway heartbeat fix: `cfee81ee`.
- Release version: `prx 0.8.16`.
- Deployed and `target/release` SHA-256:
  `48fc16a8869f1094436f1341fc7e245ec78cbe661f68bc9f2ef926606475ca52`.
- Runtime unit: `prx.service`, active/running with PID 4117244, started
  2026-07-19 05:23:45 EDT.

### Automated regression

- `cargo fmt --all -- --check`: pass.
- `cargo check -p openprx --all-features`: pass.
- `cargo test -p openprx --lib --all-features`: 5,725 passed, 0 failed,
  6 ignored.
- Focused configuration, audit concurrency, shell, PTY, Cron, Xin, health,
  doctor, tool-registration, MCP-schema, safety-filter, and fitness-storage
  tests: pass.

### Deployed runtime and K3 comparison

The deployed binary was opened as
`/home/ck/.cargo/bin/prx chat --plain -p kimi-code -m k3` in tmux pane
`demo:2.0`. K3 queried the deployed runtime and emitted the terminal marker
`K3_PRX_0816_FULL_SELF_CHECK_DONE`.

The row-level matrix contains 27 entries. The corrected totals are:

- `TESTED_PASS`: 21.
- `AVAILABLE_UNCONFIGURED`: 5 (outbound MCP, hooks, WASM/plugins, skills,
  and `http_request` with an empty domain allowlist).
- `EXPECTED_DISABLED`: 1 (Causal Tree only).
- `FAIL`: 0.

The K3 prose footer said 22/4/1, but its own rows enumerate 21/5/1; acceptance
uses the row-level count. Inbound MCP initialize and tools/list returned HTTP
200 with seven tools. Direct shell recorded 44 successful native executions.
Managed sessions, memory lifecycle, local 768-dimensional embedding, web fetch,
web search, Cron, Xin, evolution, gateway/A2A, sessions/subagent, nodes, and file
read/write/edit were exercised successfully. Empty catalogs were not promoted
to PASS.

All 16,364 audit-log lines parsed as JSON after the final recheck, and all 68
new records after the repaired historical boundary omit `sandbox_backend`. The
known test Cron jobs and memory probes were removed, and no test Cron jobs or
child-agent processes remained. The tmux chat stays open for inspection.

After the quiet-gateway fix, `doctor runtime` was first sampled five consecutive
times after more than 60 seconds without an HTTP health request; each result was
19 ok, 1 warning, and 0 errors. K3 then independently queried the deployed
runtime from the fresh tmux chat and emitted
`K3_PRX_GATEWAY_HEARTBEAT_RECHECK_DONE`. Its comparison observed a 155-second
quiet period while the gateway heartbeat remained fresh, with an age of 11.7
seconds, and reported `FAIL=0`. The remaining doctor warning states that memory
ACL scoping is disabled, which is the requested unrestricted policy. The daemon
journal from the final deployment start has no ERROR, WARN, 401, stale, or panic
records.

K3 attempted to implement its quiet period as one 70-second shell command. The
shell tool correctly enforced its 60-second per-command timeout and recorded
that individual tool call as failed; K3 retried with a short observation call
and completed the comparison. This is a test-command limit violation, not a
host shell authorization, ACL, sandbox, execution, or output-capture failure.

## Configuration remediation follow-up (2026-07-19)

The regression report at
`/home/ck/.openprx/workspace/reports/prx0816_regression_audit_2026-07-19.md`
identified three configuration-level follow-ups. They are now resolved without
adding feature switches:

- **P1 HTTP requests:** active `config.d/tools.toml` contains an explicit
  operator-reviewed allowlist for `api.github.com`, `api.openai.com`,
  `api.anthropic.com`, `api.moonshot.cn`, `api.kimi.com`, and
  `api.search.brave.com`. Empty lists remain deny-all. K3's first GitHub call
  reached the remote and got HTTP 403 because it lacked User-Agent; a retry with
  `User-Agent: OpenPRX-Doctor/0.8.16` returned HTTP 200 and `"Design for
  failure."`, proving the allowlist no longer rejects the request.
- **P2 web search:** `web_search.provider = "duckduckgo"` is explicit in the
  active config and generated templates. The tool is always registered (no
  `enabled` switch); K3 executed real DuckDuckGo searches successfully. The
  host returned empty result sets for the tested queries, which is an upstream
  DuckDuckGo limitation, not a disabled PRX capability. Brave can be selected
  later after its key is configured.
- **P3 doctor noise:** `autonomy.acknowledge_unrestricted_profile` and
  `reliability.acknowledge_single_provider_risk` are non-gating operator
  acknowledgements. They suppress only the matching doctor warnings while
  leaving unrestricted shell behavior and provider fallback behavior unchanged.
  Active config sets both to `true`; the only remaining doctor warning is the
  intentional `memory.acl_enabled=false` posture.

The follow-up implementation is commit `68c8f31b`. It adds schema-native
acknowledgement fields, template documentation, doctor checks, and regression
tests. After deployment, the service is `prx.service` PID 24914, binary/target
SHA-256 `ca762c16dc4e2fc365c7260dabccfa7c99af0ae4014f24518ed9d5a3daa04660`.
The config hot-reload applied generation 2 with `restart_required=[]`; the
post-reload journal has no config errors. Full lib regression is 5,727 passed,
0 failed, 6 ignored; doctor-focused acknowledgement tests are 41/41 and
template tests 22/22.
