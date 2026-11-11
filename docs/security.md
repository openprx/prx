# Runtime security

This page describes current runtime boundaries in this tree. It does not claim
release acceptance. Vulnerability reporting and supported versions belong to
[Security Policy](../SECURITY.md). Report vulnerabilities to info@sylvode.org;
do not open a public issue.

For configuration and secret handling, see
[Configuration — Security](configuration.md#security). For related runtime
behavior, see [Decision plane](decision-plane.md),
[Advisory decision engine](decision-engine.md), [Tools](tools.md),
[Web Console](web-console.md) and
[Provider routing and cost lifecycle](provider-routing-cost-lifecycle.md).

Autonomy `full` is the default. It still applies identity-scope ACLs, workspace
path bounds, and tool/command policy. It is not an absence of limits. The
former hourly action cap (`autonomy.max_actions_per_hour`) is retired and
ignored. Child-session parity and PRX-to-PRX messaging are not current
behavior.

## Source identity and owner scope

A selected decision does not establish who requested an operation. Entrypoints
bind the authenticated or locally established source, workspace, owner,
session, run and configuration generation before dispatch. A caller-supplied
role, model answer, display name or serialized runtime field cannot replace
that source proof. Console/API operations use the authenticated principal;
delegated and scheduled work retain their own source and inherited scope. A
child must not acquire its parent's execution or approval authority by copying
an identifier.

Decision inspection is scoped by workspace and owner. Unknown and out-of-scope
decisions share the absence behavior; inspecting a decision grants no execution
permission. Imported ownerless legacy rows remain local-operator-only.

Enabled decision admission requires the configured durable source
prerequisites. Disabling message recording does not authorize inventing a
source event or silently writing history to satisfy that requirement. A source
path that cannot establish its required proof must refuse before provider/tool
execution.

## Tool surface owner

The model sees the admitted tool definitions. The surface is built by the
surface owner: registry ∩ ACL ∩ `always_exclude` / channel exclusions ∩ model
allowlist ∩ catalog eligibility. That assembly does not read the advisory
decision engine, so a turn still has a surface when `[decision_engine] enabled
= false`.

Execution then binds the selected capability, schema fingerprint, catalog
generation and source scope. Structural argument validation precedes an
approval request. Runtime-only argument fields are removed before trusted
execution metadata is attached. Stale fingerprints, changed scope or missing
live authority must not be repaired by publishing the full registry, switching
owner, or reissuing an old decision.

## Approval is exact, durable and single use

Approval binds workspace, owner, session, exact operation and call digest,
capability/catalog identity and expiry. Model text such as “approved”, a
decision backend's answer, or ordinary channel text is not an approval grant.

Two durable namespaces exist and do not mix at the authority key:

- **Decision-keyed** pending records, keyed by decision id.
- **Run-keyed** exact-call records, keyed by run id and call digest, used when
  there is no causal decision. A missing run id refuses the grant.

Listings, claims and revocations keep those keys separate. The Console/gateway
approval inbox merges both namespaces into one list.

For a fresh supervised file operation, the runtime can collect a single
`file_write` or `file_edit` argument proposal through the recorded Chat or
Console owner. The proposal exposes one schema with an empty executable
capability set. Validated arguments are saved in the authenticated, encrypted
pending record. A session "Always" answer never approves a tool that itself
requires a signed one-shot grant bound to its exact target
(`skills_manage` `lifecycle_publish`, `lifecycle_rollback`,
`lifecycle_import_executable`); each needs its own review.

Chat and Console review and explicit resume are distinct: recording approval
does not execute the call. Resume runs the exact reviewed operation, with a
durable claim shared with revocation. Expiry, cross-owner/session use, changed
operation or prior consumption refuse execution. A claim whose write/readback
becomes uncertain remains consumed or uncertain. Channels without an approval
surface remain pending or refuse. They must not emulate approval through an
unauthenticated adapter or a second model request.

## Recovery keys and database backup

SQLite recovery keys live in `.recovery_keys` next to the database. Key bytes
are never logged. Opening refuses incomplete backup directories, so a crash
leftover cannot be restored as a live key repository or turned into a fresh
authority.

A coherent backup is a consistent snapshot of the database plus every key
generation that snapshot names, published by one atomic rename. The runtime
writes the snapshot with `VACUUM INTO`; it does not copy a live write-ahead
log. Copy the completed bundle. A database without its keys cannot recover.
Keys without that database do not start a new authority. Restoring one without
the other is not supported.

## Disconnection, interruption and unknown cost

Cancellation, timeout, a lost connection, or an absent telemetry row does not
prove that a provider request or tool effect never happened. Preserve
unresolved usage, reservations and effect evidence until authenticated
reconciliation is possible; do not infer zero cost, a refund, successful
completion or permission to send the request again.

Each supported physical attempt requires an actual source-owned reservation
before dispatch. Missing scope, pricing, transport accounting support or
budget still refuses the send. Fully reported usage settles at its actual
amount; incomplete usage keeps token and money liability.

Durable ingress interruption recovery is integrated. It is **not a released
crash-recovery guarantee**. Input and recovery metadata must commit atomically.
Acquiring a local file lock or a database advisory lock does not prove remote
work stopped. Recovery must not create a new provider/tool attempt or
fabricate a terminal result.

## Disabled decision plane

`[causal_tree].enabled` defaults to true. `[decision_engine].enabled` defaults
to false. Disabling the decision plane uses conservative builtin selection, no
remote decision-backend I/O, no decision/outcome database writes, preservation
of existing decision data for read-only inspection, and no full-registry
publication. It is not a network kill switch. Ordinary chat provider traffic,
conversation recording and terminal persistence have their own contracts.

The loader preserves versioned explicit disables. Recognized unversioned
legacy `enabled = false` defaults become unset and inherit true; keep the
plane off with `schema_version = 2` and `enabled = false`. See
[upgrade actions](migration-unreleased.md) and
[Decision plane](decision-plane.md#enable-disable-and-rollback).

Disabling the plane or reverting code does not cancel dispatched work, refund
charges, make consumed grants reusable or authorize re-execution. Preserve the
prior revision, decision data, authenticated source/approval/terminal evidence
and cost journals.

## Evidence and privacy

Configuration, endpoint labels and inspection output must use the existing
redaction and allowlist contracts. Decision backend explanations are not
trusted instructions or user-visible conversation content. Keep credentials,
private URLs, raw sensitive arguments and unredacted provider errors out of
diagnostic exports. Redacted decision projections remain opt-in;
source/history recording has separate privacy controls.

## Dependency advisory exception

The 2026-10-25 review removed the `RUSTSEC-2026-0381` exception from both
[deny.toml](../deny.toml) and [the audit configuration](../.cargo/audit.toml).
The updated
[RustSec advisory](https://rustsec.org/advisories/RUSTSEC-2026-0381.html)
lists the `webpki-roots` 0.26.11 compatibility bridge as unaffected when its
separately resolved 1.x dependency is at least 1.0.10.

The locked graph resolves that parent to 1.0.10. The version bans still reject
older root bundles. A fresh raw scan with no ignored advisories reports zero
vulnerabilities on this graph. It still reports informational warnings; the
existing bincode policy exception is separate.
