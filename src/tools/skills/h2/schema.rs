//! Complete native H2 intent contract. Owner-issued fields are echo data.
//! Build typed Values directly: no deep json! expansion or runtime parser.
use serde_json::{Map, Value};

pub(super) fn parameters_schema() -> Value {
    let mut definitions = Map::new();
    definitions.insert("Catalog".into(), definition_catalog());
    definitions.insert("Digest".into(), definition_digest());
    definitions.insert("Id".into(), definition_id());
    definitions.insert("StageId".into(), definition_stage_id());
    definitions.insert("SkillId".into(), definition_skill_id());
    definitions.insert("PatternId".into(), definition_pattern_id());
    definitions.insert("RevisionId".into(), definition_revision_id());
    definitions.insert("Timestamp".into(), definition_timestamp());
    definitions.insert("Owner".into(), definition_owner());
    definitions.insert("Family".into(), definition_family());
    definitions.insert("Split".into(), definition_split());
    definitions.insert("IndependentSplit".into(), definition_independent_split());
    definitions.insert("Resources".into(), definition_resources());
    definitions.insert("ActiveResources".into(), definition_active_resources());
    definitions.insert("LessonQueryIntent".into(), definition_lesson_query_intent());
    definitions.insert("Draft".into(), definition_draft());
    definitions.insert("Handle".into(), definition_handle());
    definitions.insert("Final".into(), definition_final());
    definitions.insert("Publish".into(), definition_publish());
    definitions.insert("Annotation".into(), definition_annotation());
    definitions.insert("Rollback".into(), definition_rollback());
    definitions.insert("LessonRef".into(), definition_lesson_ref());
    definitions.insert("Retirement".into(), definition_retirement());
    definitions.insert("Lessons".into(), definition_lessons());
    definitions.insert("ModelBinding".into(), definition_model_binding());
    definitions.insert("OriginData".into(), definition_origin_data());
    definitions.insert("Admission".into(), definition_admission());
    definitions.insert("LessonCompatibility".into(), definition_lesson_compatibility());
    definitions.insert("LessonLimits".into(), definition_lesson_limits());
    definitions.insert("LessonStats".into(), definition_lesson_stats());
    definitions.insert("LessonSnapshotEntry".into(), definition_lesson_snapshot_entry());
    definitions.insert("LessonSnapshot".into(), definition_lesson_snapshot());
    definitions.insert("Pins".into(), definition_pins());
    definitions.insert("Stage".into(), definition_stage());
    definitions.insert("Spec".into(), definition_spec());
    definitions.insert("Operation".into(), definition_operation());
    definitions.insert("IssuedPlan".into(), definition_issued_plan());
    let mut root = Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([
                (
                    "action".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        (
                            "enum".into(),
                            Value::Array(vec![
                                Value::from("lifecycle_experiment_catalog"),
                                Value::from("lifecycle_experiment_plan"),
                                Value::from("lifecycle_experiment_admit"),
                                Value::from("lifecycle_experiment_run"),
                                Value::from("lifecycle_experiment_inspect"),
                                Value::from("lifecycle_experiment_cancel"),
                                Value::from("lifecycle_experiment_recover"),
                                Value::from("lifecycle_experiment_review"),
                                Value::from("lifecycle_experiment_publish"),
                                Value::from("lifecycle_experiment_annotate"),
                                Value::from("lifecycle_experiment_rollback"),
                                Value::from("lifecycle_experiment_retire"),
                                Value::from("lifecycle_experiment_lessons"),
                            ]),
                        ),
                        (
                            "description".into(),
                            Value::from(
                                "Choose exactly one action. Request DTO mapping: catalog=Catalog; plan=Draft; admit=IssuedPlan; run=IssuedPlan; inspect=Handle; cancel=Handle; recover=Handle; review=Final; publish=Publish; annotate=Annotation; rollback=Rollback; retire=Retirement; lessons=Lessons.",
                            ),
                        ),
                    ])),
                ),
                (
                    "request".into(),
                    Value::Object(Map::from_iter([
                        (
                            "oneOf".into(),
                            Value::Array(vec![
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Catalog"))])),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Draft"))])),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/IssuedPlan"))])),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Handle"))])),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Final"))])),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Publish"))])),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Annotation"))])),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Rollback"))])),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Retirement"))])),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Lessons"))])),
                            ]),
                        ),
                        (
                            "description".into(),
                            Value::from(
                                "Match the DTO named for action. Plan uses business intent; admit/run echo the complete plan result; handles/final selection/lesson refs come from actual prior results. Missing evidence is a reason to inspect or report unavailable, never invent signed data.",
                            ),
                        ),
                    ])),
                ),
            ])),
        ),
        (
            "required".into(),
            Value::Array(vec![Value::from("action"), Value::from("request")]),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(concat!(
                "Controlled instruction lifecycle. Start with catalog using {} to discover real candidate skill/pattern IDs, then plan with selected IDs and explicit user budgets. Catalog is bounded read-only discovery, not eligibility or authority; Plan revalidates candidates. Then admit and run the returned immutable plan. Inspect the actual snapshot. If final_selection exists, review it, publish ",
                "{",
                "expected:final_selection",
                "}",
                ", then annotate the actual committed transaction. Approval is separate from execution: use the original terminal/Console review and explicit resume. Cancel persists a request; recover reconciles original facts; neither proves termination or retries Unknown. Historical inspect/query/replay and fresh authorized maintenance do not resume an old root. Study is local CLI only."
            )),
        ),
        (
            "allOf".into(),
            Value::Array(vec![
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "action".into(),
                                    Value::Object(Map::from_iter([(
                                        "const".into(),
                                        Value::from("lifecycle_experiment_catalog"),
                                    )])),
                                )])),
                            ),
                            ("required".into(), Value::Array(vec![Value::from("action")])),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([(
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "request".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Catalog"))])),
                            )])),
                        )])),
                    ),
                ])),
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "action".into(),
                                    Value::Object(Map::from_iter([(
                                        "const".into(),
                                        Value::from("lifecycle_experiment_plan"),
                                    )])),
                                )])),
                            ),
                            ("required".into(), Value::Array(vec![Value::from("action")])),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([(
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "request".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Draft"))])),
                            )])),
                        )])),
                    ),
                ])),
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "action".into(),
                                    Value::Object(Map::from_iter([(
                                        "const".into(),
                                        Value::from("lifecycle_experiment_admit"),
                                    )])),
                                )])),
                            ),
                            ("required".into(), Value::Array(vec![Value::from("action")])),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([(
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "request".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/IssuedPlan"))])),
                            )])),
                        )])),
                    ),
                ])),
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "action".into(),
                                    Value::Object(Map::from_iter([(
                                        "const".into(),
                                        Value::from("lifecycle_experiment_run"),
                                    )])),
                                )])),
                            ),
                            ("required".into(), Value::Array(vec![Value::from("action")])),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([(
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "request".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/IssuedPlan"))])),
                            )])),
                        )])),
                    ),
                ])),
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "action".into(),
                                    Value::Object(Map::from_iter([(
                                        "const".into(),
                                        Value::from("lifecycle_experiment_inspect"),
                                    )])),
                                )])),
                            ),
                            ("required".into(), Value::Array(vec![Value::from("action")])),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([(
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "request".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Handle"))])),
                            )])),
                        )])),
                    ),
                ])),
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "action".into(),
                                    Value::Object(Map::from_iter([(
                                        "const".into(),
                                        Value::from("lifecycle_experiment_cancel"),
                                    )])),
                                )])),
                            ),
                            ("required".into(), Value::Array(vec![Value::from("action")])),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([(
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "request".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Handle"))])),
                            )])),
                        )])),
                    ),
                ])),
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "action".into(),
                                    Value::Object(Map::from_iter([(
                                        "const".into(),
                                        Value::from("lifecycle_experiment_recover"),
                                    )])),
                                )])),
                            ),
                            ("required".into(), Value::Array(vec![Value::from("action")])),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([(
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "request".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Handle"))])),
                            )])),
                        )])),
                    ),
                ])),
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "action".into(),
                                    Value::Object(Map::from_iter([(
                                        "const".into(),
                                        Value::from("lifecycle_experiment_review"),
                                    )])),
                                )])),
                            ),
                            ("required".into(), Value::Array(vec![Value::from("action")])),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([(
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "request".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Final"))])),
                            )])),
                        )])),
                    ),
                ])),
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "action".into(),
                                    Value::Object(Map::from_iter([(
                                        "const".into(),
                                        Value::from("lifecycle_experiment_publish"),
                                    )])),
                                )])),
                            ),
                            ("required".into(), Value::Array(vec![Value::from("action")])),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([(
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "request".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Publish"))])),
                            )])),
                        )])),
                    ),
                ])),
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "action".into(),
                                    Value::Object(Map::from_iter([(
                                        "const".into(),
                                        Value::from("lifecycle_experiment_annotate"),
                                    )])),
                                )])),
                            ),
                            ("required".into(), Value::Array(vec![Value::from("action")])),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([(
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "request".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Annotation"))])),
                            )])),
                        )])),
                    ),
                ])),
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "action".into(),
                                    Value::Object(Map::from_iter([(
                                        "const".into(),
                                        Value::from("lifecycle_experiment_rollback"),
                                    )])),
                                )])),
                            ),
                            ("required".into(), Value::Array(vec![Value::from("action")])),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([(
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "request".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Rollback"))])),
                            )])),
                        )])),
                    ),
                ])),
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "action".into(),
                                    Value::Object(Map::from_iter([(
                                        "const".into(),
                                        Value::from("lifecycle_experiment_retire"),
                                    )])),
                                )])),
                            ),
                            ("required".into(), Value::Array(vec![Value::from("action")])),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([(
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "request".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Retirement"))])),
                            )])),
                        )])),
                    ),
                ])),
                Value::Object(Map::from_iter([
                    (
                        "if".into(),
                        Value::Object(Map::from_iter([
                            (
                                "properties".into(),
                                Value::Object(Map::from_iter([(
                                    "action".into(),
                                    Value::Object(Map::from_iter([(
                                        "const".into(),
                                        Value::from("lifecycle_experiment_lessons"),
                                    )])),
                                )])),
                            ),
                            ("required".into(), Value::Array(vec![Value::from("action")])),
                        ])),
                    ),
                    (
                        "then".into(),
                        Value::Object(Map::from_iter([(
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "request".into(),
                                Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/Lessons"))])),
                            )])),
                        )])),
                    ),
                ])),
            ]),
        ),
        (
            "examples".into(),
            Value::Array(vec![
                Value::Object(Map::from_iter([
                    ("action".into(), Value::from("lifecycle_experiment_catalog")),
                    ("request".into(), Value::Object(Map::new())),
                ])),
                Value::Object(Map::from_iter([
                    ("action".into(), Value::from("lifecycle_experiment_inspect")),
                    (
                        "request".into(),
                        Value::Object(Map::from_iter([
                            (
                                "experiment_id".into(),
                                Value::from("h2c-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                            ),
                            (
                                "plan_sha256".into(),
                                Value::from("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                            ),
                        ])),
                    ),
                ])),
            ]),
        ),
    ]);
    root.insert("$defs".into(), Value::Object(definitions));
    Value::Object(root)
}

fn definition_catalog() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([(
                "after_skill".into(),
                Value::Object(Map::from_iter([
                    (
                        "anyOf".into(),
                        Value::Array(vec![
                            Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/SkillId"))])),
                            Value::Object(Map::from_iter([("type".into(), Value::from("null"))])),
                        ]),
                    ),
                    (
                        "description".into(),
                        Value::from(
                            "Optional advisory cursor copied from actual catalog next_after_skill; omit or null for the first page.",
                        ),
                    ),
                ])),
            )])),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Bounded candidate discovery: no plan/root/owner or MAC fields. Returns state complete|partial|unavailable, skills[{skill_id,display_name,description,patterns[{pattern_id,statement}]}], next_after_skill, observations, scanned_entries and input_bytes. At most 16 skills/8 patterns each/32KiB output; partial/unavailable is not an empty-store proof. Cursor pages skills, not exhaustive patterns. Plan revalidates every selected ID.",
            ),
        ),
        ("examples".into(), Value::Array(vec![Value::Object(Map::new())])),
    ]))
}

