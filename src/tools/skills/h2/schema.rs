//! Complete native H2 intent contract. Owner-issued fields are echo data.
//! Build typed Values directly: no deep json! expansion or runtime parser.
use serde_json::{Map, Value};
mod lessons;
mod lifecycle;
mod operation;
mod resources;
mod scalars;
use lessons::{
    definition_admission, definition_lesson_compatibility, definition_lesson_limits, definition_lesson_snapshot,
    definition_lesson_snapshot_entry, definition_lesson_stats, definition_origin_data, definition_pins,
};
use lifecycle::{
    definition_annotation, definition_final, definition_handle, definition_lesson_ref, definition_lessons,
    definition_model_binding, definition_publish, definition_retirement, definition_rollback,
};
use operation::{definition_issued_plan, definition_operation, definition_spec, definition_stage};
use resources::{definition_active_resources, definition_draft, definition_lesson_query_intent, definition_resources};
use scalars::{
    definition_catalog, definition_digest, definition_family, definition_id, definition_independent_split,
    definition_owner, definition_pattern_id, definition_revision_id, definition_skill_id, definition_split,
    definition_stage_id, definition_timestamp,
};

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

#[cfg(test)]
mod tests;
