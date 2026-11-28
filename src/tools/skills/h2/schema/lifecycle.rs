use serde_json::{Map, Value};

pub(super) fn definition_handle() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "experiment_id".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Id"))])),
                ),
                (
                    "plan_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![Value::from("experiment_id"), Value::from("plan_sha256")]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Copy snapshot.handle exactly. A plan response yields experiment_id=plan.spec.id and plan_sha256 from the subsequent admission snapshot; the handle is a selector, never execution authority.",
            ),
        ),
    ]))
}

pub(super) fn definition_final() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "handle".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Handle"))])),
                ),
                (
                    "proposal_id".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        ("minLength".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "proposal_stage".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/StageId"))])),
                ),
                (
                    "skill_id".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/SkillId"))])),
                ),
                (
                    "baseline_revision".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/RevisionId"))])),
                ),
                (
                    "baseline_record_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "baseline_generation".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                    ])),
                ),
                (
                    "candidate_revision".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/RevisionId"))])),
                ),
                (
                    "candidate_record_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "evaluation_epoch".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u32::MAX)),
                    ])),
                ),
                (
                    "evaluating_since".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Timestamp"))])),
                ),
                (
                    "suite_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "gate_stage".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/StageId"))])),
                ),
                (
                    "gate_witness_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "gate_id".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        ("minLength".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "gate_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("handle"),
                Value::from("proposal_id"),
                Value::from("proposal_stage"),
                Value::from("skill_id"),
                Value::from("baseline_revision"),
                Value::from("baseline_record_sha256"),
                Value::from("baseline_generation"),
                Value::from("candidate_revision"),
                Value::from("candidate_record_sha256"),
                Value::from("evaluation_epoch"),
                Value::from("evaluating_since"),
                Value::from("suite_sha256"),
                Value::from("gate_stage"),
                Value::from("gate_witness_sha256"),
                Value::from("gate_id"),
                Value::from("gate_sha256"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Copy actual snapshot.final_selection unchanged for review/publish/annotate. If it is null or absent, inspect/run has not produced this result: do not invent candidate, epoch, gate or hashes. A final expectation is data checked against the original owner under its lock, never a grant.",
            ),
        ),
    ]))
}

pub(super) fn definition_publish() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([(
                "expected".into(),
                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Final"))])),
            )])),
        ),
        ("required".into(), Value::Array(vec![Value::from("expected")])),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Exact final publication intent; original source review, private gate, local-owner approval and CAS remain mandatory. Committed replay is historical.",
            ),
        ),
    ]))
}

pub(super) fn definition_annotation() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "expected".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Final"))])),
                ),
                (
                    "publication_tx".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        (
                            "description".into(),
                            Value::from(
                                "Copy publication.transaction.transaction_id (or actual committed transaction observation), never an authorization-attempt ID.",
                            ),
                        ),
                        ("minLength".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "task_family".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Family"))])),
                ),
                (
                    "expires_at".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Timestamp"))])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("expected"),
                Value::from("publication_tx"),
                Value::from("task_family"),
                Value::from("expires_at"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Annotate a real committed publication. This records adopted evidence, not a claim of demonstrated benefit.",
            ),
        ),
    ]))
}

pub(super) fn definition_rollback() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "handle".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Handle"))])),
                ),
                (
                    "prior_revision".into(),
                    Value::Object(Map::from_iter([
                        ("$ref".into(), Value::from("#/$defs/RevisionId")),
                        (
                            "description".into(),
                            Value::from(
                                "Choose an actual previously published target for this skill, not an arbitrary candidate.",
                            ),
                        ),
                    ])),
                ),
                (
                    "observed_generation".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        (
                            "description".into(),
                            Value::from(
                                "Actual current active generation observed before rollback; stale generation must be re-observed, not guessed.",
                            ),
                        ),
                    ])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("handle"),
                Value::from("prior_revision"),
                Value::from("observed_generation"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Fresh exact rollback authority plus authentic publication history; cannot restart an old root.",
            ),
        ),
    ]))
}

pub(super) fn definition_lesson_ref() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "adoption_id".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        ("minLength".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "adoption_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "publication_tx".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        ("minLength".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "adopted_revision".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/RevisionId"))])),
                ),
                (
                    "adopted_record_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("adoption_id"),
                Value::from("adoption_sha256"),
                Value::from("publication_tx"),
                Value::from("adopted_revision"),
                Value::from("adopted_record_sha256"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Copy one actual lessons.references item or snapshot.adoption.records[].reference. IDs and matching strings alone are not adoption authority.",
            ),
        ),
    ]))
}

pub(super) fn definition_retirement() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "handle".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Handle"))])),
                ),
                (
                    "reference".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/LessonRef"))])),
                ),
                (
                    "retired".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("boolean")),
                        (
                            "description".into(),
                            Value::from("true requests monotone retirement; cannot undo an existing retirement."),
                        ),
                    ])),
                ),
                (
                    "invalidated".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("boolean")),
                        (
                            "description".into(),
                            Value::from("true requests monotone invalidation; cannot undo existing invalidation."),
                        ),
                    ])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("handle"),
                Value::from("reference"),
                Value::from("retired"),
                Value::from("invalidated"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Maintain an authentic adopted reference. Preserve both booleans; false is not a reset command.",
            ),
        ),
    ]))
}

pub(super) fn definition_lessons() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "handle".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Handle"))])),
                ),
                (
                    "task_family".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Family"))])),
                ),
                (
                    "lifetime_ms".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(1_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        (
                            "description".into(),
                            Value::from(
                                "Bounded query lifetime, at most the original plan elapsed_ms; never refreshes the experiment.",
                            ),
                        ),
                    ])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("handle"),
                Value::from("task_family"),
                Value::from("lifetime_ms"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Resolve eligible lessons for the actual current target baseline/model/policy. Empty references is valid; lookup is not proof of delivery or quality.",
            ),
        ),
    ]))
}

pub(super) fn definition_model_binding() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "provider_label".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        ("minLength".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "model".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        ("minLength".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "temperature_milli".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u32::MAX)),
                    ])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("provider_label"),
                Value::from("model"),
                Value::from("temperature_milli"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from("Copy the owner-issued model binding; this does not select a different model or credential."),
        ),
    ]))
}
