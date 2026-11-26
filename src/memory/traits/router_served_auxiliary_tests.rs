use super::RouterLearningEvidence;
use crate::llm::route_decision::{
    AttemptStatus, ExecutionStatus, ProviderAttempt, ProviderExecutionOutcome, TokenUsage,
};

fn outcome(model: &str) -> ProviderExecutionOutcome {
    let now = chrono::Utc::now();
    ProviderExecutionOutcome {
        prepared_parameters: Vec::new(),
        decision_id: "prx-routing-observation".to_owned(),
        started_at: now,
        finished_at: now,
        attempts: vec![ProviderAttempt {
            seq: 1,
            provider: "prx-provider".to_owned(),
            model: model.to_owned(),
            started_at: now,
            finished_at: now,
            status: AttemptStatus::Success,
            error_class: None,
            error_message: None,
        }],
        final_provider: "prx-provider".to_owned(),
        final_model: model.to_owned(),
        status: ExecutionStatus::Success,
        fallback_reason: None,
        tokens_used: TokenUsage::default(),
    }
}

#[test]
fn served_auxiliary_selects_real_serving_tuple_without_replacing_main_usage() {
    let main = outcome("cheap");
    let mut marker = serde_json::json!({ "status": "completed", "provider_outcome": main });
    assert_eq!(
        RouterLearningEvidence::actual_terminal_outcome(&marker).unwrap().1,
        "cheap"
    );
    let original = marker.get("provider_outcome").expect("main outcome").clone();
    marker.as_object_mut().expect("terminal object").insert(
        "served_auxiliary".to_owned(),
        serde_json::json!({ "provider_outcome": outcome("premium") }),
    );
    assert_eq!(
        RouterLearningEvidence::actual_terminal_outcome(&marker).unwrap().1,
        "premium"
    );
    assert_eq!(marker.get("provider_outcome").expect("main outcome"), &original);
    *marker
        .pointer_mut("/served_auxiliary/provider_outcome/final_model")
        .expect("premium final model") = serde_json::json!("foreign");
    assert!(RouterLearningEvidence::actual_terminal_outcome(&marker).is_err());
    *marker
        .get_mut("served_auxiliary")
        .expect("present auxiliary projection") = serde_json::json!({});
    assert!(
        RouterLearningEvidence::actual_terminal_outcome(&marker).is_err(),
        "malformed present projection must not fall back to cheap"
    );
}
