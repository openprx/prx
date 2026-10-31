//! Complete native H2 intent contract. Owner-issued plan fields are echo data,
//! never constructors for runtime authority. Study has no model action.
use serde_json::Value;

#[rustfmt::skip]
pub(super) fn parameters_schema()->Value {
    serde_json::json!(
{
  "type": "object",
  "properties": {
    "action": {
      "type": "string",
      "enum": [
        "lifecycle_experiment_catalog",
        "lifecycle_experiment_plan",
        "lifecycle_experiment_admit",
        "lifecycle_experiment_run",
        "lifecycle_experiment_inspect",
        "lifecycle_experiment_cancel",
        "lifecycle_experiment_recover",
        "lifecycle_experiment_review",
        "lifecycle_experiment_publish",
        "lifecycle_experiment_annotate",
        "lifecycle_experiment_rollback",
        "lifecycle_experiment_retire",
        "lifecycle_experiment_lessons"
      ],
      "description": "Choose exactly one action. Request DTO mapping: catalog=Catalog; plan=Draft; admit=IssuedPlan; run=IssuedPlan; inspect=Handle; cancel=Handle; recover=Handle; review=Final; publish=Publish; annotate=Annotation; rollback=Rollback; retire=Retirement; lessons=Lessons."
    },
    "request": {
      "oneOf": [
        {
          "$ref": "#/$defs/Catalog"
        },
        {
          "$ref": "#/$defs/Draft"
        },
        {
          "$ref": "#/$defs/IssuedPlan"
        },
        {
          "$ref": "#/$defs/Handle"
        },
        {
          "$ref": "#/$defs/Final"
        },
        {
          "$ref": "#/$defs/Publish"
        },
        {
          "$ref": "#/$defs/Annotation"
        },
        {
          "$ref": "#/$defs/Rollback"
        },
        {
          "$ref": "#/$defs/Retirement"
        },
        {
          "$ref": "#/$defs/Lessons"
        }
      ],
      "description": "Match the DTO named for action. Plan uses business intent; admit/run echo the complete plan result; handles/final selection/lesson refs come from actual prior results. Missing evidence is a reason to inspect or report unavailable, never invent signed data."
    }
  },
  "required": [
    "action",
    "request"
  ],
  "additionalProperties": false,
  "description": "Controlled instruction lifecycle. Start with catalog using {} to discover real candidate skill/pattern IDs, then plan with selected IDs and explicit user budgets. Catalog is bounded read-only discovery, not eligibility or authority; Plan revalidates candidates. Then admit and run the returned immutable plan. Inspect the actual snapshot. If final_selection exists, review it, publish {expected:final_selection}, then annotate the actual committed transaction. Approval is separate from execution: use the original terminal/Console review and explicit resume. Cancel persists a request; recover reconciles original facts; neither proves termination or retries Unknown. Historical inspect/query/replay and fresh authorized maintenance do not resume an old root. Study is local CLI only.",
  "$defs": {
    "Catalog": {
      "type": "object",
      "properties": {
        "after_skill": {
          "anyOf": [
            {
              "$ref": "#/$defs/SkillId"
            },
            {
              "type": "null"
            }
          ],
          "description": "Optional advisory cursor copied from actual catalog next_after_skill; omit or null for the first page."
        }
      },
      "additionalProperties": false,
      "description": "Bounded candidate discovery: no plan/root/owner or MAC fields. Returns state complete|partial|unavailable, skills[{skill_id,display_name,description,patterns[{pattern_id,statement}]}], next_after_skill, observations, scanned_entries and input_bytes. At most 16 skills/8 patterns each/32KiB output; partial/unavailable is not an empty-store proof. Cursor pages skills, not exhaustive patterns. Plan revalidates every selected ID.",
      "examples": [
        {}
      ]
    },
    "Digest": {
      "type": "string",
      "description": "Lowercase SHA256 copied from the actual service result; never guess or recompute owner evidence.",
      "pattern": "^[0-9a-f]{64}$",
      "minLength": 64,
      "maxLength": 64
    },
    "Id": {
      "type": "string",
      "description": "Exact record/request ID, not a display label.",
      "pattern": "^[A-Za-z0-9_-]{1,64}$",
      "minLength": 1,
      "maxLength": 64
    },
    "StageId": {
      "type": "string",
      "description": "Service-issued stage identity; retain unchanged.",
      "pattern": "^[A-Za-z0-9_-]{1,48}$",
      "minLength": 1,
      "maxLength": 48
    },
    "SkillId": {
      "type": "string",
      "description": "Existing managed skill ID chosen in the business request; not its display name.",
      "pattern": "^[a-z0-9][a-z0-9_-]{0,63}$",
      "minLength": 1,
      "maxLength": 64
    },
    "PatternId": {
      "type": "string",
      "description": "Existing verified evidence pattern ID chosen for this skill; a syntactically valid invented ID is not usable evidence.",
      "pattern": "^pat-[0-9a-f]{32}$",
      "minLength": 36,
      "maxLength": 36
    },
    "RevisionId": {
      "type": "string",
      "description": "Actual published/candidate revision copied from a service observation.",
      "pattern": "^r1-[0-9a-f]{64}$",
      "minLength": 67,
      "maxLength": 67
    },
    "Timestamp": {
      "type": "string",
      "description": "RFC3339 UTC timestamp. User-requested expiry must be future and fit the original root; observed timestamps must be copied unchanged.",
      "format": "date-time"
    },
    "Owner": {
      "type": "string",
      "description": "Owner-issued data: copy from the plan; never choose a caller owner or treat this string as authority.",
      "minLength": 1,
      "maxLength": 512
    },
    "Family": {
      "type": "string",
      "description": "Same bounded task family used by the actual adopted evidence.",
      "minLength": 1,
      "maxLength": 128
    },
    "Split": {
      "type": "string",
      "enum": [
        "development",
        "validation",
        "holdout"
      ]
    },
    "IndependentSplit": {
      "type": "string",
      "enum": [
        "validation",
        "holdout"
      ],
      "description": "Independent verification cannot use development. Full publication uses validation plus separately allocated holdout and private gate."
    },
    "Resources": {
      "type": "object",
      "properties": {
        "calls": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX,
          "description": "Provider call count."
        },
        "tokens": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX,
          "description": "Provider tokens."
        },
        "micro_usd": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX,
          "description": "Integer micro-US dollars."
        },
        "tool_calls": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX,
          "description": "Native tool calls."
        },
        "output_bytes": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX,
          "description": "Captured output bytes."
        },
        "scratch_bytes": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX,
          "description": "Owned scratch/native-write bytes."
        }
      },
      "required": [
        "calls",
        "tokens",
        "micro_usd",
        "tool_calls",
        "output_bytes",
        "scratch_bytes"
      ],
      "additionalProperties": false,
      "description": "All six integer units are required. Zero is valid only where the service permits it. Unknown usage retains its original allocation."
    },
    "ActiveResources": {
      "type": "object",
      "properties": {
        "calls": {
          "type": "integer",
          "minimum": 1,
          "maximum": u64::MAX,
          "description": "Provider call count."
        },
        "tokens": {
          "type": "integer",
          "minimum": 1,
          "maximum": u64::MAX,
          "description": "Provider tokens."
        },
        "micro_usd": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX,
          "description": "Integer micro-US dollars."
        },
        "tool_calls": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX,
          "description": "Native tool calls."
        },
        "output_bytes": {
          "type": "integer",
          "minimum": 1,
          "maximum": u64::MAX,
          "description": "Captured output bytes."
        },
        "scratch_bytes": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX,
          "description": "Owned scratch/native-write bytes."
        }
      },
      "required": [
        "calls",
        "tokens",
        "micro_usd",
        "tool_calls",
        "output_bytes",
        "scratch_bytes"
      ],
      "additionalProperties": false,
      "description": "Calls, tokens and output_bytes must be positive for root/model-stage allocations. Every stage allocation and their declared total must fit the explicitly confirmed root ceilings."
    },
    "LessonQueryIntent": {
      "type": "object",
      "properties": {
        "task_family": {
          "$ref": "#/$defs/Family"
        }
      },
      "required": [
        "task_family"
      ],
      "additionalProperties": false,
      "description": "Optional business lookup intent. The owner resolves guidance and compatibility; no caller-supplied evidence snapshot."
    },
    "Draft": {
      "type": "object",
      "properties": {
        "id": {
          "$ref": "#/$defs/Id",
          "description": "New business request ID; plan returns a different owner-derived controlled experiment ID. Reuse the returned plan rather than planning again to refresh time."
        },
        "skill_id": {
          "$ref": "#/$defs/SkillId"
        },
        "patterns": {
          "type": "array",
          "items": {
            "$ref": "#/$defs/PatternId"
          },
          "minItems": 1,
          "maxItems": 8
        },
        "schedule": {
          "type": "string",
          "enum": [
            "single",
            "sequential_2",
            "parallel_2"
          ],
          "default": "single",
          "description": "One bounded cycle, sequential k2, or parallel k2; not unbounded recursion."
        },
        "verification_split": {
          "$ref": "#/$defs/IndependentSplit"
        },
        "resources": {
          "$ref": "#/$defs/ActiveResources"
        },
        "proposal_allocation": {
          "$ref": "#/$defs/ActiveResources"
        },
        "probe_allocation": {
          "$ref": "#/$defs/ActiveResources"
        },
        "evaluation_allocation": {
          "$ref": "#/$defs/ActiveResources"
        },
        "gate_allocation": {
          "anyOf": [
            {
              "$ref": "#/$defs/ActiveResources",
              "description": "Required for publication: an explicit private gate allocation after exact validation and holdout verification."
            },
            {
              "type": "null"
            }
          ]
        },
        "holdout_allocation": {
          "anyOf": [
            {
              "$ref": "#/$defs/ActiveResources",
              "description": "With a gate, use verification_split=validation and allocate a separate final holdout."
            },
            {
              "type": "null"
            }
          ]
        },
        "lesson_query": {
          "anyOf": [
            {
              "$ref": "#/$defs/LessonQueryIntent"
            },
            {
              "type": "null"
            }
          ]
        },
        "elapsed_ms": {
          "type": "integer",
          "minimum": 1,
          "maximum": u64::MAX,
          "description": "Original immutable root lifetime in milliseconds; must fit the launch ceiling."
        },
        "max_input_bytes": {
          "type": "integer",
          "minimum": 1,
          "maximum": 262144
        },
        "max_output_bytes": {
          "type": "integer",
          "minimum": 1,
          "maximum": 65536
        },
        "proposal_timeout_ms": {
          "type": "integer",
          "minimum": 1,
          "maximum": 600000
        }
      },
      "required": [
        "id",
        "skill_id",
        "patterns",
        "verification_split",
        "resources",
        "proposal_allocation",
        "probe_allocation",
        "evaluation_allocation",
        "elapsed_ms",
        "max_input_bytes",
        "max_output_bytes",
        "proposal_timeout_ms"
      ],
      "additionalProperties": false,
      "description": "Business intent for plan. Supply IDs and explicit budgets, not workspace/owner/MAC/pins/proof. The compiler assigns max_concurrent and all authoritative pins. For a publishable final candidate, include holdout_allocation and gate_allocation with primary validation. Allocation sum, suite evidence, policy and actual expiry are checked by the owner.",
      "allOf": [
        {
          "if": {
            "required": [
              "holdout_allocation"
            ],
            "properties": {
              "holdout_allocation": {
                "type": "object"
              }
            }
          },
          "then": {
            "properties": {
              "verification_split": {
                "const": "validation"
              }
            }
          }
        },
        {
          "if": {
            "required": [
              "gate_allocation"
            ],
            "properties": {
              "gate_allocation": {
                "type": "object"
              }
            }
          },
          "then": {
            "required": [
              "holdout_allocation"
            ],
            "properties": {
              "verification_split": {
                "const": "validation"
              },
              "holdout_allocation": {
                "type": "object"
              }
            }
          }
        }
      ]
    },
    "Handle": {
      "type": "object",
      "properties": {
        "experiment_id": {
          "$ref": "#/$defs/Id"
        },
        "plan_sha256": {
          "$ref": "#/$defs/Digest"
        }
      },
      "required": [
        "experiment_id",
        "plan_sha256"
      ],
      "additionalProperties": false,
      "description": "Copy snapshot.handle exactly. A plan response yields experiment_id=plan.spec.id and plan_sha256 from the subsequent admission snapshot; the handle is a selector, never execution authority."
    },
    "Final": {
      "type": "object",
      "properties": {
        "handle": {
          "$ref": "#/$defs/Handle"
        },
        "proposal_id": {
          "type": "string",
          "minLength": 1
        },
        "proposal_stage": {
          "$ref": "#/$defs/StageId"
        },
        "skill_id": {
          "$ref": "#/$defs/SkillId"
        },
        "baseline_revision": {
          "$ref": "#/$defs/RevisionId"
        },
        "baseline_record_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "baseline_generation": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX
        },
        "candidate_revision": {
          "$ref": "#/$defs/RevisionId"
        },
        "candidate_record_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "evaluation_epoch": {
          "type": "integer",
          "minimum": 0,
          "maximum": u32::MAX
        },
        "evaluating_since": {
          "$ref": "#/$defs/Timestamp"
        },
        "suite_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "gate_stage": {
          "$ref": "#/$defs/StageId"
        },
        "gate_witness_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "gate_id": {
          "type": "string",
          "minLength": 1
        },
        "gate_sha256": {
          "$ref": "#/$defs/Digest"
        }
      },
      "required": [
        "handle",
        "proposal_id",
        "proposal_stage",
        "skill_id",
        "baseline_revision",
        "baseline_record_sha256",
        "baseline_generation",
        "candidate_revision",
        "candidate_record_sha256",
        "evaluation_epoch",
        "evaluating_since",
        "suite_sha256",
        "gate_stage",
        "gate_witness_sha256",
        "gate_id",
        "gate_sha256"
      ],
      "additionalProperties": false,
      "description": "Copy actual snapshot.final_selection unchanged for review/publish/annotate. If it is null or absent, inspect/run has not produced this result: do not invent candidate, epoch, gate or hashes. A final expectation is data checked against the original owner under its lock, never a grant."
    },
    "Publish": {
      "type": "object",
      "properties": {
        "expected": {
          "$ref": "#/$defs/Final"
        }
      },
      "required": [
        "expected"
      ],
      "additionalProperties": false,
      "description": "Exact final publication intent; original source review, private gate, local-owner approval and CAS remain mandatory. Committed replay is historical."
    },
    "Annotation": {
      "type": "object",
      "properties": {
        "expected": {
          "$ref": "#/$defs/Final"
        },
        "publication_tx": {
          "type": "string",
          "description": "Copy publication.transaction.transaction_id (or actual committed transaction observation), never an authorization-attempt ID.",
          "minLength": 1
        },
        "task_family": {
          "$ref": "#/$defs/Family"
        },
        "expires_at": {
          "$ref": "#/$defs/Timestamp"
        }
      },
      "required": [
        "expected",
        "publication_tx",
        "task_family",
        "expires_at"
      ],
      "additionalProperties": false,
      "description": "Annotate a real committed publication. This records adopted evidence, not a claim of demonstrated benefit."
    },
    "Rollback": {
      "type": "object",
      "properties": {
        "handle": {
          "$ref": "#/$defs/Handle"
        },
        "prior_revision": {
          "$ref": "#/$defs/RevisionId",
          "description": "Choose an actual previously published target for this skill, not an arbitrary candidate."
        },
        "observed_generation": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX,
          "description": "Actual current active generation observed before rollback; stale generation must be re-observed, not guessed."
        }
      },
      "required": [
        "handle",
        "prior_revision",
        "observed_generation"
      ],
      "additionalProperties": false,
      "description": "Fresh exact rollback authority plus authentic publication history; cannot restart an old root."
    },
    "LessonRef": {
      "type": "object",
      "properties": {
        "adoption_id": {
          "type": "string",
          "minLength": 1
        },
        "adoption_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "publication_tx": {
          "type": "string",
          "minLength": 1
        },
        "adopted_revision": {
          "$ref": "#/$defs/RevisionId"
        },
        "adopted_record_sha256": {
          "$ref": "#/$defs/Digest"
        }
      },
      "required": [
        "adoption_id",
        "adoption_sha256",
        "publication_tx",
        "adopted_revision",
        "adopted_record_sha256"
      ],
      "additionalProperties": false,
      "description": "Copy one actual lessons.references item or snapshot.adoption.records[].reference. IDs and matching strings alone are not adoption authority."
    },
    "Retirement": {
      "type": "object",
      "properties": {
        "handle": {
          "$ref": "#/$defs/Handle"
        },
        "reference": {
          "$ref": "#/$defs/LessonRef"
        },
        "retired": {
          "type": "boolean",
          "description": "true requests monotone retirement; cannot undo an existing retirement."
        },
        "invalidated": {
          "type": "boolean",
          "description": "true requests monotone invalidation; cannot undo existing invalidation."
        }
      },
      "required": [
        "handle",
        "reference",
        "retired",
        "invalidated"
      ],
      "additionalProperties": false,
      "description": "Maintain an authentic adopted reference. Preserve both booleans; false is not a reset command."
    },
    "Lessons": {
      "type": "object",
      "properties": {
        "handle": {
          "$ref": "#/$defs/Handle"
        },
        "task_family": {
          "$ref": "#/$defs/Family"
        },
        "lifetime_ms": {
          "type": "integer",
          "minimum": 1,
          "maximum": u64::MAX,
          "description": "Bounded query lifetime, at most the original plan elapsed_ms; never refreshes the experiment."
        }
      },
      "required": [
        "handle",
        "task_family",
        "lifetime_ms"
      ],
      "additionalProperties": false,
      "description": "Resolve eligible lessons for the actual current target baseline/model/policy. Empty references is valid; lookup is not proof of delivery or quality."
    },
    "ModelBinding": {
      "type": "object",
      "properties": {
        "provider_label": {
          "type": "string",
          "minLength": 1
        },
        "model": {
          "type": "string",
          "minLength": 1
        },
        "temperature_milli": {
          "type": "integer",
          "minimum": 0,
          "maximum": u32::MAX
        }
      },
      "required": [
        "provider_label",
        "model",
        "temperature_milli"
      ],
      "additionalProperties": false,
      "description": "Copy the owner-issued model binding; this does not select a different model or credential."
    },
    "OriginData": {
      "type": "object",
      "properties": {
        "version": {
          "type": "integer",
          "const": 1
        },
        "authority_expires_at": {
          "$ref": "#/$defs/Timestamp"
        },
        "kind": {
          "type": "string",
          "enum": [
            "terminal",
            "gateway"
          ]
        },
        "boot": {
          "type": "string",
          "minLength": 1
        },
        "profile": {
          "$ref": "#/$defs/Digest"
        },
        "actor": {
          "type": "string",
          "minLength": 1
        },
        "target_owner": {
          "$ref": "#/$defs/Owner"
        },
        "workspace": {
          "type": "string",
          "minLength": 1
        },
        "session": {
          "type": "string",
          "minLength": 1
        },
        "run": {
          "type": "string",
          "minLength": 1
        },
        "event": {
          "type": "string",
          "minLength": 1
        },
        "input": {
          "$ref": "#/$defs/Digest"
        },
        "routing": {
          "$ref": "#/$defs/Digest"
        },
        "sequence": {
          "anyOf": [
            {
              "type": "integer",
              "minimum": 0,
              "maximum": u64::MAX
            },
            {
              "type": "null"
            }
          ]
        }
      },
      "required": [
        "version",
        "authority_expires_at",
        "kind",
        "boot",
        "profile",
        "actor",
        "target_owner",
        "workspace",
        "session",
        "run",
        "event",
        "input",
        "routing"
      ],
      "additionalProperties": false,
      "description": "Copy plan.spec.controlled_admission.pin.source wholesale. These are owner-authenticated observations; JSON cannot create a producer, origin proof, approval or continuation."
    },
    "Admission": {
      "type": "object",
      "properties": {
        "pin": {
          "type": "object",
          "properties": {
            "version": {
              "type": "integer",
              "const": 1
            },
            "source": {
              "$ref": "#/$defs/OriginData"
            }
          },
          "required": [
            "version",
            "source"
          ],
          "additionalProperties": false
        },
        "owner_mac": {
          "$ref": "#/$defs/Digest"
        }
      },
      "required": [
        "pin",
        "owner_mac"
      ],
      "additionalProperties": false,
      "description": "Copy the exact returned controlled_admission object. Never fill in a MAC, boot, principal, owner or source yourself. Missing/mismatched evidence fails closed; it is not repaired by the tool."
    },
    "LessonCompatibility": {
      "type": "object",
      "properties": {
        "model_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "policy_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "source_profile_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "tokenizer_profile_sha256": {
          "$ref": "#/$defs/Digest"
        }
      },
      "required": [
        "model_sha256",
        "policy_sha256",
        "source_profile_sha256",
        "tokenizer_profile_sha256"
      ],
      "additionalProperties": false
    },
    "LessonLimits": {
      "type": "object",
      "properties": {
        "examined": {
          "type": "integer",
          "minimum": 0,
          "maximum": u32::MAX
        },
        "index_bytes": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX
        },
        "record_bytes": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX
        },
        "decoded_bytes": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX
        },
        "lessons": {
          "type": "integer",
          "minimum": 0,
          "maximum": u32::MAX
        },
        "item_bytes": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX
        },
        "context_bytes": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX
        },
        "context_token_upper_bound": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX
        },
        "elapsed_ms": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX
        }
      },
      "required": [
        "examined",
        "index_bytes",
        "record_bytes",
        "decoded_bytes",
        "lessons",
        "item_bytes",
        "context_bytes",
        "context_token_upper_bound",
        "elapsed_ms"
      ],
      "additionalProperties": false
    },
    "LessonStats": {
      "type": "object",
      "properties": {
        "examined": {
          "type": "integer",
          "minimum": 0,
          "maximum": u32::MAX
        },
        "index_bytes": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX
        },
        "decoded_bytes": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX
        },
        "excluded": {
          "type": "integer",
          "minimum": 0,
          "maximum": u32::MAX
        },
        "elapsed_ms": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX
        }
      },
      "required": [
        "examined",
        "index_bytes",
        "decoded_bytes",
        "excluded",
        "elapsed_ms"
      ],
      "additionalProperties": false
    },
    "LessonSnapshotEntry": {
      "type": "object",
      "properties": {
        "reference": {
          "$ref": "#/$defs/LessonRef"
        },
        "evidence_pin_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "sanitized_guidance": {
          "type": "string"
        },
        "guidance_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "expires_at": {
          "$ref": "#/$defs/Timestamp"
        }
      },
      "required": [
        "reference",
        "evidence_pin_sha256",
        "sanitized_guidance",
        "guidance_sha256",
        "expires_at"
      ],
      "additionalProperties": false
    },
    "LessonSnapshot": {
      "type": "object",
      "properties": {
        "schema_version": {
          "type": "integer",
          "minimum": 0,
          "maximum": u32::MAX
        },
        "owner_scope": {
          "$ref": "#/$defs/Owner"
        },
        "task_family": {
          "$ref": "#/$defs/Family"
        },
        "query_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "index_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "compatibility": {
          "$ref": "#/$defs/LessonCompatibility"
        },
        "exclusions": {
          "type": "array",
          "items": {
            "type": "string"
          }
        },
        "limits": {
          "$ref": "#/$defs/LessonLimits"
        },
        "stats": {
          "$ref": "#/$defs/LessonStats"
        },
        "entries": {
          "type": "array",
          "items": {
            "$ref": "#/$defs/LessonSnapshotEntry"
          },
          "maxItems": 2
        },
        "formatter_version": {
          "type": "string"
        },
        "planned_at": {
          "$ref": "#/$defs/Timestamp"
        },
        "expires_at": {
          "$ref": "#/$defs/Timestamp"
        }
      },
      "required": [
        "schema_version",
        "owner_scope",
        "task_family",
        "query_sha256",
        "index_sha256",
        "compatibility",
        "exclusions",
        "limits",
        "stats",
        "entries",
        "formatter_version",
        "planned_at",
        "expires_at"
      ],
      "additionalProperties": false,
      "description": "Optional owner-resolved guidance snapshot copied from the returned plan only. Never supply your own sanitized guidance or compatibility claims."
    },
    "Pins": {
      "type": "object",
      "properties": {
        "skill_id": {
          "$ref": "#/$defs/SkillId"
        },
        "baseline_revision": {
          "$ref": "#/$defs/RevisionId"
        },
        "baseline_record_digest": {
          "$ref": "#/$defs/Digest"
        },
        "baseline_generation": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX
        },
        "binary_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "config_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "policy_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "catalog_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "environment_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "edit_scope_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "model": {
          "$ref": "#/$defs/ModelBinding"
        },
        "suite_digest": {
          "$ref": "#/$defs/Digest"
        },
        "development_split_digest": {
          "$ref": "#/$defs/Digest"
        },
        "verification_split": {
          "$ref": "#/$defs/IndependentSplit"
        },
        "verification_split_digest": {
          "$ref": "#/$defs/Digest"
        },
        "holdout_split_digest": {
          "anyOf": [
            {
              "$ref": "#/$defs/Digest"
            },
            {
              "type": "null"
            }
          ]
        },
        "lesson_snapshot": {
          "anyOf": [
            {
              "$ref": "#/$defs/LessonSnapshot"
            },
            {
              "type": "null"
            }
          ]
        }
      },
      "required": [
        "skill_id",
        "baseline_revision",
        "baseline_record_digest",
        "baseline_generation",
        "binary_sha256",
        "config_sha256",
        "policy_sha256",
        "catalog_sha256",
        "environment_sha256",
        "edit_scope_sha256",
        "model",
        "suite_digest",
        "development_split_digest",
        "verification_split",
        "verification_split_digest"
      ],
      "additionalProperties": false,
      "description": "Copy exact plan.spec.pins from plan. The runtime resolves these from the real target; caller labels/digests cannot authorize a store/model/suite."
    },
    "Stage": {
      "type": "object",
      "properties": {
        "id": {
          "$ref": "#/$defs/StageId"
        },
        "kind": {
          "type": "string",
          "enum": [
            "model_proposal",
            "solver_trial",
            "independent_verification",
            "selection",
            "gate_review"
          ]
        },
        "dependencies": {
          "type": "array",
          "items": {
            "$ref": "#/$defs/StageId"
          },
          "uniqueItems": true,
          "maxItems": 64
        },
        "input_sha256": {
          "$ref": "#/$defs/Digest"
        },
        "allocation": {
          "$ref": "#/$defs/Resources"
        }
      },
      "required": [
        "id",
        "kind",
        "dependencies",
        "input_sha256",
        "allocation"
      ],
      "additionalProperties": false
    },
    "Spec": {
      "type": "object",
      "properties": {
        "controlled_admission": {
          "$ref": "#/$defs/Admission"
        },
        "schema_version": {
          "type": "integer",
          "const": 1
        },
        "id": {
          "type": "string",
          "description": "Copy owner-issued controlled ID from plan.",
          "pattern": "^h2c-[0-9a-f]{60}$",
          "minLength": 64,
          "maxLength": 64
        },
        "owner_scope": {
          "$ref": "#/$defs/Owner"
        },
        "pins": {
          "$ref": "#/$defs/Pins"
        },
        "resources": {
          "$ref": "#/$defs/ActiveResources"
        },
        "max_concurrent": {
          "type": "integer",
          "minimum": 1,
          "maximum": 64
        },
        "elapsed_ms": {
          "type": "integer",
          "minimum": 1,
          "maximum": u64::MAX
        },
        "created_at": {
          "$ref": "#/$defs/Timestamp"
        },
        "expires_at": {
          "$ref": "#/$defs/Timestamp"
        },
        "stages": {
          "type": "array",
          "items": {
            "$ref": "#/$defs/Stage"
          },
          "minItems": 1,
          "maxItems": 64
        }
      },
      "required": [
        "controlled_admission",
        "schema_version",
        "id",
        "owner_scope",
        "pins",
        "resources",
        "max_concurrent",
        "elapsed_ms",
        "created_at",
        "expires_at",
        "stages"
      ],
      "additionalProperties": false,
      "description": "Copy the complete server-issued immutable spec, including all pins and MAC. Do not reconstruct it from business inputs. Native controlled admit/run require controlled_admission even though the legacy general Rust DTO permits None."
    },
    "Operation": {
      "oneOf": [
        {
          "type": "object",
          "properties": {
            "operation": {
              "type": "string",
              "const": "proposal"
            },
            "patterns": {
              "type": "array",
              "items": {
                "$ref": "#/$defs/PatternId"
              },
              "minItems": 1,
              "maxItems": 8
            },
            "max_input_bytes": {
              "type": "integer",
              "minimum": 1,
              "maximum": 262144
            },
            "max_output_bytes": {
              "type": "integer",
              "minimum": 1,
              "maximum": 65536
            },
            "timeout_ms": {
              "type": "integer",
              "minimum": 1,
              "maximum": 600000
            }
          },
          "required": [
            "operation",
            "patterns",
            "max_input_bytes",
            "max_output_bytes",
            "timeout_ms"
          ],
          "additionalProperties": false
        },
        {
          "type": "object",
          "properties": {
            "operation": {
              "type": "string",
              "const": "evaluation"
            },
            "proposal_stage": {
              "$ref": "#/$defs/StageId"
            },
            "split": {
              "$ref": "#/$defs/Split"
            }
          },
          "required": [
            "operation",
            "proposal_stage",
            "split"
          ],
          "additionalProperties": false
        },
        {
          "type": "object",
          "properties": {
            "operation": {
              "type": "string",
              "const": "verify"
            },
            "evaluation_stage": {
              "$ref": "#/$defs/StageId"
            }
          },
          "required": [
            "operation",
            "evaluation_stage"
          ],
          "additionalProperties": false
        },
        {
          "type": "object",
          "properties": {
            "operation": {
              "type": "string",
              "const": "development_probe"
            },
            "proposal_stage": {
              "anyOf": [
                {
                  "$ref": "#/$defs/StageId"
                },
                {
                  "type": "null"
                }
              ]
            }
          },
          "required": [
            "operation"
          ],
          "additionalProperties": false
        },
        {
          "type": "object",
          "properties": {
            "operation": {
              "type": "string",
              "const": "development_proposal"
            },
            "role": {
              "type": "string",
              "enum": [
                "general",
                "domain",
                "compose"
              ]
            },
            "patterns": {
              "type": "array",
              "items": {
                "$ref": "#/$defs/PatternId"
              },
              "minItems": 1,
              "maxItems": 8
            },
            "probe_stages": {
              "type": "array",
              "items": {
                "$ref": "#/$defs/StageId"
              }
            },
            "parent_selection": {
              "anyOf": [
                {
                  "$ref": "#/$defs/StageId"
                },
                {
                  "type": "null"
                }
              ]
            },
            "lesson_context_sha256": {
              "anyOf": [
                {
                  "$ref": "#/$defs/Digest"
                },
                {
                  "type": "null"
                }
              ]
            },
            "max_input_bytes": {
              "type": "integer",
              "minimum": 1,
              "maximum": 262144
            },
            "max_output_bytes": {
              "type": "integer",
              "minimum": 1,
              "maximum": 65536
            },
            "timeout_ms": {
              "type": "integer",
              "minimum": 1,
              "maximum": 600000
            }
          },
          "required": [
            "operation",
            "role",
            "patterns",
            "probe_stages",
            "max_input_bytes",
            "max_output_bytes",
            "timeout_ms"
          ],
          "additionalProperties": false
        },
        {
          "type": "object",
          "properties": {
            "operation": {
              "type": "string",
              "const": "development_selection"
            },
            "probe_stages": {
              "type": "array",
              "items": {
                "$ref": "#/$defs/StageId"
              }
            },
            "parent_selections": {
              "type": "array",
              "items": {
                "$ref": "#/$defs/StageId"
              }
            }
          },
          "required": [
            "operation",
            "probe_stages"
          ],
          "additionalProperties": false
        },
        {
          "type": "object",
          "properties": {
            "operation": {
              "type": "string",
              "const": "selected_evaluation"
            },
            "selection_stage": {
              "$ref": "#/$defs/StageId"
            },
            "split": {
              "$ref": "#/$defs/Split"
            },
            "validation_stage": {
              "anyOf": [
                {
                  "$ref": "#/$defs/StageId"
                },
                {
                  "type": "null"
                }
              ]
            }
          },
          "required": [
            "operation",
            "selection_stage",
            "split"
          ],
          "additionalProperties": false
        },
        {
          "type": "object",
          "properties": {
            "operation": {
              "type": "string",
              "const": "gate_review"
            },
            "selection_stage": {
              "$ref": "#/$defs/StageId"
            },
            "validation_stage": {
              "$ref": "#/$defs/StageId"
            },
            "validation_verifier": {
              "$ref": "#/$defs/StageId"
            },
            "holdout_stage": {
              "$ref": "#/$defs/StageId"
            },
            "holdout_verifier": {
              "$ref": "#/$defs/StageId"
            }
          },
          "required": [
            "operation",
            "selection_stage",
            "validation_stage",
            "validation_verifier",
            "holdout_stage",
            "holdout_verifier"
          ],
          "additionalProperties": false
        }
      ],
      "description": "Copy only operations returned by plan; operation hashes and dependency graph are frozen. No StudyTrial action or operation is exposed on this model surface."
    },
    "IssuedPlan": {
      "type": "object",
      "properties": {
        "spec": {
          "$ref": "#/$defs/Spec"
        },
        "operations": {
          "type": "object",
          "additionalProperties": {
            "$ref": "#/$defs/Operation"
          },
          "minProperties": 1,
          "maxProperties": 64
        }
      },
      "required": [
        "spec",
        "operations"
      ],
      "additionalProperties": false,
      "description": "The exact complete object returned by lifecycle_experiment_plan. Copy both spec and operations unchanged to admit/run. This is verifiable data, not runtime authority; do not regenerate any nested owner/MAC/pin/source fields."
    }
  },
  "allOf": [
    {
      "if": {
        "properties": {
          "action": {
            "const": "lifecycle_experiment_catalog"
          }
        },
        "required": [
          "action"
        ]
      },
      "then": {
        "properties": {
          "request": {
            "$ref": "#/$defs/Catalog"
          }
        }
      }
    },
    {
      "if": {
        "properties": {
          "action": {
            "const": "lifecycle_experiment_plan"
          }
        },
        "required": [
          "action"
        ]
      },
      "then": {
        "properties": {
          "request": {
            "$ref": "#/$defs/Draft"
          }
        }
      }
    },
    {
      "if": {
        "properties": {
          "action": {
            "const": "lifecycle_experiment_admit"
          }
        },
        "required": [
          "action"
        ]
      },
      "then": {
        "properties": {
          "request": {
            "$ref": "#/$defs/IssuedPlan"
          }
        }
      }
    },
    {
      "if": {
        "properties": {
          "action": {
            "const": "lifecycle_experiment_run"
          }
        },
        "required": [
          "action"
        ]
      },
      "then": {
        "properties": {
          "request": {
            "$ref": "#/$defs/IssuedPlan"
          }
        }
      }
    },
    {
      "if": {
        "properties": {
          "action": {
            "const": "lifecycle_experiment_inspect"
          }
        },
        "required": [
          "action"
        ]
      },
      "then": {
        "properties": {
          "request": {
            "$ref": "#/$defs/Handle"
          }
        }
      }
    },
    {
      "if": {
        "properties": {
          "action": {
            "const": "lifecycle_experiment_cancel"
          }
        },
        "required": [
          "action"
        ]
      },
      "then": {
        "properties": {
          "request": {
            "$ref": "#/$defs/Handle"
          }
        }
      }
    },
    {
      "if": {
        "properties": {
          "action": {
            "const": "lifecycle_experiment_recover"
          }
        },
        "required": [
          "action"
        ]
      },
      "then": {
        "properties": {
          "request": {
            "$ref": "#/$defs/Handle"
          }
        }
      }
    },
    {
      "if": {
        "properties": {
          "action": {
            "const": "lifecycle_experiment_review"
          }
        },
        "required": [
          "action"
        ]
      },
      "then": {
        "properties": {
          "request": {
            "$ref": "#/$defs/Final"
          }
        }
      }
    },
    {
      "if": {
        "properties": {
          "action": {
            "const": "lifecycle_experiment_publish"
          }
        },
        "required": [
          "action"
        ]
      },
      "then": {
        "properties": {
          "request": {
            "$ref": "#/$defs/Publish"
          }
        }
      }
    },
    {
      "if": {
        "properties": {
          "action": {
            "const": "lifecycle_experiment_annotate"
          }
        },
        "required": [
          "action"
        ]
      },
      "then": {
        "properties": {
          "request": {
            "$ref": "#/$defs/Annotation"
          }
        }
      }
    },
    {
      "if": {
        "properties": {
          "action": {
            "const": "lifecycle_experiment_rollback"
          }
        },
        "required": [
          "action"
        ]
      },
      "then": {
        "properties": {
          "request": {
            "$ref": "#/$defs/Rollback"
          }
        }
      }
    },
    {
      "if": {
        "properties": {
          "action": {
            "const": "lifecycle_experiment_retire"
          }
        },
        "required": [
          "action"
        ]
      },
      "then": {
        "properties": {
          "request": {
            "$ref": "#/$defs/Retirement"
          }
        }
      }
    },
    {
      "if": {
        "properties": {
          "action": {
            "const": "lifecycle_experiment_lessons"
          }
        },
        "required": [
          "action"
        ]
      },
      "then": {
        "properties": {
          "request": {
            "$ref": "#/$defs/Lessons"
          }
        }
      }
    }
  ],
  "examples": [
    {
      "action": "lifecycle_experiment_catalog",
      "request": {}
    },
    {
      "action": "lifecycle_experiment_inspect",
      "request": {
        "experiment_id": "h2c-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "plan_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
      }
    }
  ]
}
    )
}
