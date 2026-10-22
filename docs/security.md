# Runtime security

This reference describes the **local development candidate as of 2026-10-04**.
It records intended security boundaries and their current source implementation;
it does not claim behavioral, performance, service or release acceptance. The
CDP security matrix and independent source/service reviews remain required.
Vulnerability reporting and supported-version policy belong to
[Security Policy](../SECURITY.md).

For configuration and secret handling, see
[Configuration — Security](configuration.md#security). For runtime behavior, see
[Decision plane](decision-plane.md), [Tools](tools.md),
[Web Console](web-console.md) and
[Provider routing and cost lifecycle](provider-routing-cost-lifecycle.md).

## Source identity and owner scope

A selected decision does not establish who requested an operation. Migrated
entrypoints bind their actual authenticated or locally established source,
workspace, owner, session, run and configuration generation before dispatch.
A caller-supplied role, model answer, display name or serialized runtime field
cannot replace that source proof. Console/API operations use the authenticated
principal; delegated and scheduled work retain their own source and inherited
scope. A child must not acquire its parent's execution or approval authority by
copying an identifier.

Decision inspection is scoped by workspace and owner. Unknown and out-of-scope
decisions share the absence behavior; inspecting a decision grants no execution
permission. Imported ownerless legacy rows remain local-operator-only. Do not
turn a historical row into a fresh authenticated request.

Enabled decision admission requires the configured durable source prerequisites.
Disabling message recording does not authorize inventing a source event or
silently writing history to satisfy that requirement. A source path that cannot
establish its required proof must refuse before provider/tool execution.

## Capability binding before execution

The model sees the admitted tool definitions. Execution binds the selected
capability, schema fingerprint, catalog generation and source scope, including
freshly discovered capabilities. Structural argument validation precedes an
approval request. Runtime-only argument fields are removed before trusted
execution metadata is attached.

Approval, preparation and queueing can outlive the state under which a call was
selected. The execution path therefore rechecks its authority before backend
invocation. Stale fingerprints, changed scope or missing live authority must not
be repaired by publishing the full registry, switching owner, or reissuing an
old decision. A valid model response still has to pass these execution gates.

## Approval is exact, durable and single use

The candidate's decision approval path persists authenticated pending state and
review evidence. It binds the workspace, owner, session, exact operation and call
digest, capability/catalog identity and expiry. Model text such as “approved”,
a decision backend's answer, or ordinary channel text is not an approval grant.

For a fresh supervised file operation, the candidate can collect a single
`file_write` or `file_edit` argument proposal through the recorded Chat or
Console owner. The source must explicitly identify that operation and pass
all admission checks except the approval that requires its concrete arguments.
The proposal exposes one schema with an empty executable capability set. Its
validated arguments are saved in the existing authenticated, encrypted pending
record. A successful pending write and exact readback are bound into terminal
replay before the result can be classified as awaiting approval. Main-provider
usage remains attached to the owning turn, including on cancellation or failure.
Plan mode and ingress adapters without the actual review/resume path refuse
this collection. The decision-stage deadline continues to bound decision work;
the subsequent main proposal uses the existing main-turn cancellation and idle
supervision. No decision or Router budget is extended.

A tool call that the tool itself only accepts under a signed one-shot grant
bound to its exact target is asked at every autonomy level: `skills_manage`
`lifecycle_publish`, `lifecycle_rollback` and `lifecycle_import_executable`.
The execution service upgrades an allowed decision to Ask for them, and a deny
stays a deny. Under supervised autonomy, naming `skills_manage` collects the
call's arguments through the same Chat or Console owner path as a file
operation, so the pending record names the exact operation. A session "Always"
answer never approves such a call; each needs its own review.

Chat and Console review and explicit resume are distinct operations: recording
approval does not execute the call. Resume runs the exact reviewed operation,
with a durable claim shared with the revocation resolution path. The backend
receives a scoped authority for that claimed invocation. A decision-bound v2
grant that fails verification cannot fall back to a legacy hash-only grant.
Expiry, cross-owner/session use, changed operation or prior consumption refuse
execution. Legacy compatibility data does not establish this durable authority.

A claim whose write/readback becomes uncertain remains consumed or uncertain;
there is no automatic restoration for retry. Revocation can prevent an unclaimed
operation; it cannot undo an already dispatched effect. Channels without an
approval surface remain pending or refuse. They must not emulate approval through
an unauthenticated adapter or a second model request.

## Disconnection, interruption and unknown cost

Cancellation, timeout, a lost connection, or an absent telemetry row does not
prove that a provider request or tool effect never happened. A delegation launch
acknowledgement does not prove that its child completed. Preserve unresolved
usage, reservations and effect evidence until authenticated reconciliation is
possible; do not infer zero cost, a refund, successful completion or permission
to send the request again.

The 2026-10-05 candidate source uses estimated admission at pinned PMM prices,
not a vendor-enforced per-send billing contract. Each supported physical attempt
requires an actual source-owned reservation before dispatch. Missing scope,
pricing, transport accounting support or budget still refuses the send. Fully
reported usage settles at its actual amount; incomplete usage keeps token and
money liability. Underestimation does not erase an incurred charge. A timeout or
loopback address is neither a refund nor billing-finality proof. Decision remote
retry rules remain unchanged; assembled lifecycle and performance acceptance
are still pending.

Durable ingress interruption recovery is integrated in the local candidate,
with retained child telemetry repair added for compilation. It is **not a
released or accepted crash-recovery guarantee**. Its
contract requires input and recovery metadata to commit atomically, real backend
lifetime ownership, bounded reads and authenticated reconciliation. Acquiring a
local file lock or a database advisory lock does not prove remote work stopped.
Missing final evidence remains unconfirmed; recovery must not create a new
provider/tool attempt or fabricate a terminal result. Historical inputs without
the required metadata cannot gain recovery authority through backfill. Kill,
partition, late-writer and fresh-process scenarios still require validation.

## Disabled mode and rollback

The accepted disabled-mode contract uses conservative builtin selection, no
remote **decision-backend** I/O, no decision/outcome database writes, preservation
of existing decision data for read-only inspection, and no full-registry
publication. It introduces no separate Observe mode. Ordinary configured chat
provider traffic, conversation recording and terminal persistence have their own
contracts; disabling decisions is not a network kill switch.

The staged candidate defaults `[causal_tree].enabled` to true and writes
`schema_version = 2` in the full init template. Adoption and end-to-end migration
and rollback acceptance remain pending. The loader preserves versioned explicit
disables, including development snapshots, while recognized unversioned legacy
defaults become unset and inherit true. An explicit rollback to disabled retains
the marker with `enabled = false`; `init --force` cannot refresh that intent to
true. Remote processing remains disabled by default, and supported paid calls
still require owned estimated budget admission. The source change neither enables remote
transports nor modifies a real configuration. Cross-entrypoint disabled behavior
remains subject to the central acceptance matrix.

Use the existing configuration management plan/confirm/apply flow for an
authorized rollback, as described in [Decision plane](decision-plane.md#enable-disable-and-rollback).
Preserve the prior revision, decision data, authenticated source/approval/terminal
evidence and cost journals. Do not delete metadata, pending approvals or unknown
reservations to force startup, bypass a newer schema or make the queue appear
empty. Disabling the plane or reverting code does not cancel dispatched work,
refund charges, make consumed grants reusable or authorize re-execution.

## Evidence and privacy

Configuration, endpoint labels and inspection output must use the existing
redaction and allowlist contracts. Decision backend explanations are not trusted
instructions or user-visible conversation content. Keep credentials, private
URLs, raw sensitive arguments and unredacted provider errors out of diagnostic
exports. Redacted decision projections remain opt-in; source/history recording
has separate privacy controls.

Release evidence must separately establish source/owner isolation, forged and
replayed approval rejection, stale-binding rejection, secret redaction,
disabled-mode behavior, real interruption/reconciliation and rollback behavior.
Compilation or a source review alone does not establish these outcomes. The
existing performance requirements remain unchanged: the 8278 allocation ceiling
and zero catalog-growth tolerance still apply.

## Dependency advisory exception

The current dependency policy retains `RUSTSEC-2026-0381` for the
`webpki-roots` 0.26.11 compatibility bridge. On the locked dependency graph this
bridge re-exports the patched 1.0.10 root set; it has no separate certificate
bundle. `deny.toml` bans other affected versions while the exception remains.
The dated review deadline is 2026-11-21, or earlier if upstream removes the
bridge. The policy files are [deny.toml](../deny.toml) and
[the audit configuration](../.cargo/audit.toml).

A policy-filtered pass is distinct from a raw advisory scan: the unfiltered
scan still reports this bridge advisory. These notes make no additional
advisory or global dependency-acceptance claim.
