use super::*;
use crate::runtime::h2_entry::profile::tests::Fixture;
use crate::runtime::turn_source::TurnSourceProof;

pub(in crate::runtime::h2_entry) fn producer(profile: Arc<ControlledH2Profile>) -> TerminalInputProducer {
    TerminalInputProducer {
        profile,
        sequence: AtomicU64::new(0),
    }
}
#[tokio::test]
async fn real_recorded_chat_binds_only_matching_input_and_one_original_turn() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let generation = crate::config::new_shared(fixture.config.clone()).pin();
    let memory: Arc<dyn crate::memory::Memory> =
        Arc::new(crate::memory::SqliteMemory::new(fixture.directory.path()).unwrap());
    let mut producer = producer(fixture.profile.clone());
    let proof = producer.submitted("message", "prx input").unwrap();
    let (source, _) = crate::chat::decision_source::admission_failure_tests::recorded_approval_source(
        &generation,
        memory.clone(),
        "prx input",
        crate::agent::turn_contracts::ChatMode::Edit,
        "prx_first",
        1,
    )
    .await;
    let origin = proof.bind(source.as_ref()).unwrap();
    assert_eq!(origin.record().event, source.source_event_id().unwrap());
    assert_eq!(origin.record().actor, source.owner_id());
    assert_eq!(origin.record().target_owner, fixture.profile.owner.as_str());
    assert_eq!(proof.bind(source.as_ref()).unwrap().record(), origin.record());
    let (other, _) = crate::chat::decision_source::admission_failure_tests::recorded_approval_source(
        &generation,
        memory.clone(),
        "prx input",
        crate::agent::turn_contracts::ChatMode::Edit,
        "prx_second",
        2,
    )
    .await;
    assert!(proof.bind(other.as_ref()).is_err());
    let changed = producer.submitted("changed", "different input").unwrap();
    assert!(changed.bind(source.as_ref()).is_err());
    // A serializable origin is evidence data; loading it installs no RUN or invocation.
    let _: EntryOriginRecord = serde_json::from_value(serde_json::to_value(origin.record()).unwrap()).unwrap();
    assert!(super::super::current_run_continuation().is_none());
    assert!(super::super::current_invocation().is_err());
}
#[test]
fn explicit_resume_normalization_preserves_original_input_and_cannot_change_decision() {
    let fixture = Fixture::new(EntryKind::Terminal);
    let mut producer = producer(fixture.profile.clone());
    let id = uuid::Uuid::new_v4().to_string();
    let command = format!("/resume-decision {id}");
    let proof = producer.submitted("resume-message", &command).unwrap();
    let routed = serde_json::json!({"decision_id":id}).to_string();
    let resumed = proof.resume_request("resume-message", &command, &routed).unwrap();
    assert!(resumed.matches_message("resume-message", &routed));
    assert!(!resumed.matches_message("other-message", &routed));
    let other = serde_json::json!({"decision_id":uuid::Uuid::new_v4().to_string()}).to_string();
    assert!(proof.resume_request("resume-message", &command, &other).is_err());
    assert!(
        producer
            .submitted("plain", "resume this please")
            .unwrap()
            .resume_request("plain", "resume this please", &routed)
            .is_err()
    );
    fixture.profile.revoke();
    assert!(producer.submitted("after", "prx input").is_err());
}
#[tokio::test]
async fn gateway_profile_cannot_adopt_actual_terminal_source_even_with_same_strings() {
    let fixture = Fixture::new(EntryKind::Gateway);
    let generation = crate::config::new_shared(fixture.config.clone()).pin();
    let memory: Arc<dyn crate::memory::Memory> =
        Arc::new(crate::memory::SqliteMemory::new(fixture.directory.path()).unwrap());
    let (source, _) = crate::chat::decision_source::admission_failure_tests::recorded_approval_source(
        &generation,
        memory,
        "prx input",
        crate::agent::turn_contracts::ChatMode::Edit,
        "prx_gateway_not_terminal",
        1,
    )
    .await;
    assert!(fixture.profile.bind_gateway(source.as_ref()).is_err());
}
