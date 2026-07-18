# Permission, PTY, and compliance remediation implementation receipt

Date: 2026-07-18

Design: `task/prx/permission-pty-compliance-remediation-design.md`

Implementation branch: `design/permission-pty-compliance-remediation`

Validated source commit: `d901d91d`

## Delivered scope

- Permission defaults remain workspace-scoped and bounded. Host-wide,
  effectively unbounded operation now requires explicit configuration and is
  reported by `prx doctor`.
- Delegation remains fail-closed for a missing or empty allowlist. The explicit
  `allowed_tools = ["*"]` contract inherits eligible parent tools while still
  excluding `delegate` and preserving the normal execution policy.
- Foreground provider turns now expose a durable terminal acknowledgement.
  Stateful commands, session mutation, and shutdown wait for the terminal
  projection instead of observing `RuntimeDualWriteGuard` as a completion
  proxy.
- The EU AI Act command now emits evidence-bearing controls and offers separate
  report, fail, and strict-warning exit policies.
- T04 emits the configured AI notice only after inbound and outbound authority
  are established and before the first response. Its durable acknowledgement
  stores a hashed peer key, version, and timestamp, not message content.
- A04 installs and verifies forced PostgreSQL RLS for vector-bearing tables.
  Scope is transaction-local and missing or cross-owner scope fails closed.
- C02 generates a versioned, hashed Annex V declaration artifact only for an
  explicitly high-risk classification and an external signature reference.
  It neither signs nor submits the artifact.
- M04 provides a durable Article 73 incident ledger with awareness time,
  causal link, severity, jurisdiction, owner, conditional deadlines, initial
  report, supplements, closure, immutable event hashes, and explicit
  `automatically_submitted = false` exports.

The implementation guidance and rollback procedures are in
`docs/compliance-controls.md`. The legal mapping is pinned to the official
Regulation (EU) 2024/1689 text:
<https://eur-lex.europa.eu/eli/reg/2024/1689/oj?locale=en>.

## Source validation

The following gates passed on the implementation branch:

```text
cargo fmt --all -- --check
cargo check --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Full test result highlights:

- library: 5795 passed, 0 failed, 7 ignored;
- binary: 33 passed;
- architecture: 10 passed;
- Chat PTY: 31 passed, 0 failed, 1 ignored;
- documentation tests: 2 passed, 2 ignored.

Live PostgreSQL verification used
`postgresql:///account?host=/var/run/postgresql&port=5366` and passed all three
focused tests: embedding reindex, memory-fabric conformance, and vector RLS
missing-scope/cross-owner denial.

Markdown lint passed. The optional repository link-check wrapper could not run
because `lychee` is not installed in the environment; this is recorded as a
tooling limitation, not a successful link check.

## Compliance gate result

Against the active local configuration, the deployed report contains 24
controls: 3 pass, 3 warning, 0 fail, 17 unknown, and 1 not applicable.

- report mode exited 0;
- `--fail-on fail` exited 0;
- strict `--fail-on warning` exited 1 as designed.

Unknown and warning are not represented as compliance success. They identify
operator evidence, classification, or applicability that is absent from the
active configuration.

## Staged deployment and runtime acceptance

Track A was built and deployed separately before the combined remediation, as
required by the design. Its binary SHA-256 was
`cc0ea60fd8ecb584bf15087f36a53538ccaf9996e8304d834df1c408a6bbef5e`.

The combined deployed `/home/ck/.cargo/bin/prx` binary is version `0.8.13` with
SHA-256
`d983788b5d8f1b36a64da3eb240f3192dc5f6b294c0feaf59474a07e769f0570`.
The user service is active, running, has zero restarts, and `prx doctor`
reports 20 checks OK, 3 warnings, and 0 errors.

In tmux session `demo`, the deployed binary was accepted with real provider
streaming (`PRX_FINAL_DEPLOY_OK`), exit plus `--continue` recovery of that exact
token, and an `/apply` attempt without an applicable fenced diff. `/apply`
failed closed and did not create or mutate `apply.txt`. The captured evidence
is `/opt/worker/tmp/prx-deployed-tmux-acceptance.txt`.

## Recovery and delivery state

The pre-remediation binary is recoverable at
`/opt/worker/tmp/prx-permission-deploy-backups/prx-pre-remediation-65cf3572`.
The staged Track A binary is recoverable at
`/opt/worker/tmp/prx-permission-deploy-backups/prx-track-a-cc0ea60f`.

No remote push was performed. Local `main` merge and its post-merge deployment
are recorded by the final delivery handoff after this receipt commit.
