# Controlled V01 instruction studies

Status: M4 business implementation candidate. Concentrated regression, real runtime and independent benefit acceptance are separate pending evidence. This page does not assert a positive result or authorize V02 rollout.

V01 compares two fixed instruction revisions at the same model, policy and task-input conditions. The owner resolves the baseline and H2 revision from the original qualified experiment's actual committed publication. It does not trust a caller's candidate label or publish a new revision as part of the study.

## Supported task and execution profile

The initial profile is `artifact_backed_instructional_workspace_v1`. Every case must produce an actual positive file effect that is not already fully satisfied by its fixtures. Only positive `artifact_contains` and `artifact_sha256` oracles are admitted; prose and a requested tool's arguments are not effect proof. Negative absence and general natural-language quality are outside this initial profile.

Each arm gets a new controlled case DATA root and isolated runtime state. The executor receives the task, fixtures, fixed revision, deadline and exact per-case native tool subset. It never receives validators, expected answers, optimizer lessons, other-arm feedback, the parent lifecycle store or its keys. `file_read`, `file_write` and `file_edit` are the supported ceiling; a case may narrow them further. The original physical accounting, cleanup, budget and cancellation owners remain in charge.

The caller must freeze root clusters, variants, repetitions, arm order, primary metric, practical delta, interval method, cost/latency ceilings and the complete-plan-or-safety-abort rule. Root tasks are statistical units: variants and repetitions inside one root are averaged. Calls, stages and the AccountingScope root are not additional independent samples. Content-derived source hashes and authenticated owner provenance prevent simple ID/partition relabelling from reopening a sample; they do not establish semantic independence by themselves.

One study retains the original 64-stage ceiling, so at most 32 paired trials fit. Larger requests fail explicitly; the system does not split them into fresh budget roots. The preregistered Hoeffding interval uses independent root means and may be wide at this bound. A positive decision requires complete valid evidence, the declared lower-bound practical gain, successful actual tasks without an aggregate success regression, no critical H2 failure and the declared cost/latency ceilings. Inconclusive results retain baseline/Shadow. No universal sample size or gain threshold is supplied.

## Calibration and confirmatory phases

`calibration` runs only Development cases through the same admitted StudyTrial path. It never yields `gain_supported`. After its complete, clean report is available, `confirmatory` pins that owner report and uses independent Holdout cases. The owner derives actual calibration and frozen training/pool source exclusions, rather than accepting an arbitrary list as evidence.

The immutable owner study directory is a retained sample claim. Admission inspects bounded authenticated history under the existing store lock. Reused tasks, corrupt/unknown history, exhausted enumeration or uncertain orphan admission refuse instead of silently reopening holdout. Repeating the same exact identity inspects its durable state; it never repeats a dispatched/unknown stage or renews allowance.

## Local operator commands

The current transport is local CLI/service. Study commands are not exposed as model-facing TUI/Gateway actions. The ordinary instruction-experiment product remains a separate entrypoint family.

```sh
prx skills lifecycle experiment study plan draft.json
prx skills lifecycle experiment study admit frozen-plan.json
prx skills lifecycle experiment study run STUDY_ID EXPERIMENT_SHA256 STUDY_SHA256
prx skills lifecycle experiment study inspect STUDY_ID EXPERIMENT_SHA256 STUDY_SHA256
prx skills lifecycle experiment study report STUDY_ID EXPERIMENT_SHA256 STUDY_SHA256
prx skills lifecycle experiment study export STUDY_ID EXPERIMENT_SHA256 STUDY_SHA256
```

Use the existing explicit owner/workspace configuration and guarded runtime procedure. Keep draft, frozen plan, detailed report and export private. `StudyDraft` is defined in `src/skills/lifecycle/study/types.rs`; its resource and protocol parameters are explicit operator intent, not default approval or runtime authority. Plan returns the resolved frozen intent without a model/tool call. Admission independently re-resolves its source/runtime facts before sealing it.

`inspect` is safe while open: it reports execution progress, cancellation, settled usage, retained liability and admission metadata without task bodies, oracles or per-arm grades. Original generic journal views contain an opaque study receipt reference and execution completion only. Semantically failed tasks still proceed through the fixed paired schedule when execution/accounting is valid; no score-based early stopping selects a favorable denominator.

Detailed `report` and `export` open only after all stages are terminal or durable cancellation has closed new dispatch. Missing work, Unknown, invalid measurements and cleanup uncertainty remain in the full report and cannot support a positive decision. A live/unknown stage is never automatically rerun by report/recovery.

## Evidence and storage accounting

Real DATA artifacts are captured after the case runtime, then retained as bounded owner-private content-addressed effect evidence. Report reconstruction checks the original stage journal, physical scope and attempt IDs, settlement, cleanup, exact revision/hydration and owner MAC. It re-reads actual effect bytes and recomputes the same frozen oracle; a saved score, boolean or caller hash alone is insufficient.

The temporary DATA/runtime-state container must be confirmed removed before copying durable effect evidence. This cleanup is distinct from the intentionally retained private study outputs. Actual encoded receipt/MAC metadata and effect copies are prospectively charged through the existing output/scratch/native-write limits; no ceiling is automatically enlarged. The frozen plan's actual encoded admission metadata requires explicit headroom beyond stage allocations in the same declared root and is reported separately. These are logical file-byte bounds, not an OS disk-quota claim.

Steady-state arm costs, unique optimizer/frozen-pool ancestry, actual calibration costs and admission metadata are separate fields. Original physical call identities are deduplicated. Missing historical evidence is unavailable/unknown, never free. Metadata/library preparation CPU and energy are not separately instrumented; zero-priced local micro-USD usage does not establish zero computation or complete lifecycle efficiency.

## Offline preparation and descriptive reproduction

The Rust owner alone admits/executes/verifies. The external script prepares intent or reproduces descriptive arithmetic from an exact-pinned closed owner export:

```sh
python3 scripts/eval/v01_study.py prepare --draft intent.json --output prepared.json
python3 scripts/eval/v01_study.py prepare --draft confirmatory-intent.json --output prepared.json --calibration-export calibration-export.json --calibration-sha256 TRUSTED_FILE_SHA256
python3 scripts/eval/v01_study.py analyze --export owner-export.json --sha256 TRUSTED_FILE_SHA256
```

The export is UTF-8 JSON: the existing-style outer `value`/`sha256`/`mac` envelope contains `value.report` and its stable `value.evidence_sha256`. The file pin must be obtained from the actual owner export, not invented from modified input. The analyzer has no MAC key, explicitly performs no owner verification, and cannot mint/upgrade a benefit or qualification decision. Prepared intent remains unadmitted until the Rust owner verifies it again.

Concentrated validation must cover ordinary/full-plan/per-stage admission bypasses, kind/input/manifest downgrade, sample relabelling and replay, actual artifact tampering, wrong physical IDs/usage, cleanup uncertainty, cancellation/drop, safe open status, closed feedback, calibration proof, uncertainty/cost decisions and unchanged nonstudy behavior before real comparative execution.
