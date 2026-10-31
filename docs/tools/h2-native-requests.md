# Controlled H2 native requests

The controlled `skills_manage` surface advertises twelve typed lifecycle actions and one read-only candidate catalog action. Its published schema includes the complete request shapes, shared `$defs`, required fields, integer units, supported enums and copy-source instructions. The execution contract additionally binds each action to its particular request type. Ordinary non-H2 skill management retains its existing schema and behavior. Study remains a local CLI surface.

Start with `catalog` using an empty request to discover existing managed skill IDs and candidate pattern IDs, then select them with business intent, a bounded schedule and explicit resource budgets. Plan revalidates every candidate. A pattern-shaped string is not evidence. Do not invent a skill, pattern, digest, source event, owner, MAC or approval to make a request look complete.

## Request map

Every call has exactly `action` and `request`. The action prefix in this table is `lifecycle_experiment_`.

| Action suffix | Request definition | Required business input or actual prior-result source |
|---|---|---|
| `catalog` | `Catalog` / `ExperimentCatalogRequest` | `{}` for the first page; optional `after_skill` copied from the actual `next_after_skill`. Returns bounded candidate IDs/short descriptions and honest `complete`/`partial`/`unavailable` state. No execution handle, owner, MAC or budget root. |
| `plan` | `Draft` / `InstructionCycleDraft` | New request `id`, existing `skill_id`, 1–8 verified `patterns`, `verification_split`, explicit root and proposal/probe/evaluation allocations, immutable `elapsed_ms`, input/output byte caps and proposal timeout. Optional schedule, holdout/gate allocations and lesson-query intent. No owner/workspace/MAC/pin input. |
| `admit` | `IssuedPlan` / `InstructionExperimentPlan` | Copy the **entire actual plan response**, including both `spec` and `operations`. Do not rebuild its owner-derived ID, dates, resources, source, MAC or pins. |
| `run` | `IssuedPlan` / `InstructionExperimentPlan` | Copy the same admitted plan unchanged. A handle alone is not this request. It must still belong to the original live launch/root. |
| `inspect` | `Handle` / `ExperimentProductHandle` | Copy the actual admission/run/inspection snapshot's `handle`: `experiment_id` and `plan_sha256`. |
| `cancel` | `Handle` | Same actual handle. The response records cancellation state; it does not by itself confirm physical termination or cleanup. |
| `recover` | `Handle` | Same actual handle. Reconcile original facts; do not dispatch a replacement or retry an Unknown attempt. |
| `review` | `Final` / `ExperimentFinalExpectation` | Copy actual `snapshot.final_selection` unchanged. A missing/null final selection is not a result to fill in. |
| `publish` | `Publish` / `ExperimentPublishRequest` | `{ "expected": <actual final_selection> }`. Preserve the candidate, baseline generation, epoch, suite and gate identities exactly. |
| `annotate` | `Annotation` / `ExperimentAnnotationRequest` | Actual `expected`, actual committed `publication_tx`, business `task_family` and bounded future `expires_at`. The transaction ID comes from the committed transaction, not an authorization attempt. |
| `rollback` | `Rollback` / `ExperimentRollbackRequest` | Actual `handle`, a genuinely published `prior_revision`, and the last actually observed active `observed_generation`. The committed transaction's `new_generation` is an observation, not a promise that no later write occurred; the owner's CAS decides. The original final expectation's `baseline_revision` identifies its published baseline. |
| `retire` | `Retirement` / `ExperimentAdoptionStatusRequest` | Actual `handle`, an actual adopted `reference`, and both `retired`/`invalidated` booleans. These maintenance flags are monotone; false does not undo an earlier true value. |
| `lessons` | `Lessons` / `ExperimentLessonQueryRequest` | Actual `handle`, same `task_family`, and positive `lifetime_ms` no greater than the original plan's `elapsed_ms`. Results may honestly be empty. |

`Final` includes `handle`, `proposal_id`, `proposal_stage`, `skill_id`, `baseline_revision`, `baseline_record_sha256`, `baseline_generation`, `candidate_revision`, `candidate_record_sha256`, `evaluation_epoch`, `evaluating_since`, `suite_sha256`, `gate_stage`, `gate_witness_sha256`, `gate_id` and `gate_sha256`. All come from the actual final observation. Review/publish never create a gate by accepting these fields.

