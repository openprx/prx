# C05 A/B Lesson Data Contract

Date: 2026-10-30. Status: root-approved minimal contract; implementation and
profile validation pending. A owns types/adoption/query/retention; B consumes
opaque prepared context and owns physical delivery. D owns gate review. No new
memory backend, graph, dispatcher, publication owner or balance is introduced.
Accepted C is 4ac7efad; D remains the sole Cargo owner during parallel work.

## Types and dependency boundary

A defines `skills/lifecycle/adoption/context.rs`; the existing lifecycle owner
reexports the required types at pub(crate) visibility. B must not define a rival
snapshot/context or copy historical gate/policy verification. Concrete source
interfaces will follow these signatures; small typed details can settle directly
between A/B without widening the contract.

```rust
// Serializable immutable DATA, not authority. All digest/ID/length fields validate.
LessonSnapshot {
  schema_version, owner_scope, task_family,
  query_pin, compatibility, exclusions, limits, selection_stats,
  entries: Vec<LessonSnapshotEntry>, // at most two, immutable adoption pins
  formatter_version, planned_at, expires_at
}
LessonSnapshotEntry {
  adoption_id, adoption_sha256, committed_publication_ref,
  adopted_revision, adopted_record_sha256, evidence_pin_sha256,
  sanitized_guidance, guidance_sha256, expires_at
}

// No Deserialize or public constructor. The service builds it from admitted
// frozen inputs and the complete, final development-editor user slot.
PreparedLessonContext { /* private Arc<existing store>, binding, refs, pins */ }
LessonHandoffGuard<'a> { /* private existing shared StoreLock + borrowed context */ }
// Clone + Serialize data; Prepared remains the non-forgeable factory boundary.
LessonRef { adoption_id, adoption_sha256, publication_tx,
            adopted_revision: RevisionId, adopted_record_sha256 }

query_adopted_lessons(query, frozen_limits) -> Result<LessonSnapshot>;
prepare_lesson_context(plan, stage, final_user_slot: &str)
  -> Result<Option<Arc<PreparedLessonContext>>>;
PreparedLessonContext::lock_handoff(actual_model, now)
  -> Result<LessonHandoffGuard<'_>>;
```

Prepared read-only getters: block(), block_sha256(), expected_user_slot_sha256(),
expected_user_slot_bytes(), snapshot_sha256(), refs(), binding(), placement().
Guard getters, visible within lifecycle: store(), skill_id(), prepared(). B takes
its actual model identity from the existing physical accounting owner, not from
model text or an unchecked caller label. The store/skill getters permit existing
private record/fsync helpers; they grant no new publication or model authority.

The prepared constructor verifies actual runtime ExperimentRunBinding (root,
stage, immutable input), admitted DevelopmentProposal operation and its exact
context pin. Generic chat/proposer, solver, validation, holdout, verifier and judge
have None. The context exists only for that editor complete/send lifetime.

## Full logical slot and actual wire validation

A first appends the bounded, uniquely marked untrusted block to the fully rendered
editor user slot. Its private constructor validates the block and records the
complete user-slot SHA256/byte count; it does not retain a second raw prompt copy.
B extracts the supported logical user text from the actual already serialized
physical body, compares full slot bytes/digest, block bytes/digest, placement and
uniqueness, and derives final_context_digest from that ACTUAL extracted context.
A block surviving while the rest of the user slot was clipped is a refusal.
Other JSON string locations do not count as delivery; a copied second block,
unsupported body shape, changed model or incomplete/clipped slot refuses.
No wire reserialization or new independent physical attempt ID is permitted.
The existing keyed whole-body binding and actual receipt identity join this
final context evidence. A pre-injection digest alone is insufficient.

Avoid circular hashes: snapshot core excludes experiment/root/operation hashes.
The deterministic block marker derives from snapshot digest + stage identity,
not from its own digest or operation digest. The block digest is then included
in the optional development operation input; the final frozen spec hashes those
inputs. Prepared context is built AFTER freeze and adds actual root/stage/input
binding. A marker is data identity, never a dispatch nonce or capability.

## Frozen caps and whole-query failure

| Bound | Initial hard ceiling |
| --- | --- |
| Owner/family descriptors examined | 64 |
| Index bytes decoded | 32 KiB |
| One adoption record decoded | 16 KiB |
| All lookup evidence bytes decoded | 1 MiB |
| Returned eligible lessons | 2 |
| One rendered guidance item | 512 ASCII bytes |
| Complete rendered block including delimiters | 1,536 ASCII bytes |
| Guidance token upper bound | 1,536, subject to verified profile below |
| Measured lookup elapsed work | 500 ms |

Limits may be reduced, never silently raised. Every examined descriptor counts,
including exclusions. Evidence verification I/O counts toward decoded-work caps.
Owner/family selects a bounded deterministic index partition before record reads;
read incrementally in stable ID order, never load all owners/families then filter.
All eligibility checks precede output limiting. Too many distractors can exhaust
the examined bound, but cannot consume an eligible output slot. ANY cap exhaustion
fails the entire lookup; no timing-dependent partial snapshot is returned.
Deadline checks bracket bounded regular-file reads and verification steps; an
uninterruptible OS read is not claimed preemptible, and no late result is accepted.

The 1,536-byte context is under the existing 64-KiB fixture/256-KiB proposer input
ceilings and must also fit the actual remaining original input allowance. No root
call/token/USD/tool/output/scratch allowance grows. Input shortage refuses instead
of clipping source, task data or guidance. Pure compilation makes no lookup/model
call. Absent optional query/snapshot/op fields serialize away and preserve old
no-lesson identities. An explicit empty selection may be retained as auditable
empty data; it creates no prepared guidance or delivery claim.

