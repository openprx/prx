use serde_json::{Map, Value};

pub(super) fn definition_origin_data() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "version".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("const".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "authority_expires_at".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Timestamp"))])),
                ),
                (
                    "kind".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        (
                            "enum".into(),
                            Value::Array(vec![Value::from("terminal"), Value::from("gateway")]),
                        ),
                    ])),
                ),
                (
                    "boot".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        ("minLength".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "profile".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "actor".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        ("minLength".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "target_owner".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Owner"))])),
                ),
                (
                    "workspace".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        ("minLength".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "session".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        ("minLength".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "run".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        ("minLength".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "event".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        ("minLength".into(), Value::from(1_u64)),
                    ])),
                ),
                (
                    "input".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "routing".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "sequence".into(),
                    Value::Object(Map::from_iter([(
                        "anyOf".into(),
                        Value::Array(vec![
                            Value::Object(Map::from_iter([
                                ("type".into(), Value::from("integer")),
                                ("minimum".into(), Value::from(0_u64)),
                                ("maximum".into(), Value::from(u64::MAX)),
                            ])),
                            Value::Object(Map::from_iter([("type".into(), Value::from("null"))])),
                        ]),
                    )])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("version"),
                Value::from("authority_expires_at"),
                Value::from("kind"),
                Value::from("boot"),
                Value::from("profile"),
                Value::from("actor"),
                Value::from("target_owner"),
                Value::from("workspace"),
                Value::from("session"),
                Value::from("run"),
                Value::from("event"),
                Value::from("input"),
                Value::from("routing"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Copy plan.spec.controlled_admission.pin.source wholesale. These are owner-authenticated observations; JSON cannot create a producer, origin proof, approval or continuation.",
            ),
        ),
    ]))
}

pub(super) fn definition_admission() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "pin".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("object")),
                        (
                            "properties".into(),
                            Value::Object(Map::from_iter([
                                (
                                    "version".into(),
                                    Value::Object(Map::from_iter([
                                        ("type".into(), Value::from("integer")),
                                        ("const".into(), Value::from(1_u64)),
                                    ])),
                                ),
                                (
                                    "source".into(),
                                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/OriginData"))])),
                                ),
                            ])),
                        ),
                        (
                            "required".into(),
                            Value::Array(vec![Value::from("version"), Value::from("source")]),
                        ),
                        ("additionalProperties".into(), Value::from(false)),
                    ])),
                ),
                (
                    "owner_mac".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![Value::from("pin"), Value::from("owner_mac")]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Copy the exact returned controlled_admission object. Never fill in a MAC, boot, principal, owner or source yourself. Missing/mismatched evidence fails closed; it is not repaired by the tool.",
            ),
        ),
    ]))
}

pub(super) fn definition_lesson_compatibility() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "model_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "policy_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "source_profile_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "tokenizer_profile_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("model_sha256"),
                Value::from("policy_sha256"),
                Value::from("source_profile_sha256"),
                Value::from("tokenizer_profile_sha256"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
    ]))
}

pub(super) fn definition_lesson_limits() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "examined".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u32::MAX)),
                    ])),
                ),
                (
                    "index_bytes".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                    ])),
                ),
                (
                    "record_bytes".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                    ])),
                ),
                (
                    "decoded_bytes".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                    ])),
                ),
                (
                    "lessons".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u32::MAX)),
                    ])),
                ),
                (
                    "item_bytes".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                    ])),
                ),
                (
                    "context_bytes".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                    ])),
                ),
                (
                    "context_token_upper_bound".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                    ])),
                ),
                (
                    "elapsed_ms".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                    ])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("examined"),
                Value::from("index_bytes"),
                Value::from("record_bytes"),
                Value::from("decoded_bytes"),
                Value::from("lessons"),
                Value::from("item_bytes"),
                Value::from("context_bytes"),
                Value::from("context_token_upper_bound"),
                Value::from("elapsed_ms"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
    ]))
}

pub(super) fn definition_lesson_stats() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "examined".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u32::MAX)),
                    ])),
                ),
                (
                    "index_bytes".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                    ])),
                ),
                (
                    "decoded_bytes".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                    ])),
                ),
                (
                    "excluded".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u32::MAX)),
                    ])),
                ),
                (
                    "elapsed_ms".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                    ])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("examined"),
                Value::from("index_bytes"),
                Value::from("decoded_bytes"),
                Value::from("excluded"),
                Value::from("elapsed_ms"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
    ]))
}