fn definition_digest() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from(
                "Lowercase SHA256 copied from the actual service result; never guess or recompute owner evidence.",
            ),
        ),
        ("pattern".into(), Value::from("^[0-9a-f]{64}$")),
        ("minLength".into(), Value::from(64_u64)),
        ("maxLength".into(), Value::from(64_u64)),
    ]))
}

fn definition_id() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from("Exact record/request ID, not a display label."),
        ),
        ("pattern".into(), Value::from("^[A-Za-z0-9_-]{1,64}$")),
        ("minLength".into(), Value::from(1_u64)),
        ("maxLength".into(), Value::from(64_u64)),
    ]))
}

fn definition_stage_id() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from("Service-issued stage identity; retain unchanged."),
        ),
        ("pattern".into(), Value::from("^[A-Za-z0-9_-]{1,48}$")),
        ("minLength".into(), Value::from(1_u64)),
        ("maxLength".into(), Value::from(48_u64)),
    ]))
}

fn definition_skill_id() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from("Existing managed skill ID chosen in the business request; not its display name."),
        ),
        ("pattern".into(), Value::from("^[a-z0-9][a-z0-9_-]{0,63}$")),
        ("minLength".into(), Value::from(1_u64)),
        ("maxLength".into(), Value::from(64_u64)),
    ]))
}

