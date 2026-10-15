#![cfg(test)]

use super::super::*;
use crate::chat::action::{Action, ProviderUsageRecordKind};
use crate::chat::turn_scheduler::{TurnPriority, TurnScheduler, TurnTaskId};
use crate::llm::route_decision::TokenUsageSource;
use tokio_util::sync::CancellationToken;

fn s() -> ChatState {
    ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
}

fn task_pair() -> (TurnTaskId, TurnTaskId) {
    let mut scheduler = TurnScheduler::new();
    let a = scheduler.enqueue("usage-a", TurnPriority::Normal, 0);
    let b = scheduler.enqueue("usage-b", TurnPriority::Normal, 0);
    (a, b)
}

fn record(total_tokens: u64) -> MainSessionTokenUsageRecord {
    MainSessionTokenUsageRecord {
        settlement_id: None,
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        prompt_tokens: total_tokens / 2,
        completion_tokens: total_tokens - (total_tokens / 2),
        total_tokens,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        source: TokenUsageSource::Reported,
        cost_usd: None,
    }
}

fn provider_usage(
    state: &mut ChatState,
    task_id: Option<TurnTaskId>,
    usage_kind: ProviderUsageRecordKind,
    total_tokens: u64,
) -> Vec<Effect> {
    state.reduce(Action::ProviderUsageRecorded {
        task_id,
        usage_kind,
        record: record(total_tokens),
    })
}

#[test]
fn p3c_same_task_final_aggregate_is_deduped_once() {
    let mut state = s();
    let (task_id, _) = task_pair();

    let first = provider_usage(&mut state, Some(task_id), ProviderUsageRecordKind::FinalAggregate, 10);
    let second = provider_usage(&mut state, Some(task_id), ProviderUsageRecordKind::FinalAggregate, 20);

    assert_eq!(first.len(), 2);
    assert!(second.is_empty(), "duplicate final aggregate should be a reducer no-op");
    assert_eq!(state.session.token_usage_records.len(), 1);
    assert_eq!(state.ui.token_usage_summary.total_tokens, 10);
}

#[test]
fn p3c_out_of_order_final_usage_stays_on_own_task() {
    let mut state = s();
    let (task_a, task_b) = task_pair();

    let _ = provider_usage(&mut state, Some(task_b), ProviderUsageRecordKind::FinalAggregate, 40);
    let _ = provider_usage(&mut state, Some(task_a), ProviderUsageRecordKind::FinalAggregate, 15);
    let duplicate_b = provider_usage(&mut state, Some(task_b), ProviderUsageRecordKind::FinalAggregate, 99);

    assert!(duplicate_b.is_empty());
    assert_eq!(state.session.token_usage_records.len(), 2);
    let totals = state
        .session
        .token_usage_records
        .iter()
        .map(|record| record.total_tokens)
        .collect::<Vec<_>>();
    assert_eq!(totals, vec![40, 15]);
    assert_eq!(state.ui.token_usage_summary.total_tokens, 55);
    assert!(state.control.final_usage_tasks_recorded.contains(&task_a));
    assert!(state.control.final_usage_tasks_recorded.contains(&task_b));
}

#[test]
fn p3c_incremental_usage_for_same_task_is_not_deduped() {
    let mut state = s();
    let (task_id, _) = task_pair();

    let _ = provider_usage(&mut state, Some(task_id), ProviderUsageRecordKind::Incremental, 7);
    let _ = provider_usage(&mut state, Some(task_id), ProviderUsageRecordKind::Incremental, 11);

    assert_eq!(state.session.token_usage_records.len(), 2);
    assert_eq!(state.ui.token_usage_summary.request_count, 2);
    assert_eq!(state.ui.token_usage_summary.total_tokens, 18);
    assert!(state.control.final_usage_tasks_recorded.is_empty());
}

#[test]
fn p3c_legacy_usage_without_task_id_is_not_deduped() {
    let mut state = s();

    let _ = provider_usage(&mut state, None, ProviderUsageRecordKind::FinalAggregate, 12);
    let _ = provider_usage(&mut state, None, ProviderUsageRecordKind::FinalAggregate, 13);

    assert_eq!(state.session.token_usage_records.len(), 2);
    assert_eq!(state.ui.token_usage_summary.request_count, 2);
    assert_eq!(state.ui.token_usage_summary.total_tokens, 25);
    assert!(state.control.final_usage_tasks_recorded.is_empty());
}