pub(super) fn definition_lesson_snapshot_entry() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "reference".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/LessonRef"))])),
                ),
                (
                    "evidence_pin_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "sanitized_guidance".into(),
                    Value::Object(Map::from_iter([("type".into(), Value::from("string"))])),
                ),
                (
                    "guidance_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
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
                Value::from("reference"),
                Value::from("evidence_pin_sha256"),
                Value::from("sanitized_guidance"),
                Value::from("guidance_sha256"),
                Value::from("expires_at"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
    ]))
}

pub(super) fn definition_lesson_snapshot() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "schema_version".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u32::MAX)),
                    ])),
                ),
                (
                    "owner_scope".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Owner"))])),
                ),
                (
                    "task_family".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Family"))])),
                ),
                (
                    "query_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "index_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "compatibility".into(),
                    Value::Object(Map::from_iter([(
                        "$ref".into(),
                        Value::from("#/$defs/LessonCompatibility"),
                    )])),
                ),
                (
                    "exclusions".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("array")),
                        (
                            "items".into(),
                            Value::Object(Map::from_iter([("type".into(), Value::from("string"))])),
                        ),
                    ])),
                ),
                (
                    "limits".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/LessonLimits"))])),
                ),
                (
                    "stats".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/LessonStats"))])),
                ),
                (
                    "entries".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("array")),
                        (
                            "items".into(),
                            Value::Object(Map::from_iter([(
                                "$ref".into(),
                                Value::from("#/$defs/LessonSnapshotEntry"),
                            )])),
                        ),
                        ("maxItems".into(), Value::from(2_u64)),
                    ])),
                ),
                (
                    "formatter_version".into(),
                    Value::Object(Map::from_iter([("type".into(), Value::from("string"))])),
                ),
                (
                    "planned_at".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Timestamp"))])),
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
                Value::from("schema_version"),
                Value::from("owner_scope"),
                Value::from("task_family"),
                Value::from("query_sha256"),
                Value::from("index_sha256"),
                Value::from("compatibility"),
                Value::from("exclusions"),
                Value::from("limits"),
                Value::from("stats"),
                Value::from("entries"),
                Value::from("formatter_version"),
                Value::from("planned_at"),
                Value::from("expires_at"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Optional owner-resolved guidance snapshot copied from the returned plan only. Never supply your own sanitized guidance or compatibility claims.",
            ),
        ),
    ]))
}

pub(super) fn definition_pins() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "skill_id".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/SkillId"))])),
                ),
                (
                    "baseline_revision".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/RevisionId"))])),
                ),
                (
                    "baseline_record_digest".into(),
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
                    "binary_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "config_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "policy_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "catalog_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "environment_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "edit_scope_sha256".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "model".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/ModelBinding"))])),
                ),
                (
                    "suite_digest".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "development_split_digest".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "verification_split".into(),
                    Value::Object(Map::from_iter([(
                        "$ref".into(),
                        Value::from("#/$defs/IndependentSplit"),
                    )])),
                ),
                (
                    "verification_split_digest".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                ),
                (
                    "holdout_split_digest".into(),
                    Value::Object(Map::from_iter([(
                        "anyOf".into(),
                        Value::Array(vec![
                            Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Digest"))])),
                            Value::Object(Map::from_iter([("type".into(), Value::from("null"))])),
                        ]),
                    )])),
                ),
                (
                    "lesson_snapshot".into(),
                    Value::Object(Map::from_iter([(
                        "anyOf".into(),
                        Value::Array(vec![
                            Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/LessonSnapshot"))])),
                            Value::Object(Map::from_iter([("type".into(), Value::from("null"))])),
                        ]),
                    )])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("skill_id"),
                Value::from("baseline_revision"),
                Value::from("baseline_record_digest"),
                Value::from("baseline_generation"),
                Value::from("binary_sha256"),
                Value::from("config_sha256"),
                Value::from("policy_sha256"),
                Value::from("catalog_sha256"),
                Value::from("environment_sha256"),
                Value::from("edit_scope_sha256"),
                Value::from("model"),
                Value::from("suite_digest"),
                Value::from("development_split_digest"),
                Value::from("verification_split"),
                Value::from("verification_split_digest"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Copy exact plan.spec.pins from plan. The runtime resolves these from the real target; caller labels/digests cannot authorize a store/model/suite.",
            ),
        ),
    ]))
}
