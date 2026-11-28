use serde_json::{Map, Value};

pub(super) fn definition_resources() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "calls".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        ("description".into(), Value::from("Provider call count.")),
                    ])),
                ),
                (
                    "tokens".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        ("description".into(), Value::from("Provider tokens.")),
                    ])),
                ),
                (
                    "micro_usd".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        ("description".into(), Value::from("Integer micro-US dollars.")),
                    ])),
                ),
                (
                    "tool_calls".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        ("description".into(), Value::from("Native tool calls.")),
                    ])),
                ),
                (
                    "output_bytes".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        ("description".into(), Value::from("Captured output bytes.")),
                    ])),
                ),
                (
                    "scratch_bytes".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        ("description".into(), Value::from("Owned scratch/native-write bytes.")),
                    ])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("calls"),
                Value::from("tokens"),
                Value::from("micro_usd"),
                Value::from("tool_calls"),
                Value::from("output_bytes"),
                Value::from("scratch_bytes"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "All six integer units are required. Zero is valid only where the service permits it. Unknown usage retains its original allocation.",
            ),
        ),
    ]))
}

pub(super) fn definition_active_resources() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "calls".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(1_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        ("description".into(), Value::from("Provider call count.")),
                    ])),
                ),
                (
                    "tokens".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(1_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        ("description".into(), Value::from("Provider tokens.")),
                    ])),
                ),
                (
                    "micro_usd".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        ("description".into(), Value::from("Integer micro-US dollars.")),
                    ])),
                ),
                (
                    "tool_calls".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        ("description".into(), Value::from("Native tool calls.")),
                    ])),
                ),
                (
                    "output_bytes".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(1_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        ("description".into(), Value::from("Captured output bytes.")),
                    ])),
                ),
                (
                    "scratch_bytes".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(0_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        ("description".into(), Value::from("Owned scratch/native-write bytes.")),
                    ])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![
                Value::from("calls"),
                Value::from("tokens"),
                Value::from("micro_usd"),
                Value::from("tool_calls"),
                Value::from("output_bytes"),
                Value::from("scratch_bytes"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Calls, tokens and output_bytes must be positive for root/model-stage allocations. Every stage allocation and their declared total must fit the explicitly confirmed root ceilings.",
            ),
        ),
    ]))
}

pub(super) fn definition_lesson_query_intent() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([(
                "task_family".into(),
                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Family"))])),
            )])),
        ),
        ("required".into(), Value::Array(vec![Value::from("task_family")])),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Optional business lookup intent. The owner resolves guidance and compatibility; no caller-supplied evidence snapshot.",
            ),
        ),
    ]))
}