`LessonRef` includes `adoption_id`, `adoption_sha256`, `publication_tx`, `adopted_revision` and `adopted_record_sha256`. Copy one real `lessons.references[]` item or `snapshot.adoption.records[].reference`.

## Planning units and bounds

Every resource object contains six unsigned integer fields: `calls`, `tokens`, `micro_usd`, `tool_calls`, `output_bytes` and `scratch_bytes`. Calls, tokens and output bytes must be positive for the root and physical model-stage allocations. Zero is allowed only where the original service permits it. The compiler sums stage allocations and checks the declared root, configured policy and original foreground permission ceilings; the schema does not replace those checks.

`schedule` is `single` (default), `sequential_2` or `parallel_2`. Independent `verification_split` is `validation` or `holdout`, never `development`. To produce a publishable final selection, use primary validation and explicitly provide **both** final `holdout_allocation` and private `gate_allocation`. A gate without the final holdout is invalid. No schedule offers unbounded recursion.

`max_input_bytes` is 1–262144; `max_output_bytes` is 1–65536; `proposal_timeout_ms` is 1–600000. Root `elapsed_ms` is positive and remains bounded by the original launch. IDs use the exact record grammar in the schema: skill IDs are lowercase `[a-z0-9][a-z0-9_-]*`, pattern IDs are `pat-` plus 32 lowercase hexadecimal characters, revisions are `r1-` plus 64 lowercase hexadecimal characters and SHA256 values contain exactly 64 lowercase hexadecimal characters. New request IDs use 1–64 ASCII letters/digits/hyphens/underscores. The planner supplies the controlled `h2c-` ID and frozen stage graph.

Timestamps use RFC3339. Task-family text is at most 128 UTF-8 bytes in the owner contract; schema string-length bounds alone do not replace that byte check. Annotation expiry must fit the original owner's deadline/policy. Query lifetime never renews root execution or evidence expiry.

## A native conversation

The operator supplies ordinary business choices, for example: “Discover candidate managed skills and patterns for this task, then select suitable existing IDs. Run one bounded cycle within these explicit budgets, with independent validation, one allocated final holdout and a private gate. Inspect the actual result and request publication approval only if it qualifies.” The model uses the advertised `Draft` fields to form the call. This is not a prompt containing a preassembled signed plan or complete golden tool-call JSON.

After `plan`, pass its returned object as `request` to `admit`, then retain that object for `run`. Use actual result fields for the remaining calls. For example, publication wraps the observed final selection once; it does not flatten, rename or regenerate it. If the full plan result was truncated or unavailable in model context, report that boundary rather than attempting to reconstruct signed data. Native end-to-end acceptance must exercise the real tool-result/context path.

Approval and execution remain separate. A mutating call may first return a pending decision; the original foreground owner reviews it and an explicit resume executes the original sealed call. An ordinary Gateway review cannot manufacture the local-owner publication stamp. Copying owner/MAC/source data never constructs the private producer, once port or continuation. Old-boot execution stays closed; historical maintenance uses current exact authority and authentic history.

## Input discovery boundary

`lifecycle_experiment_catalog` reaches the same bounded owner reader as the local CLI through a fresh source-owned, target-verified, single-use exact call port. It is read-only and does not bind or allocate an execution root. Start with `{"action":"lifecycle_experiment_catalog","request":{}}`; continue only with the actual returned `next_after_skill` when present. Choose a returned `skill_id` and its candidate `pattern_id` values for Plan. Do not turn catalog descriptions into proof of eligibility or fabricate an ID when discovery is partial/unavailable.

The result exposes `state`, `skills`, `next_after_skill`, `observations`, `scanned_entries` and `input_bytes`; each skill has its ID, short display name/description and candidate pattern IDs/statements. It excludes suites/oracles, raw experience, keys, MACs and runtime authority. Pages are bounded to 16 skills, 8 patterns per skill and 32 KiB encoded output; the owner caps total input at 2 MiB, 512 directory entries plus one overflow probe, 1,024 index entries and a cooperative 500 ms scan deadline. See [the owner catalog contract](h2-candidate-catalog.md) for failure and cursor semantics. Plan retains its original full owner validation.

The schema is a source/interface change, not a native model/runtime acceptance result. Provider schema normalization, original DTO deserialization and complete TUI/Gateway positive business flows remain part of the consolidated validation phase.
