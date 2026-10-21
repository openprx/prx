# Instruction skill lifecycle

PRX ships the owner-scoped, instruction-only lifecycle: immutable skill
revisions, task receipts, evidence-linked knowledge, bounded proposal staging,
frozen-suite evaluation, publication gates, journaled promotion and rollback.
It is disabled by default. Enabling it does not start automatic learning:
proposal scheduling is not wired in this candidate, and there is no CLI
`propose` command. Executable lifecycle bundles are not shipped yet (SL11);
`allow_executable_proposals = true` is refused when configuration loads.
Existing manifest shell/script/HTTP skills are a separate execution surface;
see [skill catalog trust and execution](skills-catalog-security.md).

## Content and evidence

A **bundle** is the complete validated content of a revision, including its
manifests and resources. A revision has immutable content identity and
provenance; a file's human-readable version is not that identity. The
owner-scoped **store** lives at `<workspace>/self/skill-lifecycle/`, separately
from the memory database. Managed content is not trusted merely because it
resides inside the workspace.

An **active binding** selects a revision at a numbered generation. A task pins
its selected revision and reads those same bytes through `skill_read` even if
a later publication changes the active binding. Existing skill directories
are not rewritten just by enabling the lifecycle; unmanaged reads keep their
existing discovery behavior until a managed binding applies.

