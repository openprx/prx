#![cfg(test)]

use super::*;

async fn composition_outcome_for_dialect(
    arguments: serde_json::Value,
    dialect: ToolSchemaDialect,
) -> (ToolExecutionOutcome, usize) {
    let calls = Arc::new(AtomicUsize::new(0));
    let service = ToolExecutionService::new(
        vec![Arc::new(CompositionTool {
            calls: Arc::clone(&calls),
        })],
        Arc::new(FixedPolicy {
            decision: ToolExecutionDecision::Allow,
            stages: Arc::new(Mutex::new(Vec::new())),
        }),
        Arc::new(FixedApproval {
            decision: approved(),
            stages: Arc::new(Mutex::new(Vec::new())),
        }),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    );
    let outcome = service
        .execute(
            ToolExecutionCommand::new("composition_tool", arguments),
            context().with_schema_dialect(dialect),
            None,
        )
        .await;
    let observed = calls.load(Ordering::SeqCst);
    (outcome, observed)
}

async fn composition_outcome(arguments: serde_json::Value) -> (ToolExecutionOutcome, usize) {
    composition_outcome_for_dialect(arguments, ToolSchemaDialect::Raw).await
}

#[tokio::test]
async fn production_path_rejects_scalar_composition_violations_before_the_backend_runs() {
    for (arguments, expected_path) in [
        (serde_json::json!({"limit": "invalid"}), "$.limit"),
        (serde_json::json!({"limit": 0}), "$.limit"),
        (serde_json::json!({"window": 25}), "$.window"),
        (serde_json::json!({"window": -1}), "$.window"),
    ] {
        let (outcome, calls) = composition_outcome(arguments.clone()).await;
        assert_eq!(
            outcome.status,
            ToolExecutionStatus::InvalidArguments,
            "{arguments} violates the published composition, got {}",
            outcome.model_content
        );
        assert_eq!(
            calls, 0,
            "a contract violation must not reach the executor: {arguments}"
        );
        assert!(
            outcome.model_content.contains(expected_path),
            "expected {expected_path} in {}",
            outcome.model_content
        );
        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|error| error.contains(expected_path)),
            "the audit record must carry the same issue: {arguments}"
        );
    }
}

#[tokio::test]
async fn production_path_rejects_array_composition_violations_before_the_backend_runs() {
    for (arguments, expected_path) in [
        (serde_json::json!({"tags": "ops"}), "$.tags"),
        (serde_json::json!({"tags": []}), "$.tags"),
        (serde_json::json!({"tags": ["ops", 7]}), "$.tags[1]"),
        (serde_json::json!({"tags": ["x"]}), "$.tags[0]"),
    ] {
        let (outcome, calls) = composition_outcome(arguments.clone()).await;
        assert_eq!(
            outcome.status,
            ToolExecutionStatus::InvalidArguments,
            "{arguments} violates the published composition, got {}",
            outcome.model_content
        );
        assert_eq!(
            calls, 0,
            "a contract violation must not reach the executor: {arguments}"
        );
        assert!(
            outcome.model_content.contains(expected_path),
            "expected {expected_path} in {}",
            outcome.model_content
        );
    }
}

#[tokio::test]
async fn production_path_rejects_object_root_conditional_and_ref_violations_before_backend() {
    for (arguments, expected_path) in [
        (serde_json::json!({"profile": {}}), "$.profile"),
        (serde_json::json!({"profile": {"name": "x"}}), "$.profile.name"),
        (serde_json::json!({"action": "write"}), "$"),
        (serde_json::json!({"target": {}}), "$.target"),
        (serde_json::json!({"target": {"id": "x"}}), "$.target.id"),
    ] {
        let (outcome, calls) = composition_outcome(arguments.clone()).await;
        assert_eq!(
            outcome.status,
            ToolExecutionStatus::InvalidArguments,
            "{arguments} violates the published composition, got {}",
            outcome.model_content
        );
        assert_eq!(
            calls, 0,
            "an object, root-conditional, or local-ref violation must not reach the executor: {arguments}"
        );
        assert!(
            outcome.model_content.contains(expected_path),
            "expected {expected_path} in {}",
            outcome.model_content
        );
    }
}

#[tokio::test]
async fn production_path_accepts_composition_respecting_scalars_and_arrays() {
    for arguments in [
        serde_json::json!({"limit": 5}),
        serde_json::json!({"tags": ["ops", "prod"]}),
        serde_json::json!({"window": 15}),
        serde_json::json!({"window": 5}),
        serde_json::json!({"profile": {"name": "prx"}}),
        serde_json::json!({"action": "write", "content": "ready"}),
        serde_json::json!({"target": {"id": "prx"}}),
        serde_json::json!({"limit": 1, "tags": ["ops"], "window": 20}),
    ] {
        let (outcome, calls) = composition_outcome(arguments.clone()).await;
        assert_eq!(
            outcome.status,
            ToolExecutionStatus::Succeeded,
            "{arguments} satisfies the published composition: {:?}",
            outcome.error
        );
        assert_eq!(
            calls, 1,
            "a contract-respecting call must reach the executor: {arguments}"
        );
    }
}

/// Composition is enforced against the document the provider was actually
/// shown, exactly like every other keyword: `clean_for_gemini` deletes
/// `minimum` and `minLength` from inside the `allOf` members too, so those
/// bounds are not held against a Gemini call, while `type` and `minItems`
/// survive cleaning inside `items` and stay enforced.
#[tokio::test]
async fn composition_is_enforced_per_provider_dialect() {
    let (gemini, gemini_calls) =
        composition_outcome_for_dialect(serde_json::json!({"limit": 0}), ToolSchemaDialect::Gemini).await;
    assert_eq!(
        gemini.status,
        ToolExecutionStatus::Succeeded,
        "clean_for_gemini strips `minimum` inside `allOf`: {}",
        gemini.model_content
    );
    assert_eq!(gemini_calls, 1);

    let (gemini, gemini_calls) =
        composition_outcome_for_dialect(serde_json::json!({"limit": "invalid"}), ToolSchemaDialect::Gemini).await;
    assert_eq!(
        gemini.status,
        ToolExecutionStatus::InvalidArguments,
        "`type` survives clean_for_gemini inside `allOf`: {}",
        gemini.model_content
    );
    assert_eq!(gemini_calls, 0);
    assert!(gemini.model_content.contains("$.limit"), "{}", gemini.model_content);

    let (gemini, gemini_calls) =
        composition_outcome_for_dialect(serde_json::json!({"tags": ["ops", 7]}), ToolSchemaDialect::Gemini).await;
    assert_eq!(
        gemini.status,
        ToolExecutionStatus::InvalidArguments,
        "an item `type` inside `allOf` survives cleaning: {}",
        gemini.model_content
    );
    assert_eq!(gemini_calls, 0);
    assert!(gemini.model_content.contains("$.tags[1]"), "{}", gemini.model_content);
}
