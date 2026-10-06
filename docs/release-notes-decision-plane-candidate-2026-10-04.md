# Decision-plane candidate notes — 2026-10-04

This is an unreleased local candidate, not an acceptance or deployment record.
The latest centralized compilation checkpoint reported when these notes were
prepared was check698. It does not establish that the subsequent KNN producer,
backend, replay, factory and default-on changes compile together, and compilation
alone does not establish runtime correctness. Root integration will update the
candidate evidence separately; no final release version is assigned here.

## Intended behavior and current limits

The decision plane binds each admitted model turn to its real input, owner,
configuration and selected capabilities. Recall is a selected Retrieve operation;
Channel provider execution follows the smart-group admission gate. Closing
instructions retain outbound identity/authorization checks. The prepared source
and init-template cutover enables the local decision plane by default, without
enabling remote decision I/O. This cutover is not yet an accepted release and
does not modify deployed configuration. See [decision plane](decision-plane.md),
[tools](tools.md) and [channels](channels.md).

Durable ingress and terminal recovery use authenticated original records,
bounded reads and tracked owner lifetimes. Recovery can reconcile retained real
terminal evidence; absent or uncertain completion stays interrupted/unconfirmed.
It does not resend a provider call, fabricate a successful settlement, or erase
incurred cost. A PostgreSQL connection/partition failure does not prove that an
already-sent provider request stopped. Child recovery binds logical parent/child
lineage to the actual store namespace and private execution identity instead of
substituting one identifier for another. These paths still require real restart,
cancellation and child-process validation.

Local KNN uses a concrete local hash embedding instance, an actual purpose-bound
history grant, bounded owner/session/policy history and a dependent PMM model
readmission. SQLite and PostgreSQL implement the history/learning ports; Lucid
uses its local SQLite delegation. The versioned learning policy attributes the
actual serving attempt and the terminal result. A successful physical response
followed by terminal failure is not successful turn training. All-failed traces
and outcomes without a serving attempt are refused for serving training; the
current consumer does not collect every failure path. Old policy records are
not silently imported into the new domain. Repeated turns on one embedded Agent
share its session; constructing another Agent or restarting does not restore the
old session statistics. See [Router](router.md) and
[routing and cost lifecycle](provider-routing-cost-lifecycle.md).

The separate billing-bound transport contract (H1) remains open. Static prices,
timeouts and post-hoc accounting do not authorize another physical send. Paid
embedding, premium AutoMix retry and Channel auxiliary vision summaries retain
`BudgetUnproven`; auxiliary vision summaries are unavailable. AutoMix retains
the actual cheap response/trace when declining escalation. This is an explicit
feature limitation, not completion of those paid paths.

## Configuration migration and rollback

D-073/D-074 distinguish versioned explicit settings from recognized historical
defaults. With `schema_version = 1`, explicit `enabled = false` remains false,
even if the init sidecar records the same old default. Recognized unversioned
legacy-default signatures, including the old bare false template, treat that
false as unset and therefore follow the candidate's adopted central default.
Unversioned false alongside modern non-legacy settings remains explicit. Loading
maps configuration in memory; a later save writes the versioned form. Unsupported
legacy values and conflicting replacement keys are refused with file/key errors.
See [configuration](configuration.md) for the complete migration contract.

For an unambiguous disable when preparing rollback, retain the version marker
with the explicit false value. A bare unversioned false can be interpreted as an
old default. Use the authorized configuration transaction/rollback flow; these
notes neither perform nor authorize a real configuration change. Keep original
configuration history, keys, owner receipts, source events, decision/outcome
stores, child links and usage records. Do not delete unknown schemas or receipts
to make an older binary appear compatible, and do not raise the existing budget
to pass a migration or performance gate.

Ordinary terminal records without the new optional Router projection preserve
their prior serialized and authenticated form. Readers that do not recognize a
new Router projection reject it. This compatibility is one-way: an older binary
is not guaranteed to read new candidate records. Verify readers and retained
schemas before a binary downgrade. Turning the plane off or reverting code does
not erase liabilities, durable evidence or already-dispatched external effects.
See [provider management](provider-management.md) and [security](security.md).

## Acceptance and delivery boundary

Final combined compilation, fixture migration, tests, feature/architecture gates,
Console reproducible assets/browser behavior, crash/entrypoint regressions and
matched performance sampling remain separate required work. The existing
performance and catalog budgets are unchanged. Independent source and service
reviews must cover the final assembled candidate; these notes are neither review.

Push, merge to main, installation/deployment, restart, real configuration changes
and hosted spending retain their existing separate permission boundaries.
Source changes or a green local compilation do not grant those permissions.