Selection, hydration and terminal **receipts** connect skill use to the actual
task outcome. Consolidated **knowledge** references those immutable experiences
and preserves rejected proposals. A failed task is usable proposal evidence
only with verified adherence to the skill's declared tools, available provider
and tool infrastructure, and no permission refusal. Unknown facts, cancelled
or blocked tasks, and ambiguous multi-skill attribution do not justify edits.
See [required tools and learning evidence](skills-catalog-security.md#required-tools-and-learning-evidence).

The managed-provider **proposer** stages a bounded patch to `SKILL.md`; it does
not activate it. Supporting failure patterns must be rechecked against stored
evidence from at least two distinct root tasks. A proposal cannot change its
baseline's capability requirements, permissions, evaluation oracle or harness.
The current proposer caps edits at 300 instruction tokens and 20% of the
baseline; configuration limits do not themselves schedule a proposal run.

A **frozen suite** fixes the cases, fixtures, validators, budgets and
baseline/candidate comparison before evaluation. Its digest identifies the
exact suite. Development, validation and holdout splits remain distinct;
validators are not handed to the executor. The operator registers the governing
suite digest for each skill; a candidate cannot pick a more favorable suite.
The **evaluator** records actual paired executions with pinned revisions and
bounded attempts. Missing cases, timeouts, stale or tampered receipts and
exhausted budgets are incomplete evidence.

The deterministic **gate** re-verifies pinned evaluation receipts and computes
the decision again. Publication requires the prespecified strict improvement,
complete samples, respected budgets and no prohibited regression. Development
runs do not authorize publication. The publication policy requires a holdout
and at least three cases per split; an optional judge can veto, never replace
the deterministic checks. Mock results, local smoke runs, fixed improvement
scores and model self-assessment alone are no evidence of learning and cannot
establish a learned improvement (specification invariant 11).

## Configuration

The full section and owner derivation are in the
[configuration reference](configuration.md). These are the shipped defaults:

```toml
[skills_lifecycle]
enabled = false
mode = "draft_only"
# owner_scope = "<trusted runtime owner id>" # required when enabled
storage_dir = "self/skill-lifecycle"
require_holdout = true
min_cases_floor = 3
max_concurrent_evaluations = 1
max_proposals_per_cycle = 1
max_edit_tokens = 300
max_edit_fraction = 0.20
allow_executable_proposals = false
# [[skills_lifecycle.suites]]
# skill_id = "release-notes"
# suite_digest = "<64 lowercase hex digits>"
```

Settings are operator-owned and read at process start; restart after changing
them. With `enabled = false`, no lifecycle store is opened and no lifecycle
actions are offered to the model. Set `owner_scope` to the exact trusted owner
of the sessions being managed. An owner-isolated caller without its own owner
id never matches. A configured suite entry registers an already frozen suite;
it is not a command to generate or import evaluation cases.

| Mode | Allowed work |
| --- | --- |
| `draft_only` | Drafts and source review; no evaluation or publication |
| `shadow` | Adds bounded evaluation and gating; never publishes |
| `auto` | Allows eligible publication and rollback under the valid policy |

The fixed `storage_dir`, mandatory holdout, `min_cases_floor` range 3–128,
evaluation concurrency range 1–8, proposals-per-cycle range 1–32, edit-token
range 1–100,000 and finite edit fraction greater than zero and at most one are
validated at load. Raising `max_edit_tokens` or `max_edit_fraction` does not
loosen the proposer's fixed 300-token / 20% cap. These limits are policy
settings, not a running scheduler.

## Operator commands and agent actions

All lifecycle CLI commands go through the same service and configured mode.
Replace angle-bracket placeholders with actual ids or observed generations:

```bash
prx skills lifecycle status
prx skills lifecycle list
prx skills lifecycle list --skill release-notes
prx skills lifecycle inspect <proposal-id>
prx skills lifecycle publish <proposal-id>
prx skills lifecycle cancel <proposal-id> --reason "superseded proposal"
prx skills lifecycle rollback --skill release-notes --revision <revision-id> --expected-generation <generation>
prx skills lifecycle force-close <proposal-id> --reason "transaction conflict investigated" --as failed
```

`status` works while disabled; other commands refuse. `list` orders proposals
oldest first, and `inspect` returns proposal detail as JSON. `publish` requires
an eligible proposal and `mode = "auto"`; `rollback` also needs `auto` and the
observed generation. The optional cancellation reason defaults to an operator
cancellation note. `force-close` is CLI-only: `--reason` is required and `--as`
is `failed` (default) or `stale`. It closes a stuck authorized proposal without
publishing; a transaction already live is recorded as committed instead.

When enabled and opened successfully, `skills_manage` adds these actions:

| Action | Arguments beyond `action` |
| --- | --- |
| `lifecycle_status` | None |
| `lifecycle_list` | Optional `skill_id` |
| `lifecycle_inspect` | `proposal_id` |
| `lifecycle_publish` | `proposal_id` |
| `lifecycle_cancel` | `proposal_id`, optional `reason` |
| `lifecycle_rollback` | `skill_id`, `revision_id`, nonnegative integer `expected_generation` |

Calls must match the configured owner. Mutations retain the common side-effect
boundary. Agent publication and rollback additionally require a signed,
one-shot approval grant bound to the exact proposal, or the rollback's skill,
revision and observed generation, at every autonomy level. Tool output or a
model's decision cannot supply that authority. There is no agent
`force-close`, `propose`, suite-authoring or evaluation action.

## Recovery, status and rollback

The publication journal binds the expected predecessor, candidate identity and
authorization. Publication uses generation checks and durable transaction
recovery; rollback creates a new active generation selecting an earlier
verified revision. It does not erase experiences, rejection history or the
publication journal, and in-flight tasks retain their pinned revision.

When the daemon starts with the lifecycle enabled, it launches a background
recovery sweep. A busy store gets at most five attempts, with waits of 250 ms,
500 ms, one second and two seconds. Recovery repairs transactions and proposals;
it never starts new proposals, evaluations or publication cycles. Failures are
logged without stopping the daemon. There is no automatic proposal scheduler
or schedule/interval key in `[skills_lifecycle]` in this candidate.

`status` reports the configured mode and owner, publication blockers, registered
suite count, limits, proposal-state counts, unreadable records and the last
startup recovery summary, including skipped proposals. `list` reports each
proposal's next step and blockers. A store lock conflict is retryable; a skipped
or inconclusive proposal is not a passing proposal.

For binary downgrades, retain the newer build if it wrote terminal facts or
trace fields the older reader rejects. Do not delete learning evidence to make
an older binary accept the store. See the
[upgrade and compatibility notes](migration-unreleased.md).


## Compatibility projection (skills/<dir>/)

For a lifecycle-managed skill, `active/<skill-id>.json` is the authority and `skills/<dir>/` is a plain directory copy of the active revision, kept for existing consumers such as discovery. Publish and rollback replace the copy after the binding commits (staged write under `skills/.openprx-projection/`, then an atomic swap on Linux; elsewhere two renames, which can leave the skill briefly absent but never half-written) and drop the cached catalog snapshots.

- A failure after the commit is reported as `CommittedRepairRequired`. Retrying the same transaction, or the startup recovery sweep, repairs the projection.
- A first publication refuses, before anything is committed, to replace an existing `skills/<dir>/` that is not its own revision. A symlinked `skills/` or `skills/.openprx-projection/` is refused too.
- Recovery resets the `skills/.openprx-disabled/<dir>` marker of every bound skill to the binding's `enabled` state. A gateway toggle of a bound skill therefore does not survive a recovery pass; change the state with a lifecycle publication or rollback.
- Skills without a binding (legacy skills) are never touched.