pub(super) fn definition_draft() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "id".into(),
                    Value::Object(Map::from_iter([
                        ("$ref".into(), Value::from("#/$defs/Id")),
                        (
                            "description".into(),
                            Value::from(
                                "New business request ID; plan returns a different owner-derived controlled experiment ID. Reuse the returned plan rather than planning again to refresh time.",
                            ),
                        ),
                    ])),
                ),
                (
                    "skill_id".into(),
                    Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/SkillId"))])),
                ),
                (
                    "patterns".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("array")),
                        (
                            "items".into(),
                            Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/PatternId"))])),
                        ),
                        ("minItems".into(), Value::from(1_u64)),
                        ("maxItems".into(), Value::from(8_u64)),
                    ])),
                ),
                (
                    "schedule".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        (
                            "enum".into(),
                            Value::Array(vec![
                                Value::from("single"),
                                Value::from("sequential_2"),
                                Value::from("parallel_2"),
                            ]),
                        ),
                        ("default".into(), Value::from("single")),
                        (
                            "description".into(),
                            Value::from("One bounded cycle, sequential k2, or parallel k2; not unbounded recursion."),
                        ),
                    ])),
                ),
                (
                    "verification_split".into(),
                    Value::Object(Map::from_iter([(
                        "$ref".into(),
                        Value::from("#/$defs/IndependentSplit"),
                    )])),
                ),
                (
                    "resources".into(),
                    Value::Object(Map::from_iter([(
                        "$ref".into(),
                        Value::from("#/$defs/ActiveResources"),
                    )])),
                ),
                (
                    "proposal_allocation".into(),
                    Value::Object(Map::from_iter([(
                        "$ref".into(),
                        Value::from("#/$defs/ActiveResources"),
                    )])),
                ),
                (
                    "probe_allocation".into(),
                    Value::Object(Map::from_iter([(
                        "$ref".into(),
                        Value::from("#/$defs/ActiveResources"),
                    )])),
                ),
                (
                    "evaluation_allocation".into(),
                    Value::Object(Map::from_iter([(
                        "$ref".into(),
                        Value::from("#/$defs/ActiveResources"),
                    )])),
                ),
                (
                    "gate_allocation".into(),
                    Value::Object(Map::from_iter([(
                        "anyOf".into(),
                        Value::Array(vec![
                            Value::Object(Map::from_iter([
                                ("$ref".into(), Value::from("#/$defs/ActiveResources")),
                                (
                                    "description".into(),
                                    Value::from(
                                        "Required for publication: an explicit private gate allocation after exact validation and holdout verification.",
                                    ),
                                ),
                            ])),
                            Value::Object(Map::from_iter([("type".into(), Value::from("null"))])),
                        ]),
                    )])),
                ),
                (
                    "holdout_allocation".into(),
                    Value::Object(Map::from_iter([(
                        "anyOf".into(),
                        Value::Array(vec![
                            Value::Object(Map::from_iter([
                                ("$ref".into(), Value::from("#/$defs/ActiveResources")),
                                (
                                    "description".into(),
                                    Value::from(
                                        "With a gate, use verification_split=validation and allocate a separate final holdout.",
                                    ),
                                ),
                            ])),
                            Value::Object(Map::from_iter([("type".into(), Value::from("null"))])),
                        ]),
                    )])),
                ),
                (
                    "lesson_query".into(),
                    Value::Object(Map::from_iter([(
                        "anyOf".into(),
                        Value::Array(vec![
                            Value::Object(Map::from_iter([(
                                "$ref".into(),
                                Value::from("#/$defs/LessonQueryIntent"),
                            )])),
                            Value::Object(Map::from_iter([("type".into(), Value::from("null"))])),
                        ]),
                    )])),
                ),
                (
                    "elapsed_ms".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("integer")),
                        ("minimum".into(), Value::from(1_u64)),
                        ("maximum".into(), Value::from(u64::MAX)),
                        (
                            "description".into(),
                            Value::from(
                                "Original immutable root lifetime in milliseconds; must fit the launch ceiling.",
                            ),
                        ),
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
                    "proposal_timeout_ms".into(),
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
                Value::from("id"),
                Value::from("skill_id"),
                Value::from("patterns"),
                Value::from("verification_split"),
                Value::from("resources"),
                Value::from("proposal_allocation"),
                Value::from("probe_allocation"),
                Value::from("evaluation_allocation"),
                Value::from("elapsed_ms"),
                Value::from("max_input_bytes"),
                Value::from("max_output_bytes"),
                Value::from("proposal_timeout_ms"),
            ]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Business intent for plan. Supply IDs and explicit budgets, not workspace/owner/MAC/pins/proof. The compiler assigns max_concurrent and all authoritative pins. For a publishable final candidate, include holdout_allocation and gate_allocation with primary validation. Allocation sum, suite evidence, policy and actual expiry are checked by the owner.",
            ),
        ),
        (
            "allOf".into(),
            Value::Array(vec![
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            ("required".into(), Value::Array(vec![Value::from("holdout_allocation")])),
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "holdout_allocation".into(),
                                    Value::Object(Map::from_iter([("type".into(), Value::from("object"))])),
                                )])),
                            ),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([(
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "verification_split".into(),
                                Value::Object(Map::from_iter([("const".into(), Value::from("validation"))])),
                            )])),
                        )])),
                    ),
                ])),
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            ("required".into(), Value::Array(vec![Value::from("gate_allocation")])),
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "gate_allocation".into(),
                                    Value::Object(Map::from_iter([("type".into(), Value::from("object"))])),
                                )])),
                            ),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([
                            ("required".into(), Value::Array(vec![Value::from("holdout_allocation")])),
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([
                                    (
                                        "verification_split".into(),
                                        Value::Object(Map::from_iter([("const".into(), Value::from("validation"))])),
                                    ),
                                    (
                                        "holdout_allocation".into(),
                                        Value::Object(Map::from_iter([("type".into(), Value::from("object"))])),
                                    ),
                                ])),
                            ),
                        ])),
                    ),
                ])),
            ]),
        ),
    ]))
}