Token algorithm: count bytes of the FINAL rendered ASCII block plus the pinned
profile's conservative boundary/BOS/add-prefix/postprocessor overhead, never
bytes/4. The sum must fit the same 1,536-token ceiling; the byte ceiling is not
raised to absorb overhead. The complete logical user slot and original request
input allowance must also include fixed template/boundary overhead.
This is accepted only for an actual model profile whose tokenizer/manifest is
pinned and checked for byte alphabet/fallback, nonempty byte-consuming tokens and
no ASCII-expanding normalization/pretokenization. Check the actual Splash mapping
through its actual model path/tokenizer evidence, not an alias assertion. Unknown
or unsupported profile refuses preparation/handoff. Initial support may be one
verified Splash profile. No generic tokenizer framework or universal bound claim
is required. Before that evidence is verified, profile support remains unproven.

## Eligibility, origins and exclusion

The service resolves owner/family and target pins from trusted stored/frozen
inputs; caller intent is not an authoritative digest. Model and original composite
policy compatibility are exact in v1. Source compatibility requires the target
pinned baseline equal the donor's ADOPTED revision/record pin; the donor's old
origin baseline is preserved but is not required to equal the target baseline.
No arbitrary descendant graph or cross-version compatibility is inferred.
Enabled, supported source/formatter/profile, expiry, invalidation and retirement
checks run before output selection and again before each physical handoff.

Derive case-ID-independent normalized task/fixture/source-root hashes for every
current registered development/validation/holdout split, and reject overlap with
the donor's retained root hashes. Preserve explicit source-root provenance where
available. Do not invent semantic equivalence detection; unknown provenance is
visible and the supported profile must fail closed where disjointness is needed.
A runtime later-task fixture must use genuinely new same-family task content and
roots, not renamed IDs or wrappers around the old held-out tasks. No old oracle,
case prompt, output or raw asset body enters guidance. Only the supported,
bounded, sanitized adopted SKILL.md/description diff is rendered as untrusted data.
Oversize/unsupported edits are excluded rather than translated or truncated into
a different claimed lesson. Exposure never implies utility or M4 benefit.

## Exact adoption proof and D boundary

A records only an exact already-Committed proposal/publication journal and
matching authorization/candidate/baseline/revision/evaluation/source-review pins,
plus D's opaque verified witness. Private qualification, Shadow eligibility or a
judge boolean cannot create adoption. No publish, reconcile, repair or current
verify_gate call is used for historical proof. The earlier historical API note
still governs publication time/repair evidence; missing proof fails closed.

D stable boundary is:
verify_gate_review_witness(experiment_id, experiment_sha256, GateReviewPin)
-> Result<VerifiedGateReviewHistory>. Pin is only stage/witness_sha256/gate_id/
gate_sha256, not confirmation. D rereads the exact journal and semantically
verifies history. Its result has no Deserialize/public constructor/VerifiedGate
conversion; read-only actual binding, immutable subject/C triple, exact gate pins,
verification bracket and settled physical attempt snapshot are available.
InsufficientTemporalEvidence retains a typed cause. A must not trust the raw
GateReviewWitness record or implement a competing policy/gate algorithm.
Complete-root cost evidence uses distinct original actual attempts, including D's
judge. Missing/external legacy joins stay Unknown/explicitly outside-root, never
invented zero, no second ledger and no duplicated parent/child sums.

## Shared lock, durable delivery and retention

Prepared guard validates only its pinned references/compatibility/expiry under
the existing SHARED store lock. B acquires this guard and writes/readbacks delivery
in the SAME bounded blocking-I/O closure; after shared store it may take the
existing actual skill lock. No shared-to-exclusive upgrade and no lock across
async model/network work. B's durable PossibleHandoffUnknown preparation must
succeed before wire handoff. Failure refuses transport after the existing admitted
Tally/receipt accounting; liability is retained and never called a free attempt.
Each retry/fallback repeats actual-model validation and has its own original
physical attempt join. No mutable lookup or silent context replacement occurs.

A is the ONLY retention writer. B exposes a bounded delivery_retention_refs_locked
reader usable while A already holds the store EXCLUSIVE pruning lock, with no
recursive shared lock acquisition. B does not expire/delete references merely
because snapshot/context expiry elapsed: possible handoff, missing terminal proof,
active/retained referrers and accounting uncertainty remain protected. Default B
reader includes all bounded records; only A applies an explicit existing-retention
policy after all reference/terminal evidence is known, never a new automatic TTL.
New snapshot/adoption/reference writes follow
the same store-lock owner. A protects old proposal.evidence_ids AND the new
proposal.bound_evaluation_attempts chain (attempts, receipts/cases, suites and
immutable revisions), retained/active experiment snapshots, unresolved physical
attempts and unexpired delivery refs. Missing/corrupt own refs retain unknown and
abort unsafe pruning; no timeout unlock or new GC/recovery framework is added.

Verify the actual old pruner/downgrade behavior: ignoring a new directory is not
proof of safe dependency retention. Unsupported older readers must quarantine/
refuse pruning before deletion, with reproducible tests, rather than silently
remove new dependencies. Keep append-only C references intact. No memory update,
root budget increase, holdout-policy reduction or broad entrypoint parity is part
of this contract.