fn definition_pattern_id() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from(
                "Existing verified evidence pattern ID chosen for this skill; a syntactically valid invented ID is not usable evidence.",
            ),
        ),
        ("pattern".into(), Value::from("^pat-[0-9a-f]{32}$")),
        ("minLength".into(), Value::from(36_u64)),
        ("maxLength".into(), Value::from(36_u64)),
    ]))
}

fn definition_revision_id() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from("Actual published/candidate revision copied from a service observation."),
        ),
        ("pattern".into(), Value::from("^r1-[0-9a-f]{64}$")),
        ("minLength".into(), Value::from(67_u64)),
        ("maxLength".into(), Value::from(67_u64)),
    ]))
}

fn definition_timestamp() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from(
                "RFC3339 UTC timestamp. User-requested expiry must be future and fit the original root; observed timestamps must be copied unchanged.",
            ),
        ),
        ("format".into(), Value::from("date-time")),
    ]))
}

fn definition_owner() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from(
                "Owner-issued data: copy from the plan; never choose a caller owner or treat this string as authority.",
            ),
        ),
        ("minLength".into(), Value::from(1_u64)),
        ("maxLength".into(), Value::from(512_u64)),
    ]))
}

fn definition_family() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from("Same bounded task family used by the actual adopted evidence."),
        ),
        ("minLength".into(), Value::from(1_u64)),
        ("maxLength".into(), Value::from(128_u64)),
    ]))
}

