#![cfg(test)]

//! End-to-end fixtures enter through AgentBuilder; no selected grant is fabricated.
use crate::agent::agent::Agent;
use crate::agent::dispatcher::XmlToolDispatcher;
use crate::config::{Config, RouterConfig, RouterModelConfig};
use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, TokenUsage};
use crate::memory::embeddings::LocalHashEmbedding;
use crate::memory::{Memory, RouterHistoryRead, RouterHistoryRow, SqliteMemory};

mod observed_sqlite;
mod paid_embedding;
use crate::providers::traits::ChatTrace;
use crate::providers::{ChatRequest, ChatResponse, Provider};
use crate::router::RouterEngine;
use async_trait::async_trait;
use observed_sqlite::{ObservedRow, ObservedSqlite};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// The selected query-memory read supplies answer evidence; Router history
// remains purpose-bound and never counts as query evidence.
const INPUT: &str = "memory_search: Explain the Rust expression `2 + 2`.";
const MODEL: &str = "prx-local-model";

struct LocalProvider(Arc<AtomicUsize>);
#[async_trait]
impl Provider for LocalProvider {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("fixture requires the traced production dispatch")
    }

    async fn chat_traced(&self, _: ChatRequest<'_>, model: &str, _: f64) -> anyhow::Result<ChatTrace> {
        assert_eq!(model, MODEL);
        self.0.fetch_add(1, Ordering::SeqCst);
        let now = chrono::Utc::now();
        Ok(ChatTrace {
            response: ChatResponse {
                text: Some("The expression adds two integers.".into()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
            attempts: vec![ProviderAttempt {
                seq: 1,
                provider: "ollama".into(),
                model: model.into(),
                started_at: now,
                finished_at: now,
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "ollama".into(),
            final_model: model.into(),
            served_member: None,
            tokens_used: TokenUsage::default(),
        })
    }
}

struct Fixture {
    // Fields drop in declaration order: retain the workspace until the Agent
    // and final observed SQLite owner have released their resources.
    agent: Agent,
    observed: Arc<ObservedSqlite>,
    calls: Arc<AtomicUsize>,
    workspace: tempfile::TempDir,
}
impl Fixture {
    // Construct the boxed production future in a synchronous frame that returns
    // before polling it, so debug temporaries do not inflate the caller's poll.
    fn turn(&mut self) -> std::pin::Pin<Box<impl std::future::Future<Output = anyhow::Result<String>> + '_>> {
        Box::pin(self.agent.turn(INPUT))
    }

    async fn new() -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let observed = Arc::new(ObservedSqlite::new(SqliteMemory::new(workspace.path()).unwrap()));
        let memory: Arc<dyn Memory> = observed.clone();
        let policy = RouterConfig {
            alpha: 1.0,
            knn_k: 1,
            knn_min_records: 1,
            models: vec![RouterModelConfig {
                model_id: MODEL.into(),
                provider: "ollama".into(),
                cost_per_million_tokens: 0.0,
                max_context: 128_000,
                reserved_output_tokens: None,
                latency_ms: 1,
                categories: vec!["conversation".into()],
                elo_rating: 1_000.0,
            }],
            ..RouterConfig::default()
        };
        let router = RouterEngine::new(
            policy.clone(),
            "ollama".into(),
            Vec::new(),
            Arc::clone(&memory),
            None,
            &crate::config::schema::CostConfig::default(),
        )
        .await
        .unwrap()
        .with_local_hash_history(Arc::new(LocalHashEmbedding::new("prx-local-hash-v1", 384)))
        .unwrap();
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.config_path = workspace.path().join("config.toml");
        config.default_model = Some(MODEL.into());
        config.causal_tree.enabled = true;
        config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        config.router = policy;
        // The real Router Retrieve hop is subject to G7, including PMM's
        // NativeTools requirement for this exact controlled model.
        config.providers.models.push(
            toml::from_str(&format!(
                "provider = \"ollama\"\nmodel = \"{MODEL}\"\noperations = [\"chat\"]\nnative_tools = true\n"
            ))
            .unwrap(),
        );
        let resolved = crate::providers::selection::check(
            &config,
            "ollama",
            MODEL,
            &crate::providers::catalog::Requirement::chat().with(crate::providers::catalog::Feature::NativeTools),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )
        .expect("controlled Router model must satisfy the real PMM tool requirement");
        assert_eq!(
            resolved.verification(),
            &crate::providers::catalog::Verification::Verified
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let agent = Agent::builder()
            .runtime_config(config, "ollama")
            .provider(Box::new(LocalProvider(Arc::clone(&calls))))
            .tools(vec![Box::new(crate::tools::memory_search::MemorySearchTool::new(
                workspace.path().to_owned(),
                Arc::clone(&memory),
                true,
            ))])
            .memory(memory)
            .observer(Arc::new(crate::observability::NoopObserver {}))
            .tool_dispatcher(Box::new(XmlToolDispatcher))
            .router(router)
            .build()
            .unwrap();
        agent.seed_scoped_query_memory_for_test(INPUT).await;
        Self {
            agent,
            observed,
            calls,
            workspace,
        }
    }

    fn database(&self) -> Connection {
        Connection::open(self.workspace.path().join("memory/brain.db")).unwrap()
    }

    fn latest_router_receipt(&self) -> Value {
        let raw: String = self.database().query_row(
            "SELECT raw_payload_json FROM message_events WHERE event_type='turn.finalized' ORDER BY id DESC LIMIT 1",
            [], |row| row.get(0),
        ).unwrap();
        let payload: Value = serde_json::from_str(&raw).unwrap();
        let receipt = payload
            .get("terminal_replay_v1")
            .unwrap()
            .get("router_read")
            .unwrap()
            .clone();
        assert!(receipt.is_object(), "terminal must carry the actual Router transition");
        let store = Connection::open(self.workspace.path().join("self/causal_tree/decisions.db")).unwrap();
        let read_id = receipt.get("read_decision_id").and_then(Value::as_str).unwrap();
        let next_id = receipt.get("next_decision_id").and_then(Value::as_str).unwrap();
        assert_ne!(read_id, next_id);
        let (read_source, read_fingerprint): (String, String) = store
            .query_row(
                "SELECT source_event_id,state_fingerprint FROM decisions WHERE decision_id=?1",
                [read_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let (prior, next_source, next_fingerprint): (String, String, String) = store
            .query_row(
                "SELECT prior_decision_id,source_event_id,state_fingerprint FROM decisions WHERE decision_id=?1",
                [next_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(prior, read_id);
        assert_eq!(next_source, read_source);
        assert_eq!(
            receipt.get("read_state_fingerprint").unwrap(),
            &Value::String(read_fingerprint)
        );
        assert_eq!(
            receipt.get("next_state_fingerprint").unwrap(),
            &Value::String(next_fingerprint)
        );
        let source_sequence: i64 = self
            .database()
            .query_row(
                "SELECT id FROM message_events WHERE event_id=?1 AND role='user' AND event_type='message.created'",
                [read_source],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            receipt.get("source_sequence").and_then(Value::as_i64),
            Some(source_sequence)
        );
        receipt
    }

    fn seed_diagnostics(&self) -> Value {
        let observations = self.observed.observations.lock();
        let reads = serde_json::json!({
            "query_reads": observations.query_reads,
            "query_hits": observations.last_query_hit_count,
            "router_reads": observations.read_calls,
            "router_commits": observations.commit_calls,
        });
        drop(observations);
        let store = Connection::open_with_flags(
            self.workspace.path().join("self/causal_tree/decisions.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let decisions = store
            .prepare("SELECT directive_kind,abstention_reason,candidates_json,gates_json,features_json FROM decisions ORDER BY created_at_ms,decision_id LIMIT 8")
            .unwrap()
            .query_map([], |row| {
                Ok(serde_json::json!({
                    "directive": row.get::<_, String>(0)?,
                    "abstention": row.get::<_, Option<String>>(1)?,
                    "candidates": row.get::<_, String>(2)?,
                    "gates": row.get::<_, String>(3)?,
                    "features": row.get::<_, String>(4)?,
                }))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        serde_json::json!({"reads": reads, "decisions": decisions})
    }

    async fn seed(&mut self) -> Value {
        let reply = self.turn().await.unwrap();
        assert_eq!(
            self.calls.load(Ordering::SeqCst),
            1,
            "actual seed reply={reply:?}; observations={}",
            self.seed_diagnostics(),
        );
        let count: i64 = self
            .database()
            .query_row("SELECT count(*) FROM router_history_v1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "positive control must create a real training receipt");
        {
            let observed = self.observed.observations.lock();
            assert_eq!(
                observed.query_reads, 1,
                "query evidence requires its own actual selected read"
            );
            assert_eq!(observed.last_query_hit_count, Some(1));
            assert_eq!(observed.read_calls, 1);
            assert_eq!(observed.last_read.as_ref().unwrap().as_ref().unwrap().count, 0);
            assert_eq!(observed.commit_calls, 1);
            assert_eq!(observed.last_commit.as_ref().unwrap(), &Ok(()));
        }
        let matches: i64 = self
            .database()
            .query_row(
                "SELECT count(*) FROM router_history_v1 h JOIN message_events s ON s.event_id=h.source_event
             JOIN message_events t ON t.event_id=h.terminal WHERE h.source_sequence=s.id AND h.terminal_sequence=t.id
             AND s.id<t.id AND s.run_id=t.run_id AND t.event_type='turn.finalized' AND h.success=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(matches, 1);
        let receipt = self.latest_router_receipt();
        assert_eq!(receipt.get("readmission_succeeded"), Some(&Value::Bool(true)));
        receipt
    }

    fn seeded_row(&self) -> RouterHistoryRow {
        self.database().query_row(
            "SELECT source_sequence,terminal_sequence,terminal,provider,model,success,embedding FROM router_history_v1",
            [], |row| {
                let bytes: Vec<u8> = row.get(6)?;
                assert_eq!(bytes.len(), 384 * 4);
                let embedding = bytes.as_chunks::<4>().0.iter().map(|chunk| {
                    f32::from_le_bytes(*chunk)
                }).collect();
                Ok(RouterHistoryRow {source_sequence: row.get(0)?, terminal_sequence: row.get(1)?,
                    terminal_event_id: row.get(2)?, provider: row.get(3)?, model: row.get(4)?,
                    success: row.get(5)?, embedding})
            },
        ).unwrap()
    }

    async fn assert_read_closed(&mut self, expected_error: &str) {
        // A fixed refusal may be an Ok user-facing response or an error. The
        // durable stage and provider count distinguish it from unrelated errors.
        let _result = self.turn().await;
        assert_eq!(self.calls.load(Ordering::SeqCst), 1);
        {
            let observed = self.observed.observations.lock();
            assert_eq!(observed.read_calls, 2);
            let chain = observed.last_read.as_ref().unwrap().as_ref().unwrap_err();
            assert!(
                chain.iter().any(|cause| cause == expected_error),
                "unexpected actual backend error: {chain:?}"
            );
            assert_eq!(observed.commit_calls, 1, "a refused read cannot train another outcome");
        }
        let receipt = self.latest_router_receipt();
        assert_eq!(receipt.get("readmission_succeeded"), Some(&Value::Bool(false)));
        assert_eq!(receipt.get("failure_stage").and_then(Value::as_str), Some("read"));
        assert_eq!(receipt.get("from_model"), receipt.get("to_model"));
        assert_eq!(receipt.get("pmm_route_binding"), Some(&Value::Null));
    }
}

#[tokio::test]
async fn sqlite_router_actual_two_hops_train_and_read_in_same_agent_session() {
    let mut fixture = Fixture::new().await;
    fixture.seed().await;
    let seed_row = fixture.seeded_row();
    fixture.turn().await.unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    let second = fixture.latest_router_receipt();
    assert_eq!(second.get("readmission_succeeded"), Some(&Value::Bool(true)));
    let source_sequence = second.get("source_sequence").and_then(Value::as_i64).unwrap();
    {
        let observed = fixture.observed.observations.lock();
        assert_eq!(observed.read_calls, 2);
        let read = observed.last_read.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(read.source_sequence, source_sequence);
        assert_eq!(read.count, 1);
        assert_eq!(read.only_row.as_ref(), Some(&ObservedRow::from_row(&seed_row)));
    }
    // The expected rows come from the independent SQL seed receipt, not from
    // the observed result. This checks inclusion and the admitted input binding;
    // it does not claim KNN affects model choice below its ten-record threshold.
    let expected = super::snapshot_digest(&RouterHistoryRead {
        source_sequence,
        rows: vec![seed_row],
    });
    assert_eq!(
        second.get("history_snapshot_sha256"),
        Some(&serde_json::to_value(expected).unwrap())
    );
    let (rows, sessions): (i64, i64) = fixture
        .database()
        .query_row(
            "SELECT count(*),count(DISTINCT session) FROM router_history_v1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((rows, sessions), (2, 1));
}

#[tokio::test]
async fn sqlite_router_nonfinite_vector_closes_actual_read_without_provider_send() {
    let mut fixture = Fixture::new().await;
    fixture.seed().await;
    let mut vector = vec![0_u8; 384 * 4];
    vector.get_mut(..4).unwrap().copy_from_slice(&f32::NAN.to_le_bytes());
    fixture
        .database()
        .execute("UPDATE router_history_v1 SET embedding=?1", [vector])
        .unwrap();
    fixture.assert_read_closed("Router history vector is non-finite").await;
}

#[tokio::test]
async fn sqlite_router_future_source_or_terminal_is_excluded_before_vector_decode() {
    for future_source in [false, true] {
        let mut fixture = Fixture::new().await;
        fixture.seed().await;
        let db = fixture.database();
        // Valid SQL shape, deliberately nonfinite vector: decoding an ineligible
        // row would close the turn. Both sequence gates must precede decoding.
        let mut vector = vec![0_u8; 384 * 4];
        vector.get_mut(..4).unwrap().copy_from_slice(&f32::NAN.to_le_bytes());
        let (source, terminal): (i64, i64) = db
            .query_row(
                "SELECT source_sequence,terminal_sequence FROM router_history_v1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        // Independently exercise the source predicate, even if an externally
        // damaged database no longer satisfies source < terminal. The other
        // case retains the SQL constraint and exercises the terminal predicate.
        if future_source {
            db.execute_batch("PRAGMA ignore_check_constraints=ON").unwrap();
        }
        db.execute(
            "UPDATE router_history_v1 SET source_sequence=?1,terminal_sequence=?2,embedding=?3",
            params![
                if future_source { i64::MAX } else { source },
                if future_source { terminal } else { i64::MAX },
                vector
            ],
        )
        .unwrap();
        drop(db);
        fixture.turn().await.unwrap();
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
        let receipt = fixture.latest_router_receipt();
        assert_eq!(receipt.get("readmission_succeeded"), Some(&Value::Bool(true)));
        let observed = fixture.observed.observations.lock();
        assert_eq!(observed.read_calls, 2);
        let read = observed.last_read.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(read.count, 0, "the ineligible NaN row must not reach the consumer");
        assert_eq!(
            Some(read.source_sequence),
            receipt.get("source_sequence").and_then(Value::as_i64)
        );
    }
}

#[tokio::test]
async fn sqlite_router_1024_eligible_rows_read_but_1025_close_before_dispatch() {
    for count in [1024_i64, 1025] {
        let mut fixture = Fixture::new().await;
        fixture.seed().await;
        let mut db = fixture.database();
        let tx = db.transaction().unwrap();
        // Copy a real, structurally valid row. Only its fixture terminal identity
        // changes; no malformed row may explain the capacity rejection.
        for index in 1..count {
            tx.execute(
                "INSERT INTO router_history_v1 SELECT domain,workspace,owner,session,policy,embedding_model,
                dimensions,embedding_version,source_event,source_sequence,?1,terminal_sequence,provider,model,
                success,embedding,fingerprint FROM router_history_v1 LIMIT 1",
                [format!("prx-fixture-terminal-{index}")],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let actual: i64 = db
            .query_row("SELECT count(*) FROM router_history_v1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(actual, count);
        drop(db);
        if count == 1025 {
            fixture
                .assert_read_closed("Router history complete domain exceeds capacity")
                .await;
        } else {
            // Reading a full domain is legal. Its subsequent training append
            // must fail capacity rather than evict the permanent receipt ledger.
            let result = fixture.turn().await;
            let unexpected = result.is_ok().then(|| {
                // Inspect the actual persisted terminal's Router transition,
                // using the existing linked-decision/source validation helper.
                // Keep this diagnostic out of successful execution and avoid
                // dumping model answers, prompts or complete terminal payloads.
                let receipt = fixture.latest_router_receipt();
                serde_json::json!({
                    "readmission_succeeded": receipt.get("readmission_succeeded"),
                    "failure_stage": receipt.get("failure_stage"),
                    "source_sequence": receipt.get("source_sequence"),
                    "history_snapshot_sha256": receipt.get("history_snapshot_sha256"),
                    "original_budget": receipt.get("original_budget"),
                    "remaining_budget": receipt.get("remaining_budget"),
                })
            });
            assert!(
                result.is_err(),
                "full ledger cannot accept another training receipt; result={result:?}; provider_calls={}; observations={:?}; durable={unexpected:?}",
                fixture.calls.load(Ordering::SeqCst),
                fixture.observed.observations.lock(),
            );
            assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
            {
                let observed = fixture.observed.observations.lock();
                assert_eq!(observed.read_calls, 2);
                assert_eq!(observed.last_read.as_ref().unwrap().as_ref().unwrap().count, 1024);
                assert_eq!(observed.commit_calls, 2);
                let chain = observed.last_commit.as_ref().unwrap().as_ref().unwrap_err();
                assert!(
                    chain
                        .iter()
                        .any(|cause| cause
                            == "Router history complete domain capacity exhausted; receipt ledger retained"),
                    "unexpected actual commit error: {chain:?}"
                );
            }
            assert_eq!(
                fixture.latest_router_receipt().get("readmission_succeeded"),
                Some(&Value::Bool(true))
            );
            let retained: i64 = fixture
                .database()
                .query_row("SELECT count(*) FROM router_history_v1", [], |row| row.get(0))
                .unwrap();
            assert_eq!(retained, 1024);
        }
    }
}

// Independent legacy scalar oracle: each f32 contributes exactly four bytes.
fn scalar_snapshot_digest(read: &RouterHistoryRead) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"router-history-snapshot-v1");
    digest.update(read.source_sequence.to_be_bytes());
    for row in &read.rows {
        digest.update(row.source_sequence.to_be_bytes());
        digest.update(row.terminal_sequence.to_be_bytes());
        for text in [&row.terminal_event_id, &row.provider, &row.model] {
            digest.update((text.len() as u64).to_be_bytes());
            digest.update(text.as_bytes());
        }
        digest.update([u8::from(row.success)]);
        for value in &row.embedding {
            digest.update(value.to_le_bytes());
        }
    }
    digest.finalize().into()
}

fn snapshot_fixture(dimensions: usize, rows: usize) -> RouterHistoryRead {
    let values = [0.0_f32, -0.0_f32, 0.5, -0.5, 1.0, -1.0];
    RouterHistoryRead {
        source_sequence: 4096,
        rows: (0..rows)
            .map(|index| RouterHistoryRow {
                source_sequence: 1,
                terminal_sequence: 2,
                terminal_event_id: format!("prx-terminal-{index}"),
                provider: "ollama".to_owned(),
                model: "prx-local-model".to_owned(),
                success: index % 2 == 0,
                embedding: values.iter().copied().cycle().take(dimensions).collect(),
            })
            .collect(),
    }
}

#[test]
fn snapshot_digest_matches_scalar_v1_at_chunk_and_domain_boundaries() {
    let empty = snapshot_fixture(384, 0);
    assert_eq!(super::snapshot_digest(&empty), scalar_snapshot_digest(&empty));
    for dimensions in [1, 15, 16, 17, 31, 32, 33, 384, 2048] {
        for rows in [1, 3] {
            let read = snapshot_fixture(dimensions, rows);
            assert_eq!(
                super::snapshot_digest(&read),
                scalar_snapshot_digest(&read),
                "dimensions={dimensions}, rows={rows}"
            );
        }
    }
    let full = snapshot_fixture(384, 1024);
    assert_eq!(super::snapshot_digest(&full), scalar_snapshot_digest(&full));
}

#[test]
fn snapshot_digest_retains_every_v1_field_order_and_signed_zero_bit() {
    let original = snapshot_fixture(17, 2);
    let baseline = scalar_snapshot_digest(&original);
    let mutations: [fn(&mut RouterHistoryRead); 11] = [
        |read| read.source_sequence += 1,
        |read| read.rows.first_mut().expect("fixture first row").source_sequence += 1,
        |read| read.rows.first_mut().expect("fixture first row").terminal_sequence += 1,
        |read| {
            read.rows
                .first_mut()
                .expect("fixture first row")
                .terminal_event_id
                .push('x')
        },
        |read| read.rows.first_mut().expect("fixture first row").provider.push('x'),
        |read| read.rows.first_mut().expect("fixture first row").model.push('x'),
        |read| {
            let row = read.rows.first_mut().expect("fixture first row");
            row.success = !row.success;
        },
        |read| {
            *read
                .rows
                .first_mut()
                .expect("fixture first row")
                .embedding
                .first_mut()
                .expect("fixture first embedding value") = -0.0;
        },
        |read| {
            *read
                .rows
                .first_mut()
                .expect("fixture first row")
                .embedding
                .get_mut(16)
                .expect("fixture tail embedding value") = 0.25;
        },
        |read| read.rows.swap(0, 1),
        |read| {
            read.rows.first_mut().expect("fixture first row").embedding.pop();
        },
    ];
    for mutate in mutations {
        let mut changed = snapshot_fixture(17, 2);
        mutate(&mut changed);
        let expected = scalar_snapshot_digest(&changed);
        assert_ne!(expected, baseline, "the scalar v1 stream binds the changed field");
        assert_eq!(super::snapshot_digest(&changed), expected);
    }
}

#[test]
fn snapshot_digest_keeps_v1_bytes_at_batched_float_boundaries() {
    for dimensions in [127, 128, 129, 255, 256, 257, 383, 384, 385, 2048] {
        let read = snapshot_fixture(dimensions, 3);
        assert_eq!(
            super::snapshot_digest(&read),
            scalar_snapshot_digest(&read),
            "dimensions={dimensions}"
        );
    }
}
