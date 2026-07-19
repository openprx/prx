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
  `UNTESTED` with the exact missing dependency, never as disabled or passed.
