use super::*;
use crate::decision::*;
use observe::{ObserveQueue, SqliteTelemetryWriter, TelemetryWriter};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn causal_adapter_maps_all_ten_questions() {
    let key = CandidateKey::from_parts("read", "file", None);
    let mut answers = BTreeMap::new();
    for q in GlobalQuestion::ALL {
        let answer = match q {
            GlobalQuestion::NextAction | GlobalQuestion::CapabilityTarget => DecisionAnswer::Choice(ChoiceAnswer {
                chosen: OptionId::code("read"),
                probabilities: BTreeMap::from([(OptionId::code("read"), Probability::new(1.0).unwrap())]),
                confidence: Probability::new(0.8).unwrap(),
            }),
            GlobalQuestion::SemanticRisk | GlobalQuestion::ExpectedGain => score(),
            _ => probability(),
        };
        answers.insert(QuestionId::global(q), answer);
    }
    for q in CandidateQuestion::ALL {
        answers.insert(
            QuestionId::candidate(&key, q),
            match q {
                CandidateQuestion::Risk | CandidateQuestion::Gain => score(),
                _ => probability(),
            },
        );
    }
    let engine = causal_adapter::CausalEngine {
        observations: ObserveQueue::disabled(),
    };
    let analysis = engine
        .analyze(IntentInput {
            decision_id: "decision_a",
            answers: &answers,
            candidates: std::slice::from_ref(&key),
        })
        .await
        .unwrap();
    assert_eq!(analysis.decision_id, "decision_a");
    assert_eq!(analysis.global.len(), 6);
    assert_eq!(analysis.candidates[key.as_str()].len(), 4);
    for q in GlobalQuestion::ALL {
        let value = &analysis.global[q.as_str()];
        match q {
            GlobalQuestion::NextAction | GlobalQuestion::CapabilityTarget => assert_eq!(
                value,
                &IntentValue::Choice {
                    probabilities: BTreeMap::from([("read".into(), 1.0)]),
                    confidence: 0.8
                }
            ),
            GlobalQuestion::SemanticRisk | GlobalQuestion::ExpectedGain => assert_eq!(value, &expected_score()),
            _ => assert_eq!(value, &IntentValue::Probability(0.7)),
        }
    }
    for q in CandidateQuestion::ALL {
        assert_eq!(
            analysis.candidates[key.as_str()][q.as_str()],
            match q {
                CandidateQuestion::Risk | CandidateQuestion::Gain => expected_score(),
                _ => IntentValue::Probability(0.7),
            }
        );
    }
    assert_eq!(std::mem::size_of::<noop::NoopEngine>(), 0);
}
fn probability() -> DecisionAnswer {
    DecisionAnswer::Noul(NoulAnswer {
        probability_true: Probability::new(0.7).unwrap(),
    })
}
fn score() -> DecisionAnswer {
    DecisionAnswer::Score(ScoreAnswer {
        distribution: vec![Probability::new(0.25).unwrap(), Probability::new(0.75).unwrap()],
        expected: 0.75,
        legend: vec![],
        confidence: Probability::new(0.9).unwrap(),
    })
}
fn expected_score() -> IntentValue {
    IntentValue::Score {
        distribution: vec![0.25, 0.75],
        expected: 0.75,
        confidence: 0.9,
    }
}
fn outcome(id: &str) -> TurnOutcome {
    TurnOutcome::new(Some(id.into()), vec!["file_read".into()], true).unwrap()
}

#[tokio::test]
async fn observe_queue_full_drops_and_counts_without_blocking() {
    let cancel = CancellationToken::new();
    let (queue, worker) = ObserveQueue::start(1, 10, cancel.clone(), || {
        Ok(Box::new(SqliteTelemetryWriter::open(std::path::Path::new(":memory:"))?))
    })
    .unwrap();
    // No await: the single-thread worker cannot drain the queue yet.
    assert_eq!(queue.observe(outcome("a")), ObserveAdmission::Accepted);
    assert_eq!(queue.observe(outcome("b")), ObserveAdmission::Dropped);
    assert_eq!(queue.counters.dropped.load(Ordering::Relaxed), 1);
    cancel.cancel();
    worker.unwrap().await.unwrap();
    let (disabled, worker) = ObserveQueue::start(1, 0, CancellationToken::new(), || {
        panic!("zero cap must not open a store")
    })
    .unwrap();
    assert!(worker.is_none());
    assert_eq!(disabled.observe(outcome("c")), ObserveAdmission::Disabled);
    assert!(TurnOutcome::new(None, vec!["x".into(); 65], false).is_err());
    assert!(TurnOutcome::new(None, vec!["x".repeat(129)], false).is_err());
    struct FailingWriter;
    impl TelemetryWriter for FailingWriter {
        fn append_fifo(&mut self, _: TurnOutcome, _: usize) -> anyhow::Result<()> {
            anyhow::bail!("test store failure")
        }
    }
    let (queue, worker) = ObserveQueue::start(1, 1, CancellationToken::new(), || Ok(Box::new(FailingWriter))).unwrap();
    let counters = std::sync::Arc::clone(&queue.counters);
    assert_eq!(queue.observe(outcome("error")), ObserveAdmission::Accepted);
    drop(queue);
    worker.unwrap().await.unwrap();
    assert_eq!(counters.store_errors.load(Ordering::Relaxed), 1);
}
#[tokio::test]
async fn observe_evicts_fifo_at_telemetry_cap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("outcomes.sqlite");
    let (queue, worker) = ObserveQueue::start(4, 2, CancellationToken::new(), || {
        Ok(Box::new(SqliteTelemetryWriter::open(&path)?))
    })
    .unwrap();
    for id in ["a", "b", "c"] {
        assert_eq!(queue.observe(outcome(id)), ObserveAdmission::Accepted);
    }
    drop(queue);
    worker.unwrap().await.unwrap();
    let mut store = SqliteTelemetryWriter::open(&path).unwrap();
    assert_eq!(store.outcomes().unwrap(), vec![outcome("b"), outcome("c")]);
    store.append_fifo(outcome("d"), 2).unwrap();
    assert_eq!(store.outcomes().unwrap(), vec![outcome("c"), outcome("d")]);
}
