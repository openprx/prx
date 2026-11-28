use serde_json::{Map, Value};

pub(super) fn definition_stage() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "id".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/StageId"))])),
                ),
                (
                    "kind".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        (
                            "enum".into(),
                            Value::Array(vec![
                                Value::from("model_proposal"),
                                Value::from("solver_trial"),
                                Value::from("independent_verification"),
                                Value::from("selection"),
                                Value::from("gate_review"),
                            ]),
                        ),
                    ])),
                ),
                (
                    "dependencies".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("array")),
                        (
                            "items".into(),
                            Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/StageId"))])),
                        ),
                        ("uniqueItems".into(), Value::from(true)),
                        ("maxItems".into(), Value::from(64_u64)),
                    ])),
                ),
                (
                    "input_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "allocation".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Resources"))])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("id"),
                Value::from("kind"),
                Value::from("dependencies"),
                Value::from("input_sha256"),
                Value::from("allocation"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
    ]))
}

pub(super) fn definition_spec() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "controlled_admission".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Admission"))])),
                ),
                (
                    "schema_version".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("const".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "id".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        (
                            "description".into(),
                            Value::from("Copy owner-issued controlled ID from plan."),
                        ),
                        ("pattern".into(), Value::from("^h2c-[0-9a-f]{60}$")),
                        ("minLength".into(), Value::from(64_u64)),
                        ("maxLength".into(), Value::from(64_u64)),
                    ])),
                ),
                (
                    "owner_scope".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Owner"))])),
                ),
                (
                    "pins".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Pins"))])),
                ),
                (
                    "resources".into(),
                    Value::Object(Map::from_iter([(
                        "$ref".into(),
                        Value::from("#/$defs/ActiveResources"),
                    )])),
                ),
                (
                    "max_concurrent".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(1_u64)),
                        ("maximum".into(), Value::from(64_u64)),
                    ])),
                ),
                (
                    "elapsed_ms".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(1_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                    ])),
                ),
                (
                    "created_at".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Timestamp"))])),
                ),
                (
                    "expires_at".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Timestamp"))])),
                ),
                (
                    "stages".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("array")),
                        (
                            "items".into(),
                            Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Stage"))])),
                        ),
                        ("minItems".into(), Value::from(1_u64)),
                        ("maxItems".into(), Value::from(64_u64)),
                    ])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("controlled_admission"),
                Value::from("schema_version"),
                Value::from("id"),
                Value::from("owner_scope"),
                Value::from("pins"),
                Value::from("resources"),
                Value::from("max_concurrent"),
                Value::from("elapsed_ms"),
                Value::from("created_at"),
                Value::from("expires_at"),
                Value::from("stages"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Copy the complete server-issued immutable spec, including all pins and MAC. Do not reconstruct it from business inputs. Native controlled admit/run require controlled_admission even though the legacy general Rust DTO permits None.",
            ),
        ),
    ]))
}