fn definition_split() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "enum".into(),
            Value::Array(vec![
                Value::from("development"),
                Value::from("validation"),
                Value::from("holdout"),
            ]),
        ),
    ]))
}

fn definition_independent_split() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "enum".into(),
            Value::Array(vec![Value::from("validation"), Value::from("holdout")]),
        ),
        (
            "description".into(),
            Value::from(
                "Independent verification cannot use development. Full publication uses validation plus separately allocated holdout and private gate.",
            ),
        ),
    ]))
}

fn definition_resources() -> Value {
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

fn definition_active_resources() -> Value {
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

fn definition_lesson_query_intent() -> Value {
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

fn definition_draft() -> Value {
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

fn definition_handle() -> Value {
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

fn definition_final() -> Value {
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

fn definition_publish() -> Value {
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

fn definition_annotation() -> Value {
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

fn definition_rollback() -> Value {
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

fn definition_lesson_ref() -> Value {
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

fn definition_retirement() -> Value {
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

fn definition_lessons() -> Value {
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

fn definition_model_binding() -> Value {
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

fn definition_origin_data() -> Value {
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

fn definition_admission() -> Value {
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

fn definition_lesson_compatibility() -> Value {
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

fn definition_lesson_limits() -> Value {
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

fn definition_lesson_stats() -> Value {
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

fn definition_lesson_snapshot_entry() -> Value {
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

fn definition_lesson_snapshot() -> Value {
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

fn definition_pins() -> Value {
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

fn definition_stage() -> Value {
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

fn definition_spec() -> Value {
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

fn definition_operation() -> Value {
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

fn definition_issued_plan() -> Value {
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

#[cfg(test)]
mod tests;
