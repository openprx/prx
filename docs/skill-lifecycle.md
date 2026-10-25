# Skill lifecycle

PRX ships an owner-scoped lifecycle for instruction skills and reviewed
executable bundles: immutable revisions, task receipts, evidence-linked
knowledge, bounded proposal staging,
frozen-suite evaluation, publication gates, journaled promotion and rollback.
It is disabled by default. Enabling it does not start automatic learning:
managed prompt proposals use the configured evolution scheduler and verified
lifecycle evidence. Reviewed executable bundles ship as SL11 (see
[Executable bundles](#executable-bundles)). Explicit script drafts require
`allow_executable_proposals = true` and a separate operator review before import.
Unmanaged manifest shell/script/HTTP skills are a separate execution
surface that SL11 leaves unchanged; see
[skill catalog trust and execution](skills-catalog-security.md).

## Content and evidence

### Script drafts

`prx skills lifecycle propose-script <template> --script scripts/report.sh
--instructions "Write the requested report"` generates one script through the
managed, accounted evolution model. The template must already declare that
exact `kind = "script"` tool. The model can create or replace only the named
script; it cannot change either manifest, capability requirements or tool
arguments. Other files stay unchanged except the reserved provenance document
`references/prx-script-proposal.json`, which records the template hash, model,
usage and accounting scope. Input/output and call time are bounded.

The result is an owner-private artifact under `self/skill-lifecycle/script-proposals`.
It is not an active revision, is not exposed through the skill catalog and is
not executed. A request with no result is incomplete; the physical accounting
journal remains authoritative for cancelled or failed model requests.

After inspecting the entire generated bundle, the operator may run
`prx skills lifecycle import-executable <candidate-dir> --reviewed <bundle-hash>`.
This is an explicit adoption under the existing Auto-mode publication policy,
not evidence-based automatic promotion. The exact complete hash must match;
the model provenance remains inside the published bundle. Runtime execution
still requires its independent tool grant, pinned hashes, cleared environment
and joined cleanup. Generated scripts and model scores do not establish learned
improvement or replace paired validation and holdout evidence.

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

## New skills

A skill can also be created from repeated work that no managed skill covered.

- **Evidence.** With the lifecycle enabled, a completed root task (never a
  child run) that read no managed skill leaves an experience record through the
  same task-terminal call-in. Only a success, or a failure whose infrastructure
  and permission causes are both positively absent, is kept. The runtime records
  a completed turn as a success and has no task-outcome check, so this is
  evidence of recurring uncovered work, not of failed tasks. A pattern of class
  `missing_skill` needs at least two distinct root tasks. New uncovered-task
  recording stops at the 4096-experience threshold; this is not a hard cap on
  the whole store, since managed-skill experiences can still be recorded.
  With both the evolution scheduler and lifecycle enabled, each evolution tick
  removes at most 128 unreferenced terminal records past the evolution config's
  `runtime.retention.cold_days`. Open tasks, pins, parent lineage and evidence
  cited by retained patterns or proposals stay protected. Unreadable evidence
  defers deletion; the sweep never treats an idle task as finished.
- **Proposal.** `propose_new_skill` writes a fixed skeleton from the request
  (name, description, required tools) and the model may only append text, within
  the same 300-token cap. The bundle is `SKILL.md` alone: no scripts and no
  executable files. Required tools may not exceed the capabilities the patterns
  were scoped to, and the name must not be held by another skill (checked again
  before the first publication).
- **Gate.** The candidate is compared with a frozen no-skill baseline on the
  same suite, validation and holdout, critical cases included. Review,
  authorization and publication are those of a revision; the first publication
  creates the skill and its binding at generation 1 and refuses a `skills/<dir>`
  that the skill does not own.
- **Not a learning claim.** Runs against a mock or loopback model, and a
  scripted proposer, prove the mechanics only.

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
id never matches. A store keeps the owner scope it was first written with:
after changing `owner_scope`, records fail with an owner-mismatch error that
names both scopes. Set the old scope back, or move `self/skill-lifecycle`
aside to start a new store. Nothing is migrated between owners. A configured suite entry registers an already frozen suite;
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
| `lifecycle_import_executable` | `source` directory, `revision_id` (the complete reviewed bundle hash) |

Calls must match the configured owner. Mutations retain the common side-effect
boundary. Agent publication and rollback additionally require a signed,
one-shot approval grant bound to the exact proposal, or the rollback's skill,
revision and observed generation, at every autonomy level. Tool output or a
model's decision cannot supply that authority. Executable import also requires
a signed one-shot grant, bound to both `source` and `revision_id`, and `auto`
mode; see [executable bundles](#executable-bundles). There is no agent
`force-close`, `propose`, suite-authoring or evaluation action.

### Approving publication, rollback and import

`lifecycle_publish`, `lifecycle_rollback` and `lifecycle_import_executable` are
always put to the operator, at every autonomy level including `full`, because
the tool only accepts them with a grant for that exact target. `lifecycle_cancel`
only closes a proposal and needs no such grant. Naming the tool is enough to
start: the agent asks the model for the call, records it as a pending decision
that names the exact operation, and runs nothing until the operator approves it.

1. Ask for the change and name `skills_manage`, for example "use skills_manage
   to import the reviewed bundle in `/srv/skills/report-runner` with revision
   `r1-...`". The turn ends as awaiting approval with a decision id. Under
   `supervised` this is the only form that starts a request; under `full` the
   model may also call the tool in an ordinary turn and gets the same pending
   decision.
2. Inspect the pending call. In chat run `/approval --decision <uuid>`; in the
   web console open the waiting reply. Check the action and its target: the
   `proposal_id` for publish, the skill, revision and observed generation for
   rollback, the `source` directory and complete bundle hash for import.
3. Approve it. In chat run `/approve --decision <uuid>` (or `/approve <n>` with
   the displayed number); in the console use the approve button or
   `POST /api/sessions/{id}/approvals/{decision_id}` with `{"approved":true}`.
   Approval only records the signed grant; it does not run the operation.
4. Resume it. In chat run `/resume-decision <uuid>`; in the console use the
   execute button or `POST /api/sessions/{id}/approvals/{decision_id}/resume`.
   The saved original call runs once with its one-shot grant. A second resume,
   or an approval for a different hash, source, proposal or generation, is
   refused.

An "Always" answer in a terminal prompt never covers these three actions: each
one needs its own approval. Read-only autonomy still refuses them outright, and
a model cannot supply grant material: every `_zc_*` argument it sends is
dropped before the tool runs.

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

## Compatibility projection

For a lifecycle-managed skill, `active/<skill-id>.json` is the authority and
`skills/<dir>/` is a plain directory copy of the active revision, kept for
existing consumers such as discovery. Publish and rollback replace the copy
after the binding commits, using a staged write under
`skills/.openprx-projection/`. Linux and macOS use a native atomic directory exchange where supported
(`renameat2` with `RENAME_EXCHANGE`, or `renameatx_np` with `RENAME_SWAP`).
Filesystems without exchange support fall back to two renames and emit a
warning: the skill may then be briefly absent, but the replacement tree is
never half-written. Other errors are returned without entering the fallback.
The existing recovery protocol also covers this fallback. Reverting this
change restores the previous macOS two-rename behavior; store formats do not change.

Refresh invalidates the catalog snapshots containing metadata and embeddings.
The separate process embedding cache is keyed by description text, so a changed
description cannot reuse the old description's vector.

- A failure after the commit is reported as `CommittedRepairRequired`: the new binding is already live, so this is not an aborted publication. Retrying the same transaction, or the startup recovery sweep, repairs the projection.
- Only the first publication refuses an unmanaged directory; after that the bound `skills/<dir>/` belongs to the lifecycle, and later generations, rollbacks and recovery overwrite it (a hand edit of a bound directory does not survive).
- A first publication refuses, before anything is committed, to replace an existing `skills/<dir>/` that is not its own revision. A symlinked `skills/` or `skills/.openprx-projection/` is refused too.
- Recovery resets the `skills/.openprx-disabled/<dir>` marker of every bound skill to the binding's `enabled` state. A gateway toggle of a bound skill therefore does not survive a recovery pass; change the state with a lifecycle publication or rollback.
- Skills without a binding (legacy skills) are never touched.

## Executable bundles

SL11 adds reviewed executable revisions: a bundle with scripts or declared
`shell`/`script`/`http` tools in `SKILL.toml`.

- **Identity.** The revision id hashes every file, so a script, a reference or
  a file's execute mode each change it; a mode-only change is a new revision.
- **Entry.** An executable revision enters only through an operator. Migration
  plans a legacy executable directory as `ExecutableNeedsReview` and never
  imports it on its own. The operator reviews the directory and publishes it
  with `skills_manage` action `lifecycle_import_executable` (`source`,
  `revision_id`). It needs a signed one-shot grant bound to the directory and
  the complete bundle hash, in every autonomy level, and runs only in `auto`
  mode under a valid publication policy, like any publication. The review
  requires well-formed tools, script paths inside the bundle that are marked
  executable, no executable file that no tool runs, and no untrusted-origin
  marker.
- **Proposer.** A proposal of a managed executable skill may change `SKILL.md`
  text only. Scripts, references, `SKILL.toml` and every mode are carried
  byte-identical, and the capability check refuses any other change. Script
  generation is a next-phase package and is not shipped.
- **Execution.** `skill_execute` for a managed skill resolves the tool from the
  revision the task is pinned to (the pin `skill_read` uses), so a task that
  used revision A runs A after B is published. Only reviewed bytes execute: the
  revision must be an operator import that recorded the review of its own bundle
  hash, or descend (bounded walk, at most 64 parents) from one while differing
  from it in `SKILL.md` only, with every other file and mode byte-identical. A
  store migrated before SL11, a rollback to an unreviewed revision and any
  hand-written candidate are refused with an instruction to re-import the
  directory through `lifecycle_import_executable`. The whole bundle is verified,
  staged into a private run directory with its exact modes, verified again, and
  the files the tool runs are hashed once more immediately before the process
  spawns; a tampered or missing file fails the call. Arguments must be names the
  pinned manifest declares, scalar and bounded. The policy must grant the tool of
  the kind (`shell` or `http_request`) and every capability the revision
  requires; those required capability tools are checked for "not denied"
  (a tool that is `Ask` under supervision passes), and only the `skill_execute`
  call itself is approved at the orchestration boundary. A manifest cannot grant
  access. A managed skill never falls back to the unpinned legacy path: a call
  with no trusted owner, or from another owner, is refused when the skill is
  managed in the configured owner's store. Independently of the configured
  `owner_scope` (including when it is unset), the legacy `skill_execute` path
  refuses any `skills/<dir>/` that is a lifecycle projection. An invalid
  configured `owner_scope` or an unreadable lifecycle store causes refusal,
  never fallback to legacy execution.
- **Environment.** A managed script does not inherit the PRX environment. On the
  native runtime the process starts with an empty environment plus `PATH`, `LANG`,
  `LC_*` and `TZ` from the parent, `HOME` set to its run directory, `TMPDIR`
  inside it, and the `PRX_SKILL_ARG_*` values of the call, so no provider key,
  token or credential reaches it. Unmanaged shell callers are unchanged.
- **Process containment.** The call uses the existing shell/HTTP adapters.
  Managed shell/script runs have a five-minute deadline and joined cleanup:
  the process group is ended when the leader exits, times out or is cancelled;
  the adapter reaps the child and joins the output readers before normal call
  completion, then removes the run directory. Limit: a script that
  deliberately starts a process in a new session (`setsid`) escapes the group.
  Today the containment for that case is the docker `RuntimeAdapter`; a cgroup v2
  `cgroup.kill` based teardown is next-phase backlog.
- **Evaluation.** Executable revisions use the same frozen-suite evaluator.
  Trials can only be offered read tools and workspace file writes, so scripts,
  mail, payments and other external effects are never run in a trial.

## Operator paired evaluation

`prx skills lifecycle suite-digest suite.json` validates an `EvalSuite` and prints
its canonical digest without opening the lifecycle store. Register that digest
under `skills_lifecycle.suites` and select `shadow` mode (evaluation without
publication) or `auto` mode. `draft_only` refuses evaluation. Then run:

```text
prx skills lifecycle install-suite suite.json
prx skills lifecycle review PROPOSAL_ID
prx skills lifecycle evaluate PROPOSAL_ID
```

Evaluation uses `AgentCaseExecutor` with the configured provider/model, fresh
scratch workspaces, pinned baseline/candidate bundles, the suite's budgets and
the owner's cost ledger. It runs validation and holdout once each and records
the deterministic gate; it never publishes. Existing split attempt limits still
apply to retries. Inspect the durable proposal/receipts if interrupted. The
separate `publish` command still requires an eligible gate and Auto mode.
A passing fixture suite demonstrates that task population only. Sustained
production improvement requires later observed tasks and cannot be inferred
from a judge score, a synthetic fixture, or one paired run.