pub(super) fn definition_operation() -> Value {
    Value::Object(Map::from_iter([
        (
            "oneOf".into(),
            Value::Array(vec![
                Value::Object(Map::from_iter([
                    ("type".into(), Value::from("object")),
                    (
                        "properties".into(),
                        Value::Object(Map::from_iter([
                            (
                                "operation".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("string")),
                                    ("const".into(), Value::from("proposal")),
                                ])),
                            ),
                            (
                                "patterns".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("array")),
                                    (
                                        "items".into(),
                                        Value::Object(Map::from_iter([(
                                            "$ref".into(),
                                            Value::from("#/$defs/PatternId"),
                                        )])),
                                    ),
                                    ("minItems".into(), Value::from(1_u64)),
                                    ("maxItems".into(), Value::from(8_u64)),
                                ])),
                            ),
                            (
                                "max_input_bytes".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("integer")),
                                    ("minimum".into(), Value::from(1_u64)),
                                    ("maximum".into(), Value::from(262_144_u64)),
                                ])),
                            ),
                            (
                                "max_output_bytes".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("integer")),
                                    ("minimum".into(), Value::from(1_u64)),
                                    ("maximum".into(), Value::from(65536_u64)),
                                ])),
                            ),
                            (
                                "timeout_ms".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("integer")),
                                    ("minimum".into(), Value::from(1_u64)),
                                    ("maximum".into(), Value::from(600_000_u64)),
                                ])),
                            ),
                        ])),
                    ),
                    (
                        "required".into(),
                        Value::Array(vec![
                            Value::from("operation"),
                            Value::from("patterns"),
                            Value::from("max_input_bytes"),
                            Value::from("max_output_bytes"),
                            Value::from("timeout_ms"),
                        ]),
                    ),
                    ("additionalProperties".into(), Value::from(false)),
                ])),
                Value::Object(Map::from_iter([
                    ("type".into(), Value::from("object")),
                    (
                        "properties".into(),
                        Value::Object(Map::from_iter([
                            (
                                "operation".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("string")),
                                    ("const".into(), Value::from("evaluation")),
                                ])),
                            ),
                            (
                                "proposal_stage".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/StageId"))])),
                            ),
                            (
                                "split".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Split"))])),
                            ),
                        ])),
                    ),
                    (
                        "required".into(),
                        Value::Array(vec![
                            Value::from("operation"),
                            Value::from("proposal_stage"),
                            Value::from("split"),
                        ]),
                    ),
                    ("additionalProperties".into(), Value::from(false)),
                ])),
                Value::Object(Map::from_iter([
                    ("type".into(), Value::from("object")),
                    (
                        "properties".into(),
                        Value::Object(Map::from_iter([
                            (
                                "operation".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("string")),
                                    ("const".into(), Value::from("verify")),
                                ])),
                            ),
                            (
                                "evaluation_stage".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/StageId"))])),
                            ),
                        ])),
                    ),
                    (
                        "required".into(),
                        Value::Array(vec![Value::from("operation"), Value::from("evaluation_stage")]),
                    ),
                    ("additionalProperties".into(), Value::from(false)),
                ])),
                Value::Object(Map::from_iter([
                    ("type".into(), Value::from("object")),
                    (
                        "properties".into(),
                        Value::Object(Map::from_iter([
                            (
                                "operation".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("string")),
                                    ("const".into(), Value::from("development_probe")),
                                ])),
                            ),
                            (
                                "proposal_stage".into(),
                                Value::Object(Map::from_iter([(
                                    "anyOf".into(),
                                    Value::Array(vec![
                                        Value::Object(Map::from_iter([(
                                            "$ref".into(),
                                            Value::from("#/$defs/StageId"),
                                        )])),
                                        Value::Object(Map::from_iter([("type".into(), Value::from("null"))])),
                                    ]),
                                )])),
                            ),
                        ])),
                    ),
                    ("required".into(), Value::Array(vec![Value::from("operation")])),
                    ("additionalProperties".into(), Value::from(false)),
                ])),
                Value::Object(Map::from_iter([
                    ("type".into(), Value::from("object")),
                    (
                        "properties".into(),
                        Value::Object(Map::from_iter([
                            (
                                "operation".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("string")),
                                    ("const".into(), Value::from("development_proposal")),
                                ])),
                            ),
                            (
                                "role".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("string")),
                                    (
                                        "enum".into(),
                                        Value::Array(vec![
                                            Value::from("general"),
                                            Value::from("domain"),
                                            Value::from("compose"),
                                        ]),
                                    ),
                                ])),
                            ),
                            (
                                "patterns".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("array")),
                                    (
                                        "items".into(),
                                        Value::Object(Map::from_iter([(
                                            "$ref".into(),
                                            Value::from("#/$defs/PatternId"),
                                        )])),
                                    ),
                                    ("minItems".into(), Value::from(1_u64)),
                                    ("maxItems".into(), Value::from(8_u64)),
                                ])),
                            ),
                            (
                                "probe_stages".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("array")),
                                    (
                                        "items".into(),
                                        Value::Object(Map::from_iter([(
                                            "$ref".into(),
                                            Value::from("#/$defs/StageId"),
                                        )])),
                                    ),
                                ])),
                            ),
                            (
                                "parent_selection".into(),
                                Value::Object(Map::from_iter([(
                                    "anyOf".into(),
                                    Value::Array(vec![
                                        Value::Object(Map::from_iter([(
                                            "$ref".into(),
                                            Value::from("#/$defs/StageId"),
                                        )])),
                                        Value::Object(Map::from_iter([("type".into(), Value::from("null"))])),
                                    ]),
                                )])),
                            ),
                            (
                                "lesson_context_sha256".into(),
                                Value::Object(Map::from_iter([(
                                    "anyOf".into(),
                                    Value::Array(vec![
                                        Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                                        Value::Object(Map::from_iter([("type".into(), Value::from("null"))])),
                                    ]),
                                )])),
                            ),
                            (
                                "max_input_bytes".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("integer")),
                                    ("minimum".into(), Value::from(1_u64)),
                                    ("maximum".into(), Value::from(262_144_u64)),
                                ])),
                            ),
                            (
                                "max_output_bytes".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("integer")),
                                    ("minimum".into(), Value::from(1_u64)),
                                    ("maximum".into(), Value::from(65536_u64)),
                                ])),
                            ),
                            (
                                "timeout_ms".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("integer")),
                                    ("minimum".into(), Value::from(1_u64)),
                                    ("maximum".into(), Value::from(600_000_u64)),
                                ])),
                            ),
                        ])),
                    ),
                    (
                        "required".into(),
                        Value::Array(vec![
                            Value::from("operation"),
                            Value::from("role"),
                            Value::from("patterns"),
                            Value::from("probe_stages"),
                            Value::from("max_input_bytes"),
                            Value::from("max_output_bytes"),
                            Value::from("timeout_ms"),
                        ]),
                    ),
                    ("additionalProperties".into(), Value::from(false)),
                ])),
                Value::Object(Map::from_iter([
                    ("type".into(), Value::from("object")),
                    (
                        "properties".into(),
                        Value::Object(Map::from_iter([
                            (
                                "operation".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("string")),
                                    ("const".into(), Value::from("development_selection")),
                                ])),
                            ),
                            (
                                "probe_stages".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("array")),
                                    (
                                        "items".into(),
                                        Value::Object(Map::from_iter([(
                                            "$ref".into(),
                                            Value::from("#/$defs/StageId"),
                                        )])),
                                    ),
                                ])),
                            ),
                            (
                                "parent_selections".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("array")),
                                    (
                                        "items".into(),
                                        Value::Object(Map::from_iter([(
                                            "$ref".into(),
                                            Value::from("#/$defs/StageId"),
                                        )])),
                                    ),
                                ])),
                            ),
                        ])),
                    ),
                    (
                        "required".into(),
                        Value::Array(vec![Value::from("operation"), Value::from("probe_stages")]),
                    ),
                    ("additionalProperties".into(), Value::from(false)),
                ])),
                Value::Object(Map::from_iter([
                    ("type".into(), Value::from("object")),
                    (
                        "properties".into(),
                        Value::Object(Map::from_iter([
                            (
                                "operation".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("string")),
                                    ("const".into(), Value::from("selected_evaluation")),
                                ])),
                            ),
                            (
                                "selection_stage".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/StageId"))])),
                            ),
                            (
                                "split".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Split"))])),
                            ),
                            (
                                "validation_stage".into(),
                                Value::Object(Map::from_iter([(
                                    "anyOf".into(),
                                    Value::Array(vec![
                                        Value::Object(Map::from_iter([(
                                            "$ref".into(),
                                            Value::from("#/$defs/StageId"),
                                        )])),
                                        Value::Object(Map::from_iter([("type".into(), Value::from("null"))])),
                                    ]),
                                )])),
                            ),
                        ])),
                    ),
                    (
                        "required".into(),
                        Value::Array(vec![
                            Value::from("operation"),
                            Value::from("selection_stage"),
                            Value::from("split"),
                        ]),
                    ),
                    ("additionalProperties".into(), Value::from(false)),
                ])),
                Value::Object(Map::from_iter([
                    ("type".into(), Value::from("object")),
                    (
                        "properties".into(),
                        Value::Object(Map::from_iter([
                            (
                                "operation".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("string")),
                                    ("const".into(), Value::from("gate_review")),
                                ])),
                            ),
                            (
                                "selection_stage".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/StageId"))])),
                            ),
                            (
                                "validation_stage".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/StageId"))])),
                            ),
                            (
                                "validation_verifier".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/StageId"))])),
                            ),
                            (
                                "holdout_stage".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/StageId"))])),
                            ),
                            (
                                "holdout_verifier".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/StageId"))])),
                            ),
                        ])),
                    ),
                    (
                        "required".into(),
                        Value::Array(vec![
                            Value::from("operation"),
                            Value::from("selection_stage"),
                            Value::from("validation_stage"),
                            Value::from("validation_verifier"),
                            Value::from("holdout_stage"),
                            Value::from("holdout_verifier"),
                        ]),
                    ),
                    ("additionalProperties".into(), Value::from(false)),
                ])),
            ]),
        ),
        (
            "description".into(),
            Value::from(
                "Copy only operations returned by plan; operation hashes and dependency graph are frozen. No StudyTrial action or operation is exposed on this model surface.",
            ),
        ),
    ]))
}

pub(super) fn definition_issued_plan() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "spec".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Spec"))])),
                ),
                (
                    "operations".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("object")),
                        (
                            "additionalProperties".into(),
                            Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Operation"))])),
                        ),
                        ("minProperties".into(), Value::from(1_u64)),
                        ("maxProperties".into(), Value::from(64_u64)),
                    ])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![Value::from("spec"), Value::from("operations")]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "The exact complete object returned by lifecycle_experiment_plan. Copy both spec and operations unchanged to admit/run. This is verifiable data, not runtime authority; do not regenerate any nested owner/MAC/pin/source fields.",
            ),
        ),
    ]))
}
